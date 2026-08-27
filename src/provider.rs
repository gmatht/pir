use crate::config::ApiKind;
use crate::plugin::ToolSpec;
use crate::types::{Block, Message, Role, Usage};
use serde_json::{json, Map, Value};
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How many times to retry a failed request (the first attempt is not a retry,
/// so this is the number of *additional* attempts). Network blips, DNS hiccups,
/// and transient 5xx / 429 responses from the provider are retried; hard errors
/// (e.g. 401/unauthorized, malformed URL) are not.
const MAX_RETRIES: usize = 4;

/// Per-attempt network timeouts (applied to every request via the ureq agent).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Initial read timeout for an attempt. This covers the **status-line read**:
/// after TCP connect, the client waits up to this long for the server's first
/// response byte (`HTTP/1.1 ... \r\n`). It's deliberately *generous* — a slow
/// or "thinking" provider (e.g. opencode.ai's zen endpoint) can take many
/// seconds before it sends anything, and ureq returns the transport error
/// "Error encountered in the status line: timed out reading response" if the
/// status line isn't received in time. A too-short read timeout made every
/// retry fail identically (all 4 attempts timing out at 2s), which is exactly
/// the failure we saw. It doubles on each retry (see `READ_TIMEOUT_GROWTH`).
const READ_TIMEOUT_INIT: Duration = Duration::from_secs(30);
/// Each retry gets this multiple of the previous attempt's read timeout. Capped
/// at `READ_TIMEOUT_MAX` so a long run of retries can't blow up to minutes.
const READ_TIMEOUT_GROWTH: u32 = 2;
const READ_TIMEOUT_MAX: Duration = Duration::from_secs(240);
/// Hard backstop for the *streaming* phase: if no bytes arrive for this long
/// the connection is treated as stalled and the request fails (rather than the
/// parser polling forever). This guards the gap *between* SSE events once
/// streaming has started — the watchdog is checked before each read, so a
/// connection that goes silent mid-stream is torn down instead of waiting for
/// EOF.
///
/// Note: `ureq` applies a single `timeout_read` to the whole connection, so a
/// read only unblocks at that boundary. The watchdog therefore fires at the next
/// read timeout, which is bounded by the per-attempt `READ_TIMEOUT_*` values —
/// not instantly at `STALL_TIMEOUT`. `STALL_TIMEOUT` bounds the worst case and
/// is overridable via `PIR_STALL_TIMEOUT_SECS` (e.g. set it low in tests, or
/// raise it for very slow providers). The const below is the default when unset.
const STALL_TIMEOUT: Duration = Duration::from_secs(180);

/// Resolve the streaming stall timeout, honouring `PIR_STALL_TIMEOUT_SECS`.
fn stall_timeout() -> Duration {
    std::env::var("PIR_STALL_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .unwrap_or(STALL_TIMEOUT)
}

/// Initial backoff between retries; it doubles each attempt (capped), giving
/// 1s, 2s, 4s, 8s for the four retries above.
const RETRY_BASE_BACKOFF: Duration = Duration::from_secs(1);
const RETRY_MAX_BACKOFF: Duration = Duration::from_secs(10);

pub struct Client {
    kind: ApiKind,
    base_url: String,
    api_key: String,
    /// Shared cancellation flag. When set (e.g. by the REPL on Ctrl-C/Ctrl-D),
    /// an in-flight streaming response aborts at its next poll boundary instead
    /// of blocking until the whole model reply is received.
    cancel: Arc<AtomicBool>,
}

impl Client {
    /// Build a ureq agent with the given per-attempt read timeout. Kept as a
    /// free fn so the retry loop can rebuild the agent with a larger timeout
    /// each attempt without cloning the whole `Client`.
    fn http_agent(read_timeout: Duration) -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(read_timeout)
            .timeout_write(WRITE_TIMEOUT)
            .build()
    }

    pub fn new(kind: ApiKind, base_url: &str, api_key: String) -> Self {
        Client {
            kind,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Point the client at the running turn's cancellation flag. The REPL sets
    /// that flag on Ctrl-C/Ctrl-D; an in-flight stream checks it between reads
    /// and aborts promptly. Passing the agent's own `Arc` lets either side flip
    /// it.
    pub fn set_cancel(&mut self, cancel: Arc<AtomicBool>) {
        self.cancel = cancel;
    }

    pub fn chat(
        &self,
        model: &str,
        max_tokens: u64,
        system: &str,
        history: &[Message],
        tools: &[ToolSpec],
        on_text: &mut dyn FnMut(&str),
    ) -> Result<(Message, Usage), String> {
        let (url, body) = match self.kind {
            ApiKind::Anthropic => self.anthropic_request(model, max_tokens, system, history, tools),
            ApiKind::OpenAi => self.openai_request(model, max_tokens, system, history, tools),
        };
        // Retry the whole request (connect + stream parse) on transient errors.
        // `emitted_text` is set by the stream parsers the moment any token is
        // delivered, so that — once the user is seeing streaming output — we
        // NEVER re-run the attempt (and risk duplicating already-printed text);
        // a mid-stream failure is surfaced as a hard error instead. A cancel
        // request aborts the whole loop immediately (no retry) via `self.cancel`.
        let cancel = self.cancel.clone();
        let mut emitted_text = false;
        for attempt in 0..=MAX_RETRIES {
            if cancel.load(Ordering::SeqCst) {
                return Err("request cancelled".to_string());
            }
            // Read timeout for this attempt: start generous and double each
            // retry (capped). The *first* attempt already waits up to
            // READ_TIMEOUT_INIT for the status line, so a slow/"thinking"
            // provider has room to respond instead of failing instantly; later
            // attempts get proportionally more time. Rebuild the ureq agent so
            // the new timeout takes effect (it's baked in at build time).
            let read_timeout = (READ_TIMEOUT_INIT
                * READ_TIMEOUT_GROWTH.saturating_pow(attempt as u32))
                .min(READ_TIMEOUT_MAX);
            let http = Self::http_agent(read_timeout);
            let mut req = http.post(&url);
            req = match self.kind {
                ApiKind::Anthropic => req
                    .set("x-api-key", &self.api_key)
                    .set("anthropic-version", "2023-06-01"),
                ApiKind::OpenAi => req.set("Authorization", &format!("Bearer {}", self.api_key)),
            };
            // `send_json` returns a `ureq::Error`; map it to our `String` error
            // before streaming so the two parsers share one error type.
            let result: Result<(Message, Usage), String> = req
                .send_json(&body)
                .map_err(http_error)
                .and_then(|resp| match self.kind {
                    ApiKind::Anthropic => stream_anthropic(
                        resp.into_reader(),
                        on_text,
                        &mut emitted_text,
                        &cancel,
                    ),
                    ApiKind::OpenAi => stream_openai(
                        resp.into_reader(),
                        on_text,
                        &mut emitted_text,
                        &cancel,
                    ),
                });
            match result {
                Ok(r) => return Ok(r),
                Err(e) => {
                    if e == "request cancelled" {
                        return Err(e);
                    }
                    // A stalled stream is terminal, not transient: the peer went
                    // silent mid-stream, so re-issuing the request won't make it
                    // resume. (Without this, the retry loop would re-send up to
                    // MAX_RETRIES times, each waiting a full read timeout before
                    // the stall watchdog fired again — far longer than the stall
                    // bound.) Cancellation is likewise fatal (handled above).
                    if e.contains("stalled") {
                        return Err(e);
                    }
                    if attempt >= MAX_RETRIES || !is_retryable(&e) || emitted_text {
                        return Err(e);
                    }
                    let backoff = (RETRY_BASE_BACKOFF * 2u32.pow(attempt as u32)).min(RETRY_MAX_BACKOFF);
                    let _ = on_text(&format!(
                        "\n\u{26a0} request failed (attempt {}), retrying in {:.0?}: {}\n",
                        attempt + 1, backoff, e
                    ));
                    // Sleep in short slices so a cancel mid-backoff is honoured.
                    let mut waited = Duration::ZERO;
                    while waited < backoff {
                        if cancel.load(Ordering::SeqCst) {
                            return Err("request cancelled".to_string());
                        }
                        let step = Duration::from_millis(100).min(backoff - waited);
                        std::thread::sleep(step);
                        waited += step;
                    }
                }
            }
        }
        unreachable!()
    }

    fn anthropic_request(
        &self,
        model: &str,
        max_tokens: u64,
        system: &str,
        history: &[Message],
        tools: &[ToolSpec],
    ) -> (String, Value) {
        (
            format!("{}/messages", self.base_url),
            json!({
                "model": model,
                "max_tokens": max_tokens,
                "stream": true,
                "system": system,
                "messages": history.iter().filter(|m| !m.is_empty())
                    .map(anthropic_message).collect::<Vec<_>>(),
                "tools": tools.iter().map(|t| json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.schema,
                })).collect::<Vec<_>>(),
            }),
        )
    }

    fn openai_request(
        &self,
        model: &str,
        max_tokens: u64,
        system: &str,
        history: &[Message],
        tools: &[ToolSpec],
    ) -> (String, Value) {
        let mut messages = vec![json!({ "role": "system", "content": system })];
        for m in history.iter().filter(|m| !m.is_empty()) {
            messages.extend(openai_message(m));
        }
        let max_key = if model.starts_with("o1")
            || model.starts_with("o3")
            || model.starts_with("o4")
            || model.contains("gpt-5")
        {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        let mut body = Map::new();
        body.insert("model".into(), json!(model));
        body.insert(max_key.into(), json!(max_tokens));
        body.insert("stream".into(), json!(true));
        body.insert("stream_options".into(), json!({ "include_usage": true }));
        body.insert("messages".into(), Value::Array(messages));
        body.insert(
            "tools".into(),
            Value::Array(tools.iter().map(|t| json!({
                "type": "function",
                "function": { "name": t.name, "description": t.description, "parameters": t.schema },
            })).collect()),
        );
        (format!("{}/chat/completions", self.base_url), Value::Object(body))
    }
}

/// Decide whether an error is worth retrying. Retry on transport-level
/// failures (DNS, connection refused, TLS, timeouts, I/O) and on transient
/// HTTP status codes (429 rate-limit, 500/502/503/504 server errors). Do
/// NOT retry on 4xx client errors other than 429 (e.g. 401 unauthorized,
/// 400 bad request) — those won't succeed on replay.
fn is_retryable(error: &str) -> bool {
    // A stalled stream (peer went silent mid-stream) or a cancellation is not a
    // transient failure worth replaying — retry would just re-block on the same
    // dead connection. These are handled as fatal by the caller; refuse them
    // here too so `is_retryable` stays the single source of truth.
    if error.contains("stalled") || error == "request cancelled" {
        return false;
    }
    if error.starts_with("HTTP 429")
        || error.starts_with("HTTP 500")
        || error.starts_with("HTTP 502")
        || error.starts_with("HTTP 503")
        || error.starts_with("HTTP 504")
    {
        return true;
    }
    // Transport-layer (non-HTTP) failures: ureq reports these as bare
    // messages; they are retryable connection/timeout/IO problems.
    if error.starts_with("HTTP ") {
        return false; // a 4xx/other 5xx we didn't explicitly allow
    }
    true
}

fn http_error(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            let mut body = String::new();
            let _ = resp.into_reader().take(8192).read_to_string(&mut body);
            let detail = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| {
                    v.pointer("/error/message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| v.get("message").and_then(Value::as_str).map(str::to_string))
                })
                .unwrap_or_else(|| body.trim().to_string());
            format!("HTTP {code}: {detail}")
        }
        other => other.to_string(),
    }
}

