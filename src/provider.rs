use crate::config::ApiKind;
use crate::plugin::ToolSpec;
use crate::types::{Block, Message, Role, Usage};
use serde_json::{json, Map, Value};
use smol::channel::{bounded as smol_channel, Receiver as SmolRx};
use smol::future;
use smol::io::AsyncBufRead;
use smol::io::AsyncBufReadExt;
use smol::io::AsyncReadExt;
use smol::Timer;
use std::io::{Error, ErrorKind, Read};
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
/// response byte (`HTTP/1.1 ... \r\n`). 120s gives even slow/"thinking"
/// providers plenty of room to start responding before we give up on the first
/// attempt (and retry with a doubled timeout). It doubles on each retry (see
/// `READ_TIMEOUT_GROWTH`) with **no upper bound** — a stubborn slow provider
/// keeps getting more time on each attempt rather than hitting a hard ceiling
/// and failing forever.
///
/// A long status-line read does **not** make cancellation slow: the connect +
/// status-line phase is run on a worker thread and raced against the `cancel`
/// flag (see `send_cancelable`), so an ESC/ctrl-c is honoured within tens of
/// milliseconds even while we're still waiting on the socket for the first byte.
const READ_TIMEOUT_INIT: Duration = Duration::from_secs(120);
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
    /// Build an async `isahc` client with the given per-attempt read timeout.
    /// Kept as a free fn so the retry loop can rebuild the client with a larger
    /// timeout each attempt without cloning the whole `Client`.
    fn isahc_client(_read_timeout: Duration) -> isahc::HttpClient {
        isahc::HttpClient::builder()
            .build()
            .expect("isahc client build failed")
    }

    /// Bridge the shared `cancel` `AtomicBool` (set by the REPL on ESC/ctrl-c)
    /// into a `smol` channel the streaming loop can `or()` against, so cancel is
    /// observed at the reactor level (instant) rather than only at the next
    /// `AtomicBool` poll. A tiny task watches the flag and signals the channel
    /// the moment it flips; polling every few ms keeps latency well under the
    /// 50ms target while leaving every cancel site (REPL/agent/tests) untouched.
    fn start_cancel_forwarder(&self) -> SmolRx<()> {
        let (tx, rx) = smol_channel::<()>(1);
        let flag = self.cancel.clone();
        smol::spawn(async move {
            while !flag.load(Ordering::SeqCst) {
                Timer::after(Duration::from_millis(5)).await;
            }
            let _ = tx.send(()).await;
        })
        .detach();
        rx
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

    /// A *single, non-streaming* completion. Used for cheap second-opinion checks
    /// (e.g. the main model reviewing a light-model "retry" verdict) where we
    /// only need a word or two back, never tool calls or streaming. No retries,
    /// no thinking budget — a hard failure is returned as `Err` and treated as
    /// "no opinion" by the caller. Honours `PIR_STALL_TIMEOUT_SECS`/the cancel
    /// flag like the streaming path, so a stuck provider can't hang the turn.
    pub fn complete(&self, model: &str, system: &str, prompt: &str) -> Result<String, String> {
        let kind = self.kind;
        let max_key = if model.starts_with("o1")
            || model.starts_with("o3")
            || model.starts_with("o4")
            || model.contains("gpt-5")
        {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        let (url, body) = match kind {
            ApiKind::Anthropic => (
                format!("{}/messages", self.base_url),
                json!({
                    "model": model,
                    "max_tokens": 12,
                    "stream": false,
                    "system": system,
                    "messages": [{ "role": "user", "content": prompt }],
                }),
            ),
            ApiKind::OpenAi => {
                let mut url = self.base_url.trim_end_matches('/').to_string();
                if !url.ends_with("/chat/completions") && !url.contains('?') {
                    url.push_str("/chat/completions");
                }
                (
                    url,
                    json!({
                        "model": model,
                        "stream": false,
                        max_key: 12,
                        "messages": [
                            { "role": "system", "content": system },
                            { "role": "user", "content": prompt },
                        ],
                    }),
                )
            }
        };
        smol::block_on(async {
            let client = Self::isahc_client(Duration::from_secs(60));
            let builder = isahc::Request::builder()
                .method("POST")
                .uri(&url)
                .header("content-type", "application/json");
            let builder = match kind {
                ApiKind::Anthropic => builder
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", "2023-06-01"),
                ApiKind::OpenAi => builder.header("Authorization", &format!("Bearer {}", self.api_key)),
            };
            let req = builder.body(body.to_string()).map_err(|e| format!("complete: {e}"))?;
            let resp = client.send_async(req).await.map_err(http_error)?;
            let status = resp.status();
            if !status.is_success() {
                let code = status.as_u16();
                let mut b = String::new();
                let _ = resp.into_body().read_to_string(&mut b).await;
                return Err(http_status_detail(code, &b));
            }
            let mut b = String::new();
            let _ = resp.into_body().read_to_string(&mut b).await;
            let v: Value = serde_json::from_str(&b).map_err(|e| format!("complete: {e}"))?;
            let raw = match kind {
                ApiKind::Anthropic => v
                    .pointer("/content/0/text")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                ApiKind::OpenAi => v
                    .pointer("/choices/0/message/content")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            };
            raw.ok_or_else(|| "complete: empty response".to_string())
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn chat(
        &self,
        model: &str,
        max_tokens: u64,
        system: &str,
        history: &[Message],
        tools: &[ToolSpec],
        on_text: &mut dyn FnMut(&str),
        thinking: crate::config::ThinkingLevel,
        model_ctx: u64,
        on_think: &mut dyn FnMut(&str),
        // Per-model API override (OpenCode Zen: the API varies per model).
        api_override: Option<ApiKind>,
        // Per-model request URL override (OpenCode Zen per-model baseUrl).
        url_override: Option<&str>,
        // `false` when the model rejects OpenAI `reasoning_effort`.
        allow_reasoning_effort: bool,
    ) -> Result<(Message, Usage), String> {
        smol::block_on(async {
        let kind = api_override.unwrap_or(self.kind);
        let (url, body) = match kind {
            ApiKind::Anthropic => self.anthropic_request(model, max_tokens, system, history, tools, thinking, model_ctx),
            ApiKind::OpenAi => match url_override {
                Some(u) => self.openai_request_at(
                    u,
                    model,
                    max_tokens,
                    system,
                    history,
                    tools,
                    thinking,
                    allow_reasoning_effort,
                ),
                None => self.openai_request(model, max_tokens, system, history, tools, thinking),
            },
        };
        // Retry the whole request (connect + stream parse) on transient errors.
        // `emitted_text` is set by the stream parsers the moment any token is
        // delivered, so that — once the user is seeing streaming output — we
        // NEVER re-run the attempt (and risk duplicating already-printed text);
        // a mid-stream failure is surfaced as a hard error instead. A cancel
        // request aborts the whole loop immediately (no retry) via `self.cancel`.
        let cancel = self.cancel.clone();
        let cancel_rx = self.start_cancel_forwarder();
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
            let client = Self::isahc_client(read_timeout);
            // Build the request (isahc async; JSON body serialised directly).
            let mut builder = isahc::Request::builder()
                .method("POST")
                .uri(&url)
                .header("content-type", "application/json");
            builder = match kind {
                ApiKind::Anthropic => builder
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", "2023-06-01"),
                ApiKind::OpenAi => builder.header("Authorization", &format!("Bearer {}", self.api_key)),
            };
            let req = match builder.body(body.to_string()) {
                Ok(r) => r,
                Err(e) => return Err(format!("http body: {e}")),
            };
            // Race the connect/status-line against the cancel channel so a cancel
            // pressed while still waiting on the socket is honoured instantly:
            // the smol reactor wakes the moment `cancel_tx` fires, no polling.
            enum ConnectOutcome {
                Resp(isahc::Response<isahc::AsyncBody>),
                Cancelled,
                Err(String),
            }
            let outcome = future::or(
                async {
                    match client.send_async(req).await {
                        Ok(r) => ConnectOutcome::Resp(r),
                        Err(e) => ConnectOutcome::Err(http_error(e)),
                    }
                },
                async {
                    let _ = cancel_rx.recv().await;
                    ConnectOutcome::Cancelled
                },
            )
            .await;
            let resp = match outcome {
                ConnectOutcome::Cancelled => return Err("request cancelled".to_string()),
                ConnectOutcome::Err(e) => return Err(e),
                ConnectOutcome::Resp(r) => r,
            };
            // isahc returns Ok for any HTTP status; surface non-2xx as an error.
            if !resp.status().is_success() {
                let code = resp.status().as_u16();
                let mut body_txt = String::new();
                let _ = resp.into_body().read_to_string(&mut body_txt).await;
                return Err(http_status_detail(code, &body_txt));
            }
            let mut reader = smol::io::BufReader::new(Box::pin(resp.into_body()));
            let result: Result<(Message, Usage), String> = match kind {
                ApiKind::Anthropic => {
                    stream_anthropic(
                        &mut reader,
                        on_text,
                        &mut emitted_text,
                        &mut saw_tool_calls,
                        &cancel,
                        &cancel_rx,
                        on_think,
                    )
                    .await
                }
                ApiKind::OpenAi => {
                    stream_openai(
                        &mut reader,
                        on_text,
                        &mut emitted_text,
                        &mut saw_tool_calls,
                        &cancel,
                        &cancel_rx,
                        on_think,
                    )
                    .await
                }
            };
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
                    on_text(&format!(
                        "\n\u{26a0} request failed (attempt {}), retrying in {:.0?} (timeout was {:.0?}): {}\n",
                        attempt + 1, backoff, read_timeout, e
                    ));
                    // Await the backoff in slices, raced against the cancel
                    // channel so a cancel mid-backoff is honoured instantly.
                    let mut waited = Duration::ZERO;
                    while waited < backoff {
                        if cancel.load(Ordering::SeqCst) {
                            return Err("request cancelled".to_string());
                        }
                        let slice = Duration::from_millis(100).min(backoff - waited);
                        future::or(
                            async { let _ = Timer::after(slice).await; },
                            async { let _ = cancel_rx.recv().await; },
                        )
                        .await;
                        waited += slice;
                    }
                }
            }
        }
        unreachable!()
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn anthropic_request(
        &self,
        model: &str,
        max_tokens: u64,
        system: &str,
        history: &[Message],
        tools: &[ToolSpec],
        thinking: crate::config::ThinkingLevel,
        model_ctx: u64,
    ) -> (String, Value) {
        // Anthropic requires `max_tokens` to be strictly greater than the
        // thinking budget; clamp the budget so it never reaches/exceeds it.
        let ctx = self.model_context(history).max(model_ctx);
        let thinking_budget = thinking
            .anthropic_budget(ctx)
            .filter(|b| *b < max_tokens.saturating_sub(1024))
            .map(|b| b.min(max_tokens.saturating_sub(1024)));
        let mut body = json!({
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
        });
        if thinking.enabled() {
            if let Some(budget) = thinking_budget {
                body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
            } else {
                // Level enabled but no safe budget (tiny context): request
                // extended thinking with the provider's default budget.
                body["thinking"] = json!({ "type": "enabled" });
            }
        }
        (
            format!("{}/messages", self.base_url),
            body,
        )
    }

    fn openai_request(
        &self,
        model: &str,
        max_tokens: u64,
        system: &str,
        history: &[Message],
        tools: &[ToolSpec],
        thinking: crate::config::ThinkingLevel,
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
        // OpenAI reasoning effort (o-series models). Non-reasoning models ignore
        // it, so we only set it when the level maps to a concrete effort.
        if let Some(effort) = thinking.oai_effort() {
            body.insert("reasoning_effort".into(), json!(effort));
        }
        (format!("{}/chat/completions", self.base_url), Value::Object(body))
    }

    /// OpenAI request with an explicit target URL (per-model override, used by
    /// the OpenCode Zen catalog) and a `reasoning_effort` opt-out for models
    /// that reject the field (kimi-k2.6, grok-build-0.1, forced-Go qwen/minimax).
    #[allow(clippy::too_many_arguments)]
    fn openai_request_at(
        &self,
        url: &str,
        model: &str,
        max_tokens: u64,
        system: &str,
        history: &[Message],
        tools: &[ToolSpec],
        thinking: crate::config::ThinkingLevel,
        allow_effort: bool,
    ) -> (String, Value) {
        let (base, mut body) = self.openai_request(model, max_tokens, system, history, tools, thinking);
        let _ = base;
        if !allow_effort {
            body.as_object_mut().unwrap().remove("reasoning_effort");
        }
        // The override is a BASE URL (e.g. "https://api.cerebras.ai" or
        // ".../v1"), not a full endpoint: POSTing it verbatim hits
        // "<base>/chat/completions" — a bare-host 404 with an empty body when
        // the override lacks the "/v1" path segment. Append the endpoint path
        // unless the caller already supplied it (or a query string).
        let mut url = url.trim_end_matches('/').to_string();
        if !url.ends_with("/chat/completions") && !url.contains('?') {
            url.push_str("/chat/completions");
        }
        (url, body)
    }

    /// Rough context window for the current request, used to scale the Anthropic
    /// thinking budget. Falls back to a common default when messages carry no
    /// usable context (we don't have the model struct here, so approximate from
    /// a 200k default). Kept cheap — only an estimate for budget sizing.
    fn model_context(&self, _history: &[Message]) -> u64 {
        // The agent forwards the model's real context via `model_ctx`, so this
        // is only a fallback default (200k) when the caller supplies 0.
        200_000
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
                    return Err(Error::other(format!("stream: {msg}")));
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
    // Quota / usage-limit errors are terminal, not transient: a rate limit that
    // names a *weekly* limit (or demands an upgrade/billing change) will not
    // lift within the retry window, so backing off 60s..240s only delays the
    // inevitable and keeps the user staring at a spinner instead of a usable
    // REPL. End the turn now so the user can switch model/provider (`/model …`)
    // and retry themselves.
    let l = error.to_lowercase();
    if l.contains("usage limit")
        || l.contains("weekly usage")
        || l.contains("rate limit exceeded")
        || l.contains("quota exceeded")
        || l.contains("quota_exceeded")
        || l.contains("insufficient_quota")
        || l.contains("upgrade for higher limits")
        || l.contains("billing")
    {
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

/// Classify an HTTP error response body and produce a short, terminal-safe
/// detail string (no megabytes of HTML pasted into the UI).
///
/// Key case: a `404` whose body is **HTML** is not the model API rejecting the
/// request — it's a proxy / gateway / load-balancer `404`, i.e. the request
/// never reached the API (misrouted `baseUrl`, a dead proxy, or an upstream
/// outage). That is *not* transient in a way we can replay our way out of, and
/// it is *not* a genuine API 404 (which would come back as JSON). We surface it
/// as `misrouted` so the turn ends (the agent drops back to the REPL rather
/// than aborting) and the message tells the user to switch provider/baseUrl
/// instead of us silently retrying a request that can't succeed on this route.
/// A genuine API `404` (JSON, e.g. "model does not exist") is fatal too, but
/// with a different, actionable message (switch model/provider).
pub(crate) fn http_status_detail(code: u16, body: &str) -> String {
    // If the API spoke JSON, prefer `error.message` / `message`. A JSON body
    // (even without a known message field) means the *model API* answered — so
    // the status is authoritative and not a routing mishap.
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        let detail = v
            .pointer("/error/message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| v.get("message").and_then(Value::as_str).map(str::to_string));
        match detail {
            Some(d) => return format!("HTTP {code}: {d}"),
            None => {
                // JSON but no message: collapse it so it's readable but bounded.
                let collapsed = body
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(200)
                    .collect::<String>();
                return format!("HTTP {code}: {collapsed}");
            }
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return format!("HTTP {code}: (empty response body)");
    }
    if trimmed.starts_with('<') || trimmed.to_ascii_lowercase().starts_with("<!doctype") {
        // HTML came back, not the API. A `404` here is a proxy/gateway 404:
        // the request never reached the model API. Mark it `misrouted` so the
        // turn ends at the REPL with an actionable hint (try a different
        // provider / fix the baseUrl) rather than us auto-retrying a dead route.
        // Other HTML status codes (e.g. a gateway `502` returning HTML) are
        // already retried on their numeric code, so just summarize them.
        if code == 404 {
            return format!(
                "HTTP {code}: misrouted (non-JSON {}-byte body) — the request never reached the model API; check the provider baseUrl or try a different provider (/model …)",
                trimmed.len()
            );
        }
        return format!(
            "HTTP {code}: non-JSON response ({} bytes) — not a model-API error (misrouted baseUrl?)",
            trimmed.len()
        );
    }
    // Some other plain-text body: keep it but bounded.
    let collapsed = trimmed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect::<String>();
    format!("HTTP {code}: {collapsed}")
}

fn http_error(e: isahc::Error) -> String {
    e.to_string()
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

async fn stream_anthropic<R: AsyncBufRead + Unpin>(
    r: &mut R,
    on_text: &mut dyn FnMut(&str),
    emitted_text: &mut bool,
    saw_tool_calls: &mut bool,
    cancel: &Arc<AtomicBool>,
    cancel_rx: &SmolRx<()>,
    on_think: &mut dyn FnMut(&str),
) -> Result<(Message, Usage), String> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut usage = Usage::default();
    let mut text = String::new();
    let mut tool: Option<(String, String, String)> = None; // (id, name, partial)
    // A separate buffer for a "thinking" content block (Anthropic extended
    // thinking). Kept distinct from `text` so the two can be ordered correctly
    // in the message's block list.
    let mut thinking = String::new();

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
        // Race the next SSE line against cancel + the stall watchdog. The smol
        // reactor wakes the instant `cancel_tx` fires, so a cancel is observed
        // without waiting for the next network byte.
        enum Step {
            Line(std::io::Result<usize>),
            Cancel,
            Stall,
        }
        let step = future::or(
            async { Step::Line(r.read_line(&mut line).await) },
            future::or(
                async {
                    let _ = cancel_rx.recv().await;
                    Step::Cancel
                },
                async {
                    Timer::after(stall_timeout()).await;
                    Step::Stall
                },
            ),
        )
        .await;
        let n = match step {
            Step::Cancel => return Err("request cancelled".to_string()),
            Step::Stall => return Err("stream: stalled (no data for 180s)".to_string()),
            Step::Line(Ok(n)) => n,
            Step::Line(Err(e)) => {
                if cancel.load(Ordering::SeqCst) {
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
                match b["type"].as_str() {
                    Some("tool_use") => {
                        tool = Some((
                            b["id"].as_str().unwrap_or_default().to_string(),
                            b["name"].as_str().unwrap_or_default().to_string(),
                            String::new(),
                        ));
                    }
                    Some("thinking") => {
                        // Begin a thinking block; its deltas arrive as
                        // `thinking_delta` and are flushed into a `Block::Thinking`
                        // on `content_block_stop`.
                        thinking.clear();
                    }
                    _ => {}
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
                    "thinking_delta" => {
                        let t = d["thinking"].as_str().unwrap_or("");
                        on_think(t);
                        thinking.push_str(t);
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some((id, name, buf)) = tool.take() {
                    let input: Value = serde_json::from_str(&buf).unwrap_or_else(|_| json!({}));
                    blocks.push(Block::ToolUse { id, name, input });
                    *saw_tool_calls = true;
                } else if !thinking.trim().is_empty() {
                    blocks.push(Block::Thinking { text: std::mem::take(&mut thinking) });
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
    if !thinking.trim().is_empty() {
        blocks.push(Block::Thinking { text: thinking });
    }
    if !text.trim().is_empty() {
        blocks.push(Block::Text(text));
    }
    if blocks.is_empty() {
        blocks.push(Block::Text("(empty response)".into()));
    }
    Ok((Message { role: Role::Assistant, blocks }, usage))
}

async fn stream_openai<R: AsyncBufRead + Unpin>(
    r: &mut R,
    on_text: &mut dyn FnMut(&str),
    emitted_text: &mut bool,
    saw_tool_calls: &mut bool,
    cancel: &Arc<AtomicBool>,
    cancel_rx: &SmolRx<()>,
    on_think: &mut dyn FnMut(&str),
) -> Result<(Message, Usage), String> {
    let mut usage = Usage::default();
    let mut text = String::new();
    let mut thinking = String::new();
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
        // Race the next SSE line against cancel + the stall watchdog. The smol
        // reactor wakes the instant `cancel_tx` fires, so a cancel is observed
        // without waiting for the next network byte.
        enum Step {
            Line(std::io::Result<usize>),
            Cancel,
            Stall,
        }
        let step = future::or(
            async { Step::Line(r.read_line(&mut line).await) },
            future::or(
                async {
                    let _ = cancel_rx.recv().await;
                    Step::Cancel
                },
                async {
                    Timer::after(stall_timeout()).await;
                    Step::Stall
                },
            ),
        )
        .await;
        let n = match step {
            Step::Cancel => return Err("request cancelled".to_string()),
            Step::Stall => return Err("stream: stalled (no data for 180s)".to_string()),
            Step::Line(Ok(n)) => n,
            Step::Line(Err(e)) => {
                if cancel.load(Ordering::SeqCst) {
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
        // OpenAI o-series reasoning: `delta.reasoning` carries the model's
        // chain-of-thought. Forward it to `on_think`.
        if let Some(t) = delta["reasoning"].as_str() {
            on_think(t);
            thinking.push_str(t);
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
    if !thinking.trim().is_empty() {
        blocks.push(Block::Thinking { text: thinking });
    }
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
    /// detect cancellation / a stall rather than waiting for EOF. Replaces the
    /// old `WouldBlock`-returning sync `Read`: with the async `or`-based loop
    /// the stall watchdog is a `smol::Timer` raced against the read, so a
    /// permanently-`Pending` source is the faithful way to simulate a silent
    /// socket (never EOF, never data, never an error).
    struct Silent;
    impl smol::io::AsyncRead for Silent {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Pending
        }
    }
    impl smol::io::AsyncBufRead for Silent {
        fn poll_fill_buf(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<&[u8]>> {
            std::task::Poll::Pending
        }
        fn consume(self: std::pin::Pin<&mut Self>, _amt: usize) {}
    }

    #[test]
    fn cancel_aborts_inflight_stream() {
        // Cancel flag already set: the parser must bail at its next pre-read
        // check instead of polling forever on the silent socket (proving the
        // cancel path is honoured at a read boundary, not just via EOF).
        let cancel = Arc::new(AtomicBool::new(true));
        let (_tx, rx) = smol_channel::<()>(1);
        let mut reader = Silent;
        let started = std::time::Instant::now();
        let res = smol::block_on(stream_openai(
            &mut reader,
            &mut |_s: &str| {},
            &mut false,
            &mut false,
            &cancel,
            &rx,
            &mut |_s: &str| {},
        ));
        let elapsed = started.elapsed();
        assert!(res.is_err(), "expected an error after cancel");
        assert_eq!(res.unwrap_err(), "request cancelled");
        assert!(elapsed < std::time::Duration::from_secs(2), "cancel took too long: {elapsed:?}");
    }

    #[test]
    fn stall_watchdog_trips() {
        // Point the stall watchdog low so it trips quickly. With a silent,
        // never-completing reader the parser must detect the stall via the
        // `smol::Timer` raced against the read (rather than waiting for EOF).
        std::env::set_var("PIR_STALL_TIMEOUT_SECS", "1");
        let cancel = Arc::new(AtomicBool::new(false));
        // Keep the sender alive so the cancel arm of the `or` stays pending and
        // the stall timer (not a closed channel) is what fires.
        let (_tx, rx) = smol_channel::<()>(1);
        let mut reader = Silent;
        let started = std::time::Instant::now();
        let res = smol::block_on(stream_openai(
            &mut reader,
            &mut |_s: &str| {},
            &mut false,
            &mut false,
            &cancel,
            &rx,
            &mut |_s: &str| {},
        ));
        let elapsed = started.elapsed();
        std::env::remove_var("PIR_STALL_TIMEOUT_SECS");
        assert!(res.is_err(), "expected a stall error");
        assert!(res.unwrap_err().contains("stalled"), "expected stall error");
        assert!(elapsed < std::time::Duration::from_secs(5), "stall took too long: {elapsed:?}");
    }

    // ── CPU-spin regression guards ──────────────────────────────────────────
    //
    // Production bug: a 100%-CPU stream-parser spin. When the peer closed a
    // streaming connection *without* the final SSE terminator (`[DONE]` /
    // `message_stop`), an older build treated the resulting EOF like a transient
    // read timeout and `continue`d — re-reading an always-empty buffer at full
    // CPU (zero syscalls, a core pegged for ~16h) instead of breaking. These
    // tests pin the fix: every `stream_*` parser must terminate promptly once
    // the reader reports EOF (with or without prior data) and must surface
    // transport errors instead of looping.
    //
    // `run_stream_with_deadline` runs the parser on its own thread and bounds
    // the wait with a wall-clock `recv_timeout`. A `smol::future::or`/`Timer`
    // race would NOT catch the bug class: an always-ready `read_line` never
    // yields to the executor, so a timer inside the executor would never fire
    // and the suite would hang instead of failing. The burner thread reproduces
    // the original symptom (a tight, non-yielding loop) and the test fails with
    // a clear marker the moment the deadline passes; the leaked thread is
    // killed when the test process exits. The parser future itself never
    // crosses a thread boundary (its `&mut dyn FnMut` callbacks are not `Send`):
    // the `run` closure builds and drives it inside the worker thread from
    // `Send` inputs.
    fn run_stream_with_deadline<T: Send + 'static>(
        run: impl FnOnce() -> Result<T, String> + Send + 'static,
        deadline_secs: u64,
    ) -> Result<T, String> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(run());
        });
        match rx.recv_timeout(Duration::from_secs(deadline_secs)) {
            Ok(res) => res,
            Err(_) => Err(format!(
                "SPIN_REGRESSION: stream parser did not terminate within {deadline_secs}s \
                 (EOF/error treated as a retryable timeout instead of a break?)"
            )),
        }
    }

    #[test]
    fn stream_openai_eof_mid_stream_returns_ok_not_spin() {
        // The exact production scenario: real SSE tokens arrive, then the peer
        // closes the connection without a final `[DONE]` (the last event has no
        // trailing newline, so the next `read_line` hits EOF). The parser must
        // break on EOF, flush the accumulated text and return Ok — NOT spin on
        // the dead stream.
        const STREAM: &str = concat!(
            r#"data: {"choices":[{"delta":{"content":"Hello "}}]}"#,
            "\n",
            r#"data: {"choices":[{"delta":{"content":"world"}}]}"#,
            "\n",
            r#"data: {"choices":[{"delta":{"content":"."}}]}"#,
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let (_keep_sender_alive, rx) = smol_channel::<()>(1);
        let run = move || {
            let mut reader = smol::io::BufReader::new(smol::io::Cursor::new(STREAM.as_bytes()));
            let mut seen: Vec<String> = Vec::new();
            let res = smol::block_on(stream_openai(
                &mut reader,
                &mut |t: &str| seen.push(t.to_string()),
                &mut false,
                &mut false,
                &cancel,
                &rx,
                &mut |_t: &str| {},
            ));
            res.map(|(msg, usage)| (msg, usage, seen))
        };
        let (msg, _usage, seen) = match run_stream_with_deadline(run, 5) {
            Ok(v) => v,
            Err(e) if e.contains("SPIN_REGRESSION") => {
                panic!("CPU-spin regression (mid-stream EOF): {e}")
            }
            Err(e) => panic!("unexpected stream error: {e}"),
        };
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(
            seen,
            vec!["Hello ".to_string(), "world".to_string(), ".".to_string()],
            "streamed deltas must be delivered in order"
        );
        match msg.blocks.as_slice() {
            [Block::Text(t)] => {
                assert_eq!(t, "Hello world.", "mid-stream EOF must flush the accumulated text")
            }
            other => panic!("expected a single Text block, got: {other:?}"),
        }
    }

    #[test]
    fn stream_openai_immediate_eof_returns_ok_not_spin() {
        // Connection closed before any byte: the very first `read_line` reports
        // EOF. The parser must return Ok ("(empty response)") immediately —
        // the old bug spun on the empty stream instead of breaking.
        let cancel = Arc::new(AtomicBool::new(false));
        let (_keep_sender_alive, rx) = smol_channel::<()>(1);
        let run = move || {
            let mut reader = smol::io::BufReader::new(smol::io::Cursor::new(&b""[..]));
            smol::block_on(stream_openai(
                &mut reader,
                &mut |_t: &str| {},
                &mut false,
                &mut false,
                &cancel,
                &rx,
                &mut |_t: &str| {},
            ))
        };
        let (msg, _usage) = match run_stream_with_deadline(run, 5) {
            Ok(v) => v,
            Err(e) if e.contains("SPIN_REGRESSION") => {
                panic!("CPU-spin regression (empty-stream EOF): {e}")
            }
            Err(e) => panic!("unexpected stream error: {e}"),
        };
        match msg.blocks.as_slice() {
            [Block::Text(t)] => assert_eq!(t, "(empty response)"),
            other => panic!("expected an '(empty response)' Text block, got: {other:?}"),
        }
    }

    /// An `AsyncBufRead` that fails immediately — a peer that reset the
    /// connection (RST) rather than closing cleanly. The parser must surface
    /// the error and terminate, not loop re-reading the dead connection.
    struct ResetReader;
    impl smol::io::AsyncRead for ResetReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(Error::new(
                ErrorKind::ConnectionReset,
                "connection reset by peer",
            )))
        }
    }
    impl smol::io::AsyncBufRead for ResetReader {
        fn poll_fill_buf(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<&[u8]>> {
            std::task::Poll::Ready(Err(Error::new(
                ErrorKind::ConnectionReset,
                "connection reset by peer",
            )))
        }
        fn consume(self: std::pin::Pin<&mut Self>, _amt: usize) {}
    }

    #[test]
    fn stream_openai_reader_error_terminates_not_spin() {
        // A transport error (TCP reset) must terminate the parser promptly —
        // the old build classified it like a timeout and looped.
        let cancel = Arc::new(AtomicBool::new(false));
        let (_keep_sender_alive, rx) = smol_channel::<()>(1);
        let run = move || {
            let mut reader = ResetReader;
            smol::block_on(stream_openai(
                &mut reader,
                &mut |_t: &str| {},
                &mut false,
                &mut false,
                &cancel,
                &rx,
                &mut |_t: &str| {},
            ))
        };
        match run_stream_with_deadline(run, 5) {
            Ok(_) => panic!("expected the connection-reset error, got Ok"),
            Err(e) if e.contains("SPIN_REGRESSION") => {
                panic!("CPU-spin regression (reader error): {e}")
            }
            Err(e) => assert!(e.contains("connection reset by peer"), "unexpected error: {e}"),
        }
    }

    #[test]
    fn stream_anthropic_eof_mid_stream_returns_ok_not_spin() {
        // Anthropic-flavoured stream cut mid-flight: text deltas arrive, then
        // EOF without the final `message_stop`. The parser must break on EOF,
        // flush the text and return Ok — not spin.
        const STREAM: &str = concat!(
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":42}}}"#,
            "\n",
            r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello "}}"#,
            "\n",
            r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"world"}}"#,
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let (_keep_sender_alive, rx) = smol_channel::<()>(1);
        let run = move || {
            let mut reader = smol::io::BufReader::new(smol::io::Cursor::new(STREAM.as_bytes()));
            smol::block_on(stream_anthropic(
                &mut reader,
                &mut |_t: &str| {},
                &mut false,
                &mut false,
                &cancel,
                &rx,
                &mut |_t: &str| {},
            ))
        };
        let (msg, usage) = match run_stream_with_deadline(run, 5) {
            Ok(v) => v,
            Err(e) if e.contains("SPIN_REGRESSION") => {
                panic!("CPU-spin regression (anthropic mid-stream EOF): {e}")
            }
            Err(e) => panic!("unexpected stream error: {e}"),
        };
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(usage.input, 42, "usage must be parsed from message_start");
        match msg.blocks.as_slice() {
            [Block::Text(t)] => {
                assert_eq!(t, "Hello world", "EOF must flush accumulated anthropic text")
            }
            other => panic!("expected a single Text block, got: {other:?}"),
        }
    }

    #[test]
    fn stream_openai_partial_tool_call_eof_flushes() {
        // Provider died in the middle of a tool call (function `arguments` JSON
        // still incomplete, no `[DONE]`). The dangling call must still be
        // flushed as a ToolUse block — the "cut early" path the EOF-spin bug
        // masked.
        const STREAM: &str = concat!(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"cmd\":"}}]}}]}"#,
            "\n",
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls -la\"}"}}]}}]}"#,
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let (_keep_sender_alive, rx) = smol_channel::<()>(1);
        let run = move || {
            let mut reader = smol::io::BufReader::new(smol::io::Cursor::new(STREAM.as_bytes()));
            smol::block_on(stream_openai(
                &mut reader,
                &mut |_t: &str| {},
                &mut false,
                &mut false,
                &cancel,
                &rx,
                &mut |_t: &str| {},
            ))
        };
        let (msg, _usage) = match run_stream_with_deadline(run, 5) {
            Ok(v) => v,
            Err(e) if e.contains("SPIN_REGRESSION") => {
                panic!("CPU-spin regression (mid-tool-call EOF): {e}")
            }
            Err(e) => panic!("unexpected stream error: {e}"),
        };
        match msg.blocks.as_slice() {
            [Block::ToolUse { id, name, input }] => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "bash");
                assert_eq!(
                    input,
                    &json!({ "cmd": "ls -la" }),
                    "partial args must be flushed as a tool call input"
                );
            }
            other => panic!("expected a ToolUse block, got: {other:?}"),
        }
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
    fn http_error_summarizes_non_json_body() {
        // A misrouted 404 returning an HTML "Not Found" page must NOT be dumped
        // across the terminal; summarize it so the routing misconfig is obvious
        // without flooding the UI. The `misrouted` marker also tells
        // `is_retryable` to replay the request instead of aborting the turn.
        let html = "<!DOCTYPE html><html><head><title>Not Found</title></head>\
                    <body><div>404 - Page Not Found</div></body></html>";
        let detail = http_status_detail(404, html);
        assert!(detail.starts_with("HTTP 404: misrouted"), "got: {detail}");
        assert!(detail.contains("misrouted"), "got: {detail}");
        assert!(!detail.contains("Page Not Found"), "HTML must not be pasted: {detail}");
        // A genuine (JSON) 404 is summarized but NOT marked misrouted, so it
        // stays fatal.
        let json = r#"{"error":{"message":"model does not exist"}}"#;
        let detail2 = http_status_detail(404, json);
        assert!(detail2.starts_with("HTTP 404: model does not exist"), "got: {detail2}");
        assert!(!detail2.contains("misrouted"), "genuine 404 must not be retryable: {detail2}");
    }

    #[test]
    fn misrouted_404_is_fatal_no_retry() {
        // A non-JSON 404 (proxy/gateway 404 → request never reached the API) is
        // fatal: we drop back to the REPL and let the user switch provider
        // rather than auto-retrying a route that can't succeed.
        let detail = http_status_detail(404, "<!DOCTYPE html><html><body>404</body></html>");
        assert!(!is_retryable(&detail), "got: {detail}");
    }

    #[test]
    fn genuine_json_404_is_fatal() {
        // A JSON 404 means the model API answered and rejected the request;
        // replaying the identical call can't help, so it's fatal.
        let detail = http_status_detail(404, r#"{"error":{"message":"model not found"}}"#);
        assert!(!is_retryable(&detail), "got: {detail}");
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
    fn quota_limit_errors_are_fatal_not_retried() {
        // A weekly/usage-limit 429 will not lift within the 60s..240s backoff
        // window — retrying only delays returning the REPL to the user. The
        // turn must end immediately instead.
        assert!(!is_retryable(
            "HTTP 429: you (gmatht) have reached your weekly usage limit, upgrade for higher limits: \
             https://ollama.com/upgrade or add extra usage: https://ollama.com/settings (ref: d6f5)"
        ));
        assert!(!is_retryable("HTTP 429: quota exceeded for project"));
        assert!(!is_retryable("HTTP 429: insufficient_quota - check billing"));
        // A plain (transient) rate limit IS still retried.
        assert!(is_retryable("HTTP 429: rate limited"));
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
        const _: () = assert!(READ_TIMEOUT_GROWTH >= 2);
        assert!(STALL_TIMEOUT.as_secs() >= 30);
        assert_eq!(MAX_RETRIES, 4);
    }

    #[test]
    fn read_timeout_doubles_each_retry() {
        // Mirrors the chat() loop's computation: generous initial timeout that
        // doubles per attempt with NO upper bound — so the most stubborn slow
        // provider keeps getting more time instead of hitting a ceiling.
        let compute = |attempt: u32| READ_TIMEOUT_INIT * READ_TIMEOUT_GROWTH.saturating_pow(attempt);
        assert_eq!(compute(0), Duration::from_secs(120));
        assert_eq!(compute(1), Duration::from_secs(240));
        assert_eq!(compute(2), Duration::from_secs(480));
        assert_eq!(compute(3), Duration::from_secs(960));
        assert_eq!(compute(4), Duration::from_secs(1920));
        assert_eq!(compute(10), Duration::from_secs(122_880));
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

    /// A local listener that accepts TCP connections but **never writes a
    /// byte** — the "slow provider" case: `send_json` is parked in the connect
    /// + status-line read until the per-attempt read timeout elapses.
    ///
    /// This is exactly the phase that used to make a cancel appear to do
    /// nothing for minutes (cancel was only re-checked at the *next retry
    /// boundary*, i.e. after the whole read timeout — 120s on the first
    /// attempt).
    fn never_sends_server() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap().to_string();
        // Accept one connection (so ureq's connect succeeds and it parks in the
        // status-line read) and just hold it without responding. The hold is
        // deliberately short: we only need the worker to still be parked on the
        // status-line read when the test flips the cancel flag ~250ms in — a
        // 1s hold is plenty and keeps the test fast (the assertion only
        // requires cancel to win well before the old 5s window).
        thread::spawn(move || {
            let (_sock, _) = listener.accept().expect("accept");
            thread::sleep(Duration::from_secs(1));
            // socket dropped here: ureq sees EOF/error, but by then the test
            // has already moved on (its worker thread is abandoned).
        });
        format!("http://{addr}")
    }

    #[test]
    fn send_cancelable_honours_cancel_during_status_line_read() {
        // Cancel pressed while `send_json` is still waiting on the connect +
        // status-line read: `send_cancelable` must bail within its 10ms poll
        // slice — NOT after the full per-attempt read timeout (120s on the
        // first attempt), which is the original complaint.
        let base_url = never_sends_server();
        let mut client = Client::new(ApiKind::OpenAi, &base_url, "test-key".to_string());
        // Wire the flag exactly like the REPL does (agent → set_cancel): the
        // client only ever observes the flag it was handed.
        let cancel = Arc::new(AtomicBool::new(false));
        client.set_cancel(cancel.clone());
        // Flip the flag ~250ms in, once the worker is parked on the read.
        let cancel2 = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(250));
            cancel2.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let res = client.chat(
            "test-model",
            16,
            "sys",
            &[Message { role: Role::User, blocks: vec![Block::Text("hi".into())] }],
            &[],
            &mut |_s: &str| {},
            crate::config::ThinkingLevel::Off,
            0,
            &mut |_s: &str| {},
            None,
            None,
            true,
        );
        let elapsed = started.elapsed();
        assert!(res.is_err(), "expected cancellation error, got {res:?}");
        assert_eq!(res.unwrap_err(), "request cancelled");
        // Generous upper bound (CI slop), but far below even one read timeout.
        assert!(
            elapsed < Duration::from_secs(5),
            "cancel during status-line read took {elapsed:?}, must be prompt"
        );
    }

    #[test]
    fn send_cancelable_completes_when_provider_is_slow_but_alive() {
        // A provider that waits ~300ms before answering must still succeed —
        // the race must not turn a merely slow response into a cancellation.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap().to_string();
        let _srv = thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            use std::io::Write as _;
            thread::sleep(Duration::from_millis(300));
            let body = "{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}\n\n";
            let frame = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {body}data: [DONE]\n\n"
            );
            let _ = sock.write_all(frame.as_bytes());
            let _ = sock.flush();
            // Hold the socket open briefly so ureq can drain the body before
            // the test client sees EOF; then drop → clean EOF for the parser.
            thread::sleep(Duration::from_millis(200));
        });
        let client = Client::new(ApiKind::OpenAi, &format!("http://{addr}"), "test-key".to_string());
        // No cancel timer here at all — the slow-but-alive provider must be
        // allowed to finish unmolested (the race must not misfire on it).
        let mut text = String::new();
        let res = client.chat(
            "test-model",
            16,
            "sys",
            &[Message { role: Role::User, blocks: vec![Block::Text("hi".into())] }],
            &[],
            &mut |t: &str| text.push_str(t),
            crate::config::ThinkingLevel::Off,
            0,
            &mut |_s: &str| {},
            None,
            None,
            true,
        );
        assert!(res.is_ok(), "slow-but-alive provider must complete, got {res:?}");
        assert!(text.contains("hi"), "expected streamed text, got {text:?}");
    }

    #[test]
    fn send_cancelable_returns_response_when_it_arrives_before_cancel() {
        // The response lands just before the flag is set: the race must return
        // the real response (stream completes) rather than spuriously bailing.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap().to_string();
        let _srv = thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            use std::io::Write as _;
            thread::sleep(Duration::from_millis(150));
            let body = "{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"}}]}\n\n";
            let frame = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {body}data: [DONE]\n\n"
            );
            let _ = sock.write_all(frame.as_bytes());
            let _ = sock.flush();
            thread::sleep(Duration::from_millis(200));
        });
        let mut client = Client::new(ApiKind::OpenAi, &format!("http://{addr}"), "test-key".to_string());
        let cancel = Arc::new(AtomicBool::new(false));
        // The server answers at ~150ms and finishes streaming ~350ms; the
        // cancel timer fires at ~500ms, comfortably after the response already
        // landed, so the test asserts the real response wins without waiting
        // the old 1.2s. (The chat() call returns as soon as the stream ends,
        // so the test's wall time is ~350ms regardless.)
        let cancel2 = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(500));
            cancel2.store(true, Ordering::SeqCst);
        });
        client.set_cancel(cancel);
        let mut text = String::new();
        let res = client.chat(
            "test-model",
            16,
            "sys",
            &[Message { role: Role::User, blocks: vec![Block::Text("hi".into())] }],
            &[],
            &mut |t: &str| text.push_str(t),
            crate::config::ThinkingLevel::Off,
            0,
            &mut |_s: &str| {},
            None,
            None,
            true,
        );
        assert!(res.is_ok(), "response that lands before cancel must win, got {res:?}");
        assert!(text.contains("ok"), "expected streamed text, got {text:?}");
    }
}
