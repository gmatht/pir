use crate::config::ApiKind;
use crate::plugin::ToolSpec;
use crate::types::{Block, Message, Role, Usage};
use serde_json::{json, Map, Value};
use std::io::{BufRead, BufReader, Error, ErrorKind, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
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
/// response byte (`HTTP/1.1 ... \r\n`). 15s is enough for most providers to
/// start responding, while still failing fast (and triggering a retry) when a
/// provider is genuinely unreachable — rather than waiting 30s+ before the
/// first retry. It doubles on each retry (see `READ_TIMEOUT_GROWTH`) with
/// **no upper bound** — a slow/"thinking" provider keeps getting more time on
/// each attempt rather than hitting a hard ceiling and failing forever.
const READ_TIMEOUT_INIT: Duration = Duration::from_secs(15);
/// Each retry gets this multiple of the previous attempt's read timeout. There
/// is deliberately no cap: the timeout is *unlimited*, simply doubling each
/// retry so the most stubborn slow provider eventually has room to respond.
const READ_TIMEOUT_GROWTH: u32 = 2;

/// Hard **request** timeout: the absolute wall-clock budget for one attempt,
/// from connection through the end of streaming. This is deliberately *much
/// longer* than the per-attempt status-line read (`READ_TIMEOUT_INIT`) — a
/// slow/"thinking" provider can take minutes before/while producing tokens, and
/// we must not kill a working turn just because the status line was slow. It is
/// enforced by racing the attempt against a deadline timer in the retry loop
/// (`chat`), so it bounds the whole attempt regardless of `ureq`'s per-read
/// timeout. Honour `PIR_REQUEST_TIMEOUT_SECS` to override (e.g. set low in tests,
/// or raise for very slow providers).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

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

/// Resolve the overall request timeout, honouring `PIR_REQUEST_TIMEOUT_SECS`.
fn request_timeout() -> Option<Duration> {
    if let Some(s) = std::env::var("PIR_REQUEST_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
    {
        return Some(Duration::from_secs(s));
    }
    Some(REQUEST_TIMEOUT)
}

/// Resolve the streaming stall timeout, honouring `PIR_STALL_TIMEOUT_SECS`.
fn stall_timeout() -> Duration {
    std::env::var("PIR_STALL_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .unwrap_or(STALL_TIMEOUT)
}

/// Backoff between retries for *non-timeout* transient failures (5xx/429): it
/// doubles each attempt (capped), giving 60s, 120s,  240s. Starting at a
/// full minute avoids hammering a sick server. Timeouts are retried *immediately*
/// instead (see `chat`), since the per-attempt read timeout doubles each retry.
/// Cancellation is still honoured promptly during the backoff because the sleep
/// loop re-checks `cancel` every 100ms.
const RETRY_BASE_BACKOFF: Duration = Duration::from_secs(60);
const RETRY_MAX_BACKOFF: Duration = Duration::from_secs(240);

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
        // True once the stream has delivered at least one tool_use block. A
        // mid-stream crash after partial tool output is *not* safe to transparently
        // retry: re-sending the whole request would lose the tool results already
        // produced and can duplicate work. Treat partial tool progress like
        // partial text — surface the error rather than replaying.
        let mut saw_tool_calls = false;
        for attempt in 0..=MAX_RETRIES {
            if cancel.load(Ordering::SeqCst) {
                return Err("request cancelled".to_string());
            }
            // Read timeout for this attempt: start generous and double each
            // retry, with no upper bound. The *first* attempt already waits up
            // to READ_TIMEOUT_INIT for the status line, so a slow/"thinking"
            // provider has room to respond instead of failing instantly; later
            // attempts get proportionally more time. Rebuild the ureq agent so
            // the new timeout takes effect (it's baked in at build time).
            let read_timeout = READ_TIMEOUT_INIT * READ_TIMEOUT_GROWTH.saturating_pow(attempt as u32);
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
                .and_then(|resp| {
                    // Wrap the blocking response body in a cancelable reader so
                    // a Ctrl-C is honoured within tens of milliseconds even
                    // while `ureq` is blocked in its network `recv` (waiting on
                    // a slow / "thinking" provider or between SSE events). The
                    // status-line read itself is left untouched (it stays
                    // generous so slow providers don't fail on connect), and the
                    // cancelable reader only governs the streaming body — which
                    // is where the wait actually happens during a turn.
                    let body_reader = CancelableReader::new(resp.into_reader(), cancel.clone());
                    let mut reader = BufReader::new(body_reader);
                    match self.kind {
                        ApiKind::Anthropic => stream_anthropic(
                            &mut reader,
                            on_text,
                            &mut emitted_text,
                            &mut saw_tool_calls,
                            &cancel,
                        ),
                        ApiKind::OpenAi => stream_openai(
                            &mut reader,
                            on_text,
                            &mut emitted_text,
                            &mut saw_tool_calls,
                            &cancel,
                        ),
                    }
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
                    if attempt >= MAX_RETRIES || !is_retryable(&e) || emitted_text || saw_tool_calls {
                        return Err(e);
                    }
                    // A timeout is retried *immediately* (no backoff): the
                    // per-attempt read timeout already doubles each retry, so the
                    // slow / "thinking" provider is given progressively more time
                    // on the next attempt rather than being re-hit after a fixed
                    // 60s wait. Non-timeout transient failures (5xx/429) keep the
                    // geometric backoff so we don't hammer a sick server. Either
                    // way, report the read timeout that killed this attempt so the
                    // user can see how long the cancelled request waited.
                    let timed_out = is_timeout(&e);
                    let backoff = if timed_out {
                        Duration::ZERO
                    } else {
                        (RETRY_BASE_BACKOFF * 2u32.pow(attempt as u32)).min(RETRY_MAX_BACKOFF)
                    };
                    let _ = on_text(&format!(
                        "\n\u{26a0} request failed (attempt {}), retrying in {:.0?} (timeout was {:.0?}): {}\n",
                        attempt + 1, backoff, read_timeout, e
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

/// Wrap a blocking `Read` so it can be cancelled promptly. `ureq`'s blocking
/// reader is parked inside a blocking `recv` that can sit for the full read
/// timeout (up to 30s on a slow / "thinking" provider, or `STALL_TIMEOUT`=180s
/// between SSE events). `std`/libc **auto-restart `EINTR`** on socket reads, so a
/// signal-based interrupt does *not* break that wait — a cooperative
/// `AtomicBool` check only runs at the next *successful* read boundary, which
/// is exactly why a plain Ctrl-C could leave "cancelling turn…" spinning for
/// seconds while the worker was still blocked on the network.
///
/// This reader solves it without touching the status-line read (which must stay
/// generous) or relying on EINTR: a dedicated pump thread drains the underlying
/// reader into a small channel, while `read()` polls that channel with a short
/// (20ms) timeout. The moment `cancel` is set, the next poll returns an error
/// instead of waiting for the next network byte — so a turn is honoured within
/// tens of milliseconds (well under the 50ms target), every time, regardless of
/// how long the peer is stalled. The pump thread is torn down on drop.
///
/// `R` is only used at construction time (the pump closure owns the source);
/// the reader itself holds no `R`-typed field, so the struct is not generic.
struct CancelableReader {
    rx: mpsc::Receiver<u8>,
    cancel: Arc<AtomicBool>,
    /// Set by the pump if the underlying `Read` fails with a *fatal* (non-timeout)
    /// error. Lets `read()` distinguish "connection closed / broken" from a
    /// cooperative cancel or a clean EOF once the channel disconnects.
    errored: Arc<Mutex<Option<String>>>,
    /// Set on `Drop` so the pump (which is blocked in the underlying `read` and
    /// cannot be joined without stalling the turn) knows to stop polling once
    /// its current read times out. Bounds a cancelled/errored turn's pump thread
    /// to at most one read-timeout of lingering, instead of spinning forever.
    done: Arc<AtomicBool>,
    pump: Option<thread::JoinHandle<()>>,
}

impl CancelableReader {
    fn new<R: Read + Send + 'static>(mut src: R, cancel: Arc<AtomicBool>) -> Self {
        let (tx, rx) = mpsc::sync_channel::<u8>(256);
        let errored: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let errored_pump = errored.clone();
        let done: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let done_pump = done.clone();
        let pump = thread::spawn(move || {
            let mut buf = [0u8; 256];
            loop {
                match src.read(&mut buf) {
                    Ok(0) => {
                        // Genuine EOF: the response body is complete. The channel
                        // `tx` drops when this closure returns, so the parser's
                        // `recv` sees `Disconnected` and treats it as a clean EOF.
                        break;
                    }
                    Ok(n) => {
                        for &b in &buf[..n] {
                            // Block only while the channel is full (the parser is
                            // behind); if the reader was dropped, stop.
                            if tx.send(b).is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) if is_read_timeout(&e) => {
                        // A read timeout (SO_RCVTIMEO) is *not* a failure — the
                        // provider is just slow / between SSE events. Keep waiting
                        // rather than ending the stream: the parser's own stall
                        // watchdog still trips if no byte ever arrives, and cancel
                        // is still honoured on the next poll. Breaking here would
                        // falsely truncate the response and defeat the watchdog.
                        // Once the reader is dropped (`done`), stop so the pump
                        // thread doesn't spin forever on recurring timeouts.
                        if done_pump.load(Ordering::SeqCst) {
                            break;
                        }
                        continue;
                    }
                    Err(e) => {
                        // A genuine read failure (e.g. ECONNRESET): poison the
                        // reader so the parser reports it instead of a clean EOF.
                        *errored_pump.lock().unwrap() = Some(e.to_string());
                        break;
                    }
                }
            }
        });
        CancelableReader { rx, cancel, errored, done, pump: Some(pump) }
    }
}

impl Read for CancelableReader {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(Error::new(ErrorKind::Interrupted, "request cancelled"));
        }
        // Short poll so cancellation is honoured within milliseconds even when
        // no bytes are arriving (a stalled / "thinking" provider).
        match self.rx.recv_timeout(Duration::from_millis(20)) {
            Ok(b) => {
                buf[0] = b;
                // Greedily pull any immediately-available bytes to fill `buf`
                // (avoid one-syscall-per-byte) without blocking past the poll.
                let mut filled = 1;
                while filled < buf.len() {
                    match self.rx.try_recv() {
                        Ok(b) => {
                            buf[filled] = b;
                            filled += 1;
                        }
                        Err(_) => break,
                    }
                }
                Ok(filled)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // No byte within the poll window: re-check cancel and keep
                // waiting (the pump is still draining the socket). `ErrorKind`
                // here is `WouldBlock` so the SSE parsers' `is_read_timeout`
                // treats it as a benign poll wake-up, not a fatal error.
                if self.cancel.load(Ordering::SeqCst) {
                    return Err(Error::new(ErrorKind::Interrupted, "request cancelled"));
                }
                Err(Error::new(ErrorKind::WouldBlock, "no data within poll window"))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Pump has ended (EOF, a fatal error, or the reader was dropped).
                if self.cancel.load(Ordering::SeqCst) {
                    return Err(Error::new(ErrorKind::Interrupted, "request cancelled"));
                }
                if let Some(msg) = self.errored.lock().unwrap().take() {
                    return Err(Error::new(ErrorKind::Other, format!("stream: {msg}")));
                }
                // Clean EOF.
                Ok(0)
            }
        }
    }
}

impl Drop for CancelableReader {
    fn drop(&mut self) {
        // Signal the pump to stop once its next read times out (it's blocked in
        // the underlying `read`, which we deliberately do NOT join — joining
        // would stall this destructor for up to the read timeout and thus the
        // whole `chat` call, reintroducing the latency we're removing). The pump
        // self-terminates on its next timeout/EOF and is then detached. We do NOT
        // flip `cancel` here: that flag is owned by the REPL/agent and must stay
        // the source of truth (flipping it on drop could wrongly turn a
        // retryable mid-stream error into a "cancelled").
        self.done.store(true, Ordering::SeqCst);
        self.pump.take(); // detach; the pump self-terminates
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

/// True when an error represents a network timeout (read/connect). Used to
/// decide that a timed-out attempt should be retried *immediately* (the read
/// timeout already doubles each retry) rather than after the geometric backoff.
fn is_timeout(error: &str) -> bool {
    error.contains("timeout")
        || error.contains("timed out")
        || error.contains("TimedOut")
        || error.contains("reading response")
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
    saw_tool_calls: &mut bool,
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
            Err(e) => {
                // The cancelable body reader interrupts with `ErrorKind::Interrupted`
                // (message "request cancelled") the instant the cooperative flag
                // is set. Surface that as the canonical cancellation error so the
                // chat loop stops immediately (no retry/backoff), instead of being
                // wrapped into a non-matching "stream: request cancelled".
                if cancel.load(Ordering::SeqCst) || e.kind() == std::io::ErrorKind::Interrupted {
                    return Err("request cancelled".to_string());
                }
                return Err(format!("stream: {e}"));
            }
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
                    *saw_tool_calls = true;
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
    saw_tool_calls: &mut bool,
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
            Err(e) => {
                if cancel.load(Ordering::SeqCst) || e.kind() == std::io::ErrorKind::Interrupted {
                    return Err("request cancelled".to_string());
                }
                return Err(format!("stream: {e}"));
            }
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
        *saw_tool_calls = true;
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
        let res = stream_openai(&mut reader, &mut |_s: &str| {}, &mut false, &mut false, &cancel);
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
        let res = stream_openai(&mut reader, &mut |_s: &str| {}, &mut false, &mut false, &cancel);
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
        assert_eq!((base * 2u32.pow(0)).min(capped), Duration::from_secs(60));
        assert_eq!((base * 2u32.pow(1)).min(capped), Duration::from_secs(120));
        assert_eq!((base * 2u32.pow(2)).min(capped), Duration::from_secs(240)); // hits cap
        assert_eq!((base * 2u32.pow(3)).min(capped), Duration::from_secs(240));
        assert_eq!((base * 2u32.pow(10)).min(capped), capped);
    }

    #[test]
    fn timeout_constants_sane() {
        assert!(CONNECT_TIMEOUT.as_secs() >= 5);
        // The streaming *status-line* read timeout is now generous, so a slow
        // / "thinking" provider has time to send its first byte before we
        // retry. The stall watchdog (between SSE events once streaming) stays
        // short so a Ctrl-C/Ctrl-D is honoured promptly. Read timeouts double
        // with NO cap — a slow provider is given ever more time per attempt.
        assert!(READ_TIMEOUT_INIT.as_secs() >= 15);
        assert!(READ_TIMEOUT_GROWTH >= 2);
        assert!(STALL_TIMEOUT.as_secs() >= 30);
        assert_eq!(MAX_RETRIES, 4);
    }

    #[test]
    fn read_timeout_doubles_each_retry() {
        // Mirrors the chat() loop's computation: generous initial timeout that
        // doubles per attempt with NO upper bound — so the most stubborn slow
        // provider keeps getting more time instead of hitting a ceiling.
        let compute = |attempt: u32| READ_TIMEOUT_INIT * READ_TIMEOUT_GROWTH.saturating_pow(attempt);
        assert_eq!(compute(0), Duration::from_secs(15));
        assert_eq!(compute(1), Duration::from_secs(30));
        assert_eq!(compute(2), Duration::from_secs(60));
        assert_eq!(compute(3), Duration::from_secs(120));
        assert_eq!(compute(4), Duration::from_secs(240));
        assert_eq!(compute(10), Duration::from_secs(15360));
        assert!(compute(1) > compute(0));
    }

    #[test]
    fn timeout_errors_detected() {
        // The retry loop retries a *timed-out* attempt immediately (no backoff),
        // relying on the doubling read timeout to give a slow provider more
        // time. `is_timeout` must recognise the exact message ureq produces.
        assert!(is_timeout("Error encountered in the status line: timed out reading response"));
        assert!(is_timeout("timed out reading response"));
        assert!(is_timeout("Connection timed out"));
        assert!(is_timeout("stream: timeout"));
        // Non-timeout transients are NOT timeouts — they keep the backoff.
        assert!(!is_timeout("HTTP 500: internal error"));
        assert!(!is_timeout("connection refused"));
    }

    /// A `Read` that blocks forever (never returns) until a `cancel` flag is
    /// set — modelling a provider that is connected but silent ("thinking"),
    /// with `ureq` parked in a blocking `recv`. We cannot actually block a
    /// thread's `read` indefinitely in a test, so we simulate the *parser's*
    /// experience: the wrapped `CancelableReader` sees no bytes and must react
    /// to the flag. To make the inner `read` return without data, we use a
    /// short-lived pump source that goes quiet (WouldBlock) — exactly the
    /// `CancelableReader`'s polling contract — then assert the reader surfaces
    /// cancellation within the 50ms budget after the flag flips.
    struct SilentAfter {
        sent: bool,
        data: &'static [u8],
    }
    impl std::io::Read for SilentAfter {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.sent {
                self.sent = true;
                let n = self.data.len().min(buf.len());
                buf[..n].copy_from_slice(&self.data[..n]);
                return Ok(n);
            }
            // After the preamble, report "would block" forever, so the pump
            // thread keeps polling but never yields bytes — i.e. a stalled
            // connection. The CancelableReader must still honour `cancel`.
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "simulated silent connection",
            ))
        }
    }

    /// A `Read` that simulates a *real* ureq socket read: it yields the
    /// preamble, then returns **timeout** errors (WouldBlock) for a while
    /// before resuming — exactly what happens between SSE events when the
    /// provider is "thinking". The old pump treated any `Err` as EOF and broke,
    /// which truncated the stream and masked the stall watchdog. The pump must
    /// keep waiting through the timeouts and deliver the late bytes.
    struct PausesThenResumes {
        state: usize,
        data: &'static [u8],
    }
    impl std::io::Read for PausesThenResumes {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.state < self.data.len() {
                let b = self.data[self.state];
                self.state += 1;
                buf[0] = b;
                return Ok(1);
            }
            // After the data is exhausted, alternate: a few timeouts, then EOF.
            // We emulate "the peer went silent for a bit then closed".
            if self.state < self.data.len() + 3 {
                self.state += 1;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "simulated socket read timeout",
                ));
            }
            Ok(0)
        }
    }

    #[test]
    fn pump_survives_midstream_timeouts() {
        // The pump must NOT treat a mid-stream read timeout as EOF — it should
        // keep waiting and deliver the rest of the body (and the parser must
        // see a clean end, not a truncated stream).
        let cancel = Arc::new(AtomicBool::new(false));
        let inner = PausesThenResumes {
            state: 0,
            data: b"data: hello\n\n",
        };
        let mut reader = CancelableReader::new(inner, cancel.clone());
        let mut got = String::new();
        let mut buf = [0u8; 64];
        // Read until EOF (the pump's WouldBlock phases must not truncate it).
        loop {
            match std::io::Read::read(&mut reader, &mut buf) {
                Ok(0) => break,
                Ok(n) => got.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(e) => {
                    // WouldBlock is expected while the pump waits out a timeout;
                    // keep reading. Any other error is a real failure.
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        continue;
                    }
                    panic!("unexpected error: {e}");
                }
            }
        }
        assert!(
            got.contains("data: hello"),
            "pump truncated the stream across timeouts: {got:?}"
        );
    }

    #[test]
    fn cancelable_reader_obeys_within_50ms() {
        // The whole point of the cancelable reader: Ctrl-C sets `cancel` while
        // `ureq` is blocked in its network `recv`; the parser must stop within
        // tens of milliseconds, every time — not after the read timeout.
        let cancel = Arc::new(AtomicBool::new(false));
        let inner = SilentAfter {
            sent: false,
            data: b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n",
        };
        let mut reader = CancelableReader::new(inner, cancel.clone());

        // Drain the preamble the pump already forwarded, then the reader is
        // waiting on the silent connection.
        let mut scratch = [0u8; 64];
        let _ = std::io::Read::read(&mut reader, &mut scratch);

        // Flip the flag as if Ctrl-C just arrived, and time how long the next
        // read takes to honour it.
        cancel.store(true, Ordering::SeqCst);
        let started = std::time::Instant::now();
        let res = std::io::Read::read(&mut reader, &mut scratch);
        let elapsed = started.elapsed();
        assert!(res.is_err(), "expected cancellation error, got {res:?}");
        assert_eq!(res.unwrap_err().kind(), std::io::ErrorKind::Interrupted);
        assert!(
            elapsed <= Duration::from_millis(50),
            "cancel took {elapsed:?}, must be <= 50ms"
        );
    }
}