/// True when a read error is a *timeout* poll wake-up rather than a fatal
/// failure. On Linux a socket `SO_RCVTIMEO` surfaces as `WouldBlock`
/// (EAGAIN), not `TimedOut`, so we must accept both — otherwise a silently
/// stalled stream would return a transport error and be swallowed by the
/// retry loop instead of tripping the stall watchdog / cancel check.
fn is_read_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock)
}
fn anthropic_message(m: &Message) -> Value {
    let role = if m.role == Role::User { "user" } else { "assistant" };
    let blocks: Vec<Value> = m
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Text(t) if !t.trim().is_empty() => Some(json!({ "type": "text", "text": t })),
            Block::ToolUse { id, name, input } => Some(
                json!({ "type": "tool_use", "id": id, "name": name, "input": input }),
            ),
            Block::ToolResult { tool_use_id, content, is_error } => Some(json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
                "is_error": is_error,
            })),
            _ => None,
        })
        .collect();
    json!({ "role": role, "content": blocks })
}

fn openai_message(m: &Message) -> Vec<Value> {
    let mut out = Vec::new();
    match m.role {
        Role::User => {
            let text = m.text().trim().to_string();
            if !text.is_empty() {
                out.push(json!({ "role": "user", "content": text }));
            }
            for b in &m.blocks {
                if let Block::ToolResult { tool_use_id, content, .. } = b {
                    out.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_use_id,
                        "content": content,
                    }));
                }
            }
        }
        Role::Assistant => {
            let text = m.text().trim().to_string();
            let calls: Vec<Value> = m
                .blocks
                .iter()
                .filter_map(|b| match b {
                    Block::ToolUse { id, name, input } => Some(json!({
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": input.to_string() },
                    })),
                    _ => None,
                })
                .collect();
            if !calls.is_empty() {
                let content = if text.is_empty() { Value::Null } else { Value::String(text) };
                out.push(json!({ "role": "assistant", "content": content, "tool_calls": calls }));
            } else if !text.is_empty() {
                out.push(json!({ "role": "assistant", "content": text }));
            }
        }
    }
    out
}

fn stream_anthropic<R: Read>(
    r: R,
    on_text: &mut dyn FnMut(&str),
    emitted_text: &mut bool,
    cancel: &Arc<AtomicBool>,
) -> Result<(Message, Usage), String> {
    let mut reader = BufReader::new(r);
    let mut blocks: Vec<Block> = Vec::new();
    let mut usage = Usage::default();
    let mut text = String::new();
    let mut tool: Option<(String, String, String)> = None; // (id, name, partial input json)

    let mut line = String::new();
    let mut last_byte = Instant::now();
    loop {
        // Check cancellation and the stall watchdog before each read. The short
        // per-read ureq timeout makes each `read_line` wake within a couple of
        // seconds, so a Ctrl-C/Ctrl-D is honoured promptly rather than blocking
        // until the whole response arrives; `STALL_TIMEOUT` is the backstop for
        // a connection that goes silent mid-stream.
        if cancel.load(Ordering::SeqCst) {
            return Err("request cancelled".to_string());
        }
        if last_byte.elapsed() > stall_timeout() {
            return Err("stream: stalled (no data for 180s)".to_string());
        }
        line.clear();
        let n = match reader.read_line(&mut line) {
            Ok(n) => n,
            Err(e) if is_read_timeout(&e) => {
                // No bytes yet within the poll window: loop back to re-check
                // cancellation / stall without consuming any input; on Linux the
                // socket timeout surfaces as WouldBlock, not TimedOut.
                continue;
            }
            Err(e) => return Err(format!("stream: {e}")),
        };
        if n == 0 {
            // EOF (or a timed-out read that returned nothing): if we've been
            // idle too long it's a stall; otherwise the stream ended.
            if last_byte.elapsed() > stall_timeout() {
                return Err("stream: stalled (no data for 180s)".to_string());
            }
            break;
        }
        last_byte = Instant::now();
        let Some(data) = line.trim_end().strip_prefix("data:") else { continue };
        let data = data.trim();
        if data == "[DONE]" { break; }
        let v: Value = match serde_json::from_str(data) { Ok(v) => v, Err(_) => continue };

        match v["type"].as_str().unwrap_or("") {
            "message_start" => {
                usage.input = v["message"]["usage"]["input_tokens"].as_u64().unwrap_or(0);
            }
            "content_block_start" => {
                let b = &v["content_block"];
                if b["type"].as_str() == Some("tool_use") {
                    tool = Some((
                        b["id"].as_str().unwrap_or_default().to_string(),
                        b["name"].as_str().unwrap_or_default().to_string(),
                        String::new(),
                    ));
                }
            }
            "content_block_delta" => {
                let d = &v["delta"];
                match d["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        let t = d["text"].as_str().unwrap_or("");
                        if !t.is_empty() {
                            *emitted_text = true;
                        }
                        on_text(t);
                        text.push_str(t);
                    }
                    "input_json_delta" => {
                        if let Some((_, _, buf)) = tool.as_mut() {
                            buf.push_str(d["partial_json"].as_str().unwrap_or(""));
                        }
                    }
                    _ => {} // thinking_delta etc.
                }
            }
            "content_block_stop" => {
                if let Some((id, name, buf)) = tool.take() {
                    let input: Value = serde_json::from_str(&buf).unwrap_or_else(|_| json!({}));
                    blocks.push(Block::ToolUse { id, name, input });
                } else if !text.trim().is_empty() {
                    blocks.push(Block::Text(std::mem::take(&mut text)));
                } else {
                    text.clear();
                }
            }
            "message_delta" => {
                if let Some(o) = v["usage"]["output_tokens"].as_u64() {
                    usage.output = o;
                }
            }
            "message_stop" => break,
            "error" => {
                let msg = v["error"]["message"].as_str().unwrap_or("unknown API error");
                return Err(msg.to_string());
            }
            _ => {}
        }
    }
    // flush dangling state if the stream was cut early
    if let Some((id, name, buf)) = tool.take() {
        let input: Value = serde_json::from_str(&buf).unwrap_or_else(|_| json!({}));
        blocks.push(Block::ToolUse { id, name, input });
    }
    if !text.trim().is_empty() {
        blocks.push(Block::Text(text));
    }
    if blocks.is_empty() {
        blocks.push(Block::Text("(empty response)".into()));
    }
    Ok((Message { role: Role::Assistant, blocks }, usage))
}

fn stream_openai<R: Read>(
    r: R,
    on_text: &mut dyn FnMut(&str),
    emitted_text: &mut bool,
    cancel: &Arc<AtomicBool>,
) -> Result<(Message, Usage), String> {
    let mut reader = BufReader::new(r);
    let mut usage = Usage::default();
    let mut text = String::new();
    let mut calls: Vec<(u64, String, String, String)> = Vec::new(); // (index, id, name, args)

    let mut line = String::new();
    let mut last_byte = Instant::now();
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err("request cancelled".to_string());
        }
        if last_byte.elapsed() > stall_timeout() {
            return Err("stream: stalled (no data for 180s)".to_string());
        }
        line.clear();
        let n = match reader.read_line(&mut line) {
            Ok(n) => n,
            Err(e) if is_read_timeout(&e) => continue,
            Err(e) => return Err(format!("stream: {e}")),
        };
        if n == 0 {
            if last_byte.elapsed() > stall_timeout() {
                return Err("stream: stalled (no data for 180s)".to_string());
            }
            break;
        }
        last_byte = Instant::now();
        let Some(data) = line.trim_end().strip_prefix("data:") else { continue };
        let data = data.trim();
        if data == "[DONE]" { break; }
        let v: Value = match serde_json::from_str(data) { Ok(v) => v, Err(_) => continue };

        if let Some(u) = v.get("usage") {
            if !u.is_null() {
                if let Some(p) = u["prompt_tokens"].as_u64() { usage.input = p; }
                if let Some(c) = u["completion_tokens"].as_u64() { usage.output = c; }
            }
        }
        let Some(choice) = v["choices"].get(0) else { continue };
        let delta = &choice["delta"];
        if let Some(t) = delta["content"].as_str() {
            if !t.is_empty() {
                *emitted_text = true;
                on_text(t);
                text.push_str(t);
            }
        }
        if let Some(tcs) = delta["tool_calls"].as_array() {
            for tc in tcs {
                let idx = tc["index"].as_u64().unwrap_or(0);
                if !calls.iter().any(|(i, _, _, _)| *i == idx) {
                    calls.push((idx, String::new(), String::new(), String::new()));
                }
                let slot = calls.iter_mut().find(|(i, _, _, _)| *i == idx).unwrap();
                if let Some(id) = tc["id"].as_str() {
                    if !id.is_empty() { slot.1 = id.to_string(); }
                }
                if let Some(name) = tc["function"]["name"].as_str() {
                    if !name.is_empty() { slot.2 = name.to_string(); }
                }
                if let Some(args) = tc["function"]["arguments"].as_str() {
                    slot.3.push_str(args);
                }
            }
        }
    }
    let mut blocks: Vec<Block> = Vec::new();
    if !text.trim().is_empty() {
        blocks.push(Block::Text(text));
    }
    for (n, (_, id, name, args)) in calls.into_iter().enumerate() {
        let id = if id.is_empty() { format!("call-{n}") } else { id };
        let name = if name.is_empty() { "unknown_tool".to_string() } else { name };
        let input: Value = serde_json::from_str(&args).unwrap_or_else(|_| json!({}));
        blocks.push(Block::ToolUse { id, name, input });
    }
    if blocks.is_empty() {
        blocks.push(Block::Text("(empty response)".into()));
    }
    Ok((Message { role: Role::Assistant, blocks }, usage))
}


#[cfg(test)]
mod tests {
    use super::*;

    /// A `Read` that yields `data` once, then reports a *timeout* on every
    /// subsequent read — simulating a server that connected, sent its preamble,
    /// then went silent (the "model thinking forever" / stalled-stream case).
    /// This lets us exercise the parser's cancel/stall logic deterministically
    /// without a live socket (which is flaky in CI/sandboxes): the parser must
    /// detect cancellation / a stall at its pre-read check rather than waiting
    /// for EOF. On Linux the timeout surfaces as `WouldBlock`, which is why the
    /// parser accepts both `TimedOut` and `WouldBlock`.
    struct BlockAfter {
        sent: bool,
        data: &'static [u8],
    }
    impl std::io::Read for BlockAfter {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.sent {
                self.sent = true;
                let n = self.data.len().min(buf.len());
                buf[..n].copy_from_slice(&self.data[..n]);
                return Ok(n);
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "simulated timeout",
            ))
        }
    }

    #[test]
    fn cancel_aborts_inflight_stream() {
        // Cancel flag already set: the parser must bail at its next pre-read
        // check instead of polling forever on the silent socket (proving the
        // cancel path is honoured at a read boundary, not just via EOF).
        let cancel = Arc::new(AtomicBool::new(true));
        let mut reader = BlockAfter {
            sent: false,
            data: b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n",
        };
        let started = std::time::Instant::now();
        let res = stream_openai(&mut reader, &mut |_s: &str| {}, &mut false, &cancel);
        let elapsed = started.elapsed();
        assert!(res.is_err(), "expected an error after cancel");
        assert_eq!(res.unwrap_err(), "request cancelled");
        assert!(elapsed < std::time::Duration::from_secs(2), "cancel took too long: {elapsed:?}");
    }

    #[test]
    fn stall_watchdog_trips() {
        // Point the stall watchdog low so it trips quickly. With a silent socket
        // (WouldBlock on every read), the parser must detect the stall at its
        // pre-read check rather than waiting indefinitely.
        std::env::set_var("PIR_STALL_TIMEOUT_SECS", "1");
        let cancel = Arc::new(AtomicBool::new(false));
        let mut reader = BlockAfter {
            sent: false,
            data: b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n",
        };
        let started = std::time::Instant::now();
        let res = stream_openai(&mut reader, &mut |_s: &str| {}, &mut false, &cancel);
        let elapsed = started.elapsed();
        std::env::remove_var("PIR_STALL_TIMEOUT_SECS");
        assert!(res.is_err(), "expected a stall error");
        assert!(res.unwrap_err().contains("stalled"), "expected stall error");
        assert!(elapsed < std::time::Duration::from_secs(5), "stall took too long: {elapsed:?}");
    }

    #[test]
    fn retryable_http_codes() {
        assert!(is_retryable("HTTP 429: rate limited"));
        assert!(is_retryable("HTTP 500: internal error"));
        assert!(is_retryable("HTTP 502: bad gateway"));
        assert!(is_retryable("HTTP 503: unavailable"));
        assert!(is_retryable("HTTP 504: gateway timeout"));
    }

    #[test]
    fn non_retryable_http_codes() {
        assert!(!is_retryable("HTTP 400: bad request"));
        assert!(!is_retryable("HTTP 401: unauthorized"));
        assert!(!is_retryable("HTTP 403: forbidden"));
        assert!(!is_retryable("HTTP 404: not found"));
        assert!(!is_retryable("HTTP 501: not implemented"));
    }

    #[test]
    fn retryable_transport_errors() {
        assert!(is_retryable("connection failed: Connection refused"));
        assert!(is_retryable("DNS lookup failed"));
        assert!(is_retryable("TLS handshake timed out"));
        assert!(is_retryable("stream: timed out"));
    }

    #[test]
    fn backoff_grows_and_caps() {
        let base = RETRY_BASE_BACKOFF;
        let capped = RETRY_MAX_BACKOFF;
        assert_eq!((base * 2u32.pow(0)).min(capped), Duration::from_secs(1));
        assert_eq!((base * 2u32.pow(1)).min(capped), Duration::from_secs(2));
        assert_eq!((base * 2u32.pow(2)).min(capped), Duration::from_secs(4));
        assert_eq!((base * 2u32.pow(3)).min(capped), Duration::from_secs(8));
        assert_eq!((base * 2u32.pow(10)).min(capped), capped);
    }

    #[test]
    fn timeout_constants_sane() {
        assert!(CONNECT_TIMEOUT.as_secs() >= 5);
        // The streaming *status-line* read timeout is now generous, so a slow
        // / "thinking" provider has time to send its first byte before we
        // retry. The stall watchdog (between SSE events once streaming) stays
        // short so a Ctrl-C/Ctrl-D is honoured promptly.
        assert!(READ_TIMEOUT_INIT.as_secs() >= 15);
        assert!(READ_TIMEOUT_GROWTH >= 2);
        assert!(READ_TIMEOUT_MAX.as_secs() >= READ_TIMEOUT_INIT.as_secs());
        assert!(STALL_TIMEOUT.as_secs() >= 30);
        assert_eq!(MAX_RETRIES, 4);
    }

    #[test]
    fn read_timeout_doubles_each_retry() {
        // Mirrors the chat() loop's computation: generous initial timeout that
        // doubles per attempt and caps at READ_TIMEOUT_MAX. This is what stops
        // every attempt from timing out identically at 2s (the old bug).
        let compute = |attempt: u32| {
            (READ_TIMEOUT_INIT * READ_TIMEOUT_GROWTH.saturating_pow(attempt)).min(READ_TIMEOUT_MAX)
        };
        assert_eq!(compute(0), Duration::from_secs(30));
        assert_eq!(compute(1), Duration::from_secs(60));
        assert_eq!(compute(2), Duration::from_secs(120));
        assert_eq!(compute(3), Duration::from_secs(240)); // hits cap
        assert_eq!(compute(4), Duration::from_secs(240));
        assert!(compute(1) > compute(0));
    }
}
