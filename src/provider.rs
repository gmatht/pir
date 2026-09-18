use crate::config::ApiKind;
use crate::plugin::ToolSpec;
use crate::types::{Block, Message, Role, Usage};
use isahc::config::Configurable;
use serde_json::{json, Map, Value};
use smol::channel::{bounded as smol_channel, Receiver as SmolRx};
use smol::future;
use smol::io::AsyncBufRead;
use smol::io::AsyncBufReadExt;
use std::pin::Pin;
use std::task::{Context, Poll};
use smol::io::AsyncReadExt;
use smol::Timer;
use std::io::{Error, ErrorKind, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Retry policy: transient failures that strike *before* any output is
/// produced (network blips, DNS hiccups, timeouts, transient 5xx / 429,
/// stalled streams with no bytes yet) are
/// retried **forever** — the turn never gives up on its own. The wait between
/// attempts doubles each time (see `retry_backoff`), so a sick server gets
/// progressively more breathing room instead of being hammered. Hard errors
/// (e.g. 401/unauthorized, malformed URL, quota/usage-limit) and failures
/// *after* output started (which can't be replayed without duplicating
/// already-printed text) are still fatal immediately. Cancellation (ESC/Ctrl-C)
/// aborts the loop at any point, including mid-wait.
///
/// Escape hatch (tests/scripts only): `PIR_MAX_ATTEMPTS=N` caps total attempts.
/// Unset (the default) means unbounded.
fn max_attempts() -> Option<u64> {
    std::env::var("PIR_MAX_ATTEMPTS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
}

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

/// HTTP transport backend for the streaming core. `Isahc` (default) is the
/// async client: connection pooling, cancel racing at the reactor level,
/// unbounded reads bounded by the stall watchdog. `Ureq` is the blocking
/// client run inside `smol::unblock`, bridged back to async for the shared
/// SSE parsers; per-read wake comes from the parser-side cancel/stall race,
/// with `timeout_read` as the backstop so a dangling pump thread always dies.
/// Selected by `PIR_HTTP_BACKEND` or the `http_backend` settings.json key
/// (see `config::http_backend_name`); unknown values fall back to `Isahc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HttpBackend {
    #[default]
    Isahc,
    Ureq,
}

impl HttpBackend {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "isahc" => Some(HttpBackend::Isahc),
            "ureq" => Some(HttpBackend::Ureq),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            HttpBackend::Isahc => "isahc",
            HttpBackend::Ureq => "ureq",
        }
    }
}

/// Wait before re-issuing a failed attempt. It doubles with each attempt so a
/// struggling server gets progressively more room: timeouts (the server was
/// merely slow) start short — 10s, 20s, 40s … capped at 5min — while other
/// transient failures (5xx/429/transport, i.e. the server may be sick) start
/// at 30s — 30s, 60s, 120s … capped at 10min. Cancellation is still honoured
/// promptly during the wait because the sleep loop re-checks `cancel` every
/// 100ms. `PIR_RETRY_BASE_SECS` / `PIR_RETRY_MAX_SECS` override the base/cap
/// (tests set the base to 0 for instant retries).
const RETRY_TIMEOUT_BASE: Duration = Duration::from_secs(10);
const RETRY_TIMEOUT_MAX: Duration = Duration::from_secs(300);
const RETRY_BASE_BACKOFF: Duration = Duration::from_secs(30);
const RETRY_MAX_BACKOFF: Duration = Duration::from_secs(600);

fn retry_backoff(attempt: u32, timed_out: bool) -> Duration {
    let (base, max) = if timed_out {
        (RETRY_TIMEOUT_BASE, RETRY_TIMEOUT_MAX)
    } else {
        (RETRY_BASE_BACKOFF, RETRY_MAX_BACKOFF)
    };
    let base = std::env::var("PIR_RETRY_BASE_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(base);
    let max = std::env::var("PIR_RETRY_MAX_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(max);
    // `attempt` grows without bound (retries never give up), so clamp the
    // exponent — the cap clamps the result anyway.
    base.saturating_mul(2u32.saturating_pow(attempt.min(16))).min(max)
}

/// Live progress of a retry wait, reported to `chat`'s `on_retry` callback
/// roughly once per second (plus once at wait start and once at wait end).
/// The UI renders the countdown from this; the provider itself stays
/// UI-agnostic.
#[derive(Debug, Clone, Copy)]
pub struct RetryWait {
    /// 1-based number of the attempt that just failed.
    pub attempt: u32,
    /// Total wait before the next attempt.
    pub total: Duration,
    /// Time left. `ZERO` means the wait is over and the next attempt fires
    /// now (the UI should erase the countdown line).
    pub remaining: Duration,
}

/// Apply pi-compatible thinking controls to an OpenAI Chat Completions
/// request body (`openai-completions.js` parity). The per-model
/// `thinkingFormat` decides the shape and `thinkingLevelMap` maps the level:
///
/// - non-reasoning models (catalog metadata present, `reasoning == false`)
///   get no thinking params at all;
/// - `deepseek`: a `thinking: {type: enabled/disabled}` toggle plus a mapped
///   `reasoning_effort` (the toggle is what actually enables thinking —
///   sending a bare effort is the slow-thinking bug this fixes);
/// - `openrouter`: a nested `reasoning: {effort}` object;
/// - `qwen`: `enable_thinking` plus a mapped `reasoning_effort`;
/// - `qwen-chat-template`: `chat_template_kwargs` with `enable_thinking` +
///   `preserve_thinking`;
/// - default (`openai`, missing, or anything unrecognized): plain mapped
///   `reasoning_effort` (legacy pir behavior when no catalog metadata is
///   available, i.e. `model_meta == None`).
///
/// An explicit `thinkingLevelMap` string always wins; otherwise pir's default
/// effort names apply (`ThinkingLevel::oai_effort`). When thinking is off, an
/// effort is only sent where pi sends one (explicit `off` strings and the
/// OpenRouter/Responses `"none"` fallback).
fn apply_openai_thinking(
    body: &mut Map<String, Value>,
    thinking: crate::config::ThinkingLevel,
    model_meta: Option<&crate::config::Model>,
) {
    use crate::config::ThinkingLevel;
    let Some(m) = model_meta else {
        // No catalog metadata (tests, legacy stores): legacy behavior.
        if let Some(effort) = thinking.oai_effort() {
            body.insert("reasoning_effort".into(), json!(effort));
        }
        return;
    };
    if !m.reasoning {
        return; // pi sends no thinking params for non-reasoning models.
    }
    let enabled = thinking != ThinkingLevel::Off;
    let allow_effort = m.supports_effort();
    let effort = m.mapped_effort(thinking);
    match m.thinking_format_name() {
        "deepseek" => {
            if enabled {
                body.insert("thinking".into(), json!({ "type": "enabled" }));
            } else if !m.off_is_null() {
                body.insert("thinking".into(), json!({ "type": "disabled" }));
            }
            if enabled && allow_effort && let Some(e) = effort {
                body.insert("reasoning_effort".into(), json!(e));
            }
        }
        "openrouter" => {
            if enabled {
                if allow_effort && let Some(e) = effort {
                    body.insert("reasoning".into(), json!({ "effort": e }));
                }
            } else if !m.off_is_null() {
                let off = m
                    .thinking_level_map
                    .get("off")
                    .and_then(|v| v.clone())
                    .unwrap_or_else(|| "none".to_string());
                body.insert("reasoning".into(), json!({ "effort": off }));
            }
        }
        "qwen" => {
            body.insert("enable_thinking".into(), json!(enabled));
            if enabled && allow_effort && let Some(e) = effort {
                body.insert("reasoning_effort".into(), json!(e));
            }
        }
        "qwen-chat-template" => {
            body.insert(
                "chat_template_kwargs".into(),
                json!({ "enable_thinking": enabled, "preserve_thinking": true }),
            );
        }
        _ => {
            if enabled && allow_effort && let Some(e) = effort {
                body.insert("reasoning_effort".into(), json!(e));
            } else if !enabled && allow_effort {
                // pi only sends an off value when it is an explicit string
                // (e.g. `"none"`); `mapped_effort(Off)` is exactly that.
                if let Some(e) = effort {
                    body.insert("reasoning_effort".into(), json!(e));
                }
            }
        }
    }
}

pub struct Client {
    kind: ApiKind,
    base_url: String,
    api_key: String,
    /// Shared HTTP client with a connection pool. Built once in `new` and
    /// reused across `chat`/`complete` calls so successive turns reuse
    /// TCP/TLS connections (keep-alive) instead of paying connect + TLS
    /// handshake on every turn. Previously a fresh `HttpClient` was built per
    /// attempt, which threw the pool away after each response.
    http: isahc::HttpClient,
    /// Blocking client for the `Ureq` backend (built once; `ureq::Agent` is
    /// cheap to clone per attempt). Only used when `backend` is `Ureq`.
    ureq_agent: ureq::Agent,
    /// Selected transport; `Isahc` unless `set_backend` says otherwise.
    backend: HttpBackend,
    /// Stable per-conversation session id, sent as `x-opencode-session` on
    /// OpenCode Go requests (routing + prompt caching; see
    /// opencode.ai/docs/go). Set by `make_client` for the `opencode-go`
    /// provider only — `None` everywhere else, so no other provider ever sees
    /// the header. `OPENCODE_SESSION_ID` overrides the generated value.
    session_id: Option<String>,
    /// Offline scripted model (see `crate::fake`): when set, `chat`/`complete`
    /// synthesize turns locally and never touch the network. Wired by
    /// `make_client` for the `fake` test provider only.
    fake: bool,
    /// Shared cancellation flag. When set (e.g. by the REPL on Ctrl-C/Ctrl-D),
    /// an in-flight streaming response aborts at its next poll boundary instead
    /// of blocking until the whole model reply is received.
    cancel: Arc<AtomicBool>,
}

impl Client {
    /// Build the shared `isahc` client (connection pool + DNS cache live here).
    /// Only the connect timeout is baked in: reads are unbounded at the socket
    /// level and bounded instead by the streaming stall watchdog + cancel flag,
    /// so a slow/"thinking" provider is never cut off mid-stream by curl.
    /// Automatic decompression is OFF: enabling it advertises
    /// `Accept-Encoding: deflate, gzip`, and some proxies buffer compressed
    /// SSE streams (killing live granularity — the byte-identical body that
    /// streamed in 9.7s via curl dribbled for minutes through pir). Like curl,
    /// we ask for identity and read exactly what the server sends.
    fn build_http_client() -> isahc::HttpClient {
        isahc::HttpClient::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .automatic_decompression(false)
            .build()
            .expect("isahc client build failed")
    }

    /// Build the blocking `ureq` client for the `Ureq` backend. Read timeout
    /// mirrors the stall watchdog (it only ever fires on true silence, which
    /// the watchdog reports first); cancel promptness comes from the
    /// parser-side cancel race, not the socket.
    fn build_ureq_agent() -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(stall_timeout())
            .timeout_write(CONNECT_TIMEOUT)
            .user_agent(&Self::user_agent())
            .build()
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
            http: Self::build_http_client(),
            ureq_agent: Self::build_ureq_agent(),
            backend: HttpBackend::default(),
            session_id: None,
            fake: false,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Select the HTTP transport for the streaming core (`Isahc` default).
    /// Called by `make_client` from `PIR_HTTP_BACKEND` / `http_backend`.
    pub fn set_backend(&mut self, backend: HttpBackend) {
        self.backend = backend;
    }

    /// Attach the conversation's session id for `x-opencode-session` routing.
    /// Called by `make_client` for providers that want it; ignored elsewhere.
    pub fn set_session_id(&mut self, id: Option<String>) {
        self.session_id = id.filter(|s| !s.is_empty());
    }

    /// pir's User-Agent: the vendor (opencode.ai/docs/go) asks clients to
    /// identify with their own agent name rather than a generic SDK/HTTP
    /// one, and uses it for routing/abuse decisions. Sent on every request.
    fn user_agent() -> String {
        format!("pir/{}", env!("CARGO_PKG_VERSION"))
    }

    /// Apply auth + session + identity headers for `kind` to a request
    /// builder. Shared by `chat` and `complete` so the two can never drift
    /// (a missing auth header on one path used to be a whole bug class).
    /// Auth + session + identity headers for `kind` as plain pairs — the
    /// single source both transports apply, so the two backends can never
    /// drift (a missing auth header on one path used to be a whole bug class).
    fn request_headers(&self, kind: ApiKind) -> Vec<(String, String)> {
        let mut out = vec![("user-agent".to_string(), Self::user_agent())];
        match kind {
            ApiKind::Anthropic => {
                out.push(("x-api-key".to_string(), self.api_key.clone()));
                out.push(("anthropic-version".to_string(), "2023-06-01".to_string()));
            }
            ApiKind::OpenAi | ApiKind::OpenAiResponses => {
                out.push(("Authorization".to_string(), format!("Bearer {}", self.api_key)));
            }
        }
        if let Some(id) = &self.session_id {
            out.push(("x-opencode-session".to_string(), id.clone()));
        }
        out
    }

    fn apply_headers(
        &self,
        mut builder: isahc::http::request::Builder,
        kind: ApiKind,
    ) -> isahc::http::request::Builder {
        for (k, v) in self.request_headers(kind) {
            builder = builder.header(&k, &v);
        }
        builder
    }

    /// Apply the shared headers to a blocking `ureq` request.
    fn apply_ureq_headers(&self, mut req: ureq::Request, kind: ApiKind) -> ureq::Request {
        for (k, v) in self.request_headers(kind) {
            req = req.set(&k, &v);
        }
        req
    }

    /// Enable the offline scripted model (see `crate::fake`).
    pub fn set_fake(&mut self, fake: bool) {
        self.fake = fake;
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
        if self.fake {
            let _ = (model, system, prompt);
            return Ok("fake ok".to_string());
        }
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
            ApiKind::OpenAiResponses => {
                let mut url = self.base_url.trim_end_matches('/').to_string();
                if !url.ends_with("/responses") && !url.contains('?') {
                    url.push_str("/responses");
                }
                (
                    url,
                    json!({
                        "model": model,
                        "input": [
                            { "role": "system", "content": system },
                            { "role": "user", "content": prompt },
                        ],
                        "stream": false,
                    }),
                )
            }
        };
        smol::block_on(async {
            let v: Value = match self.backend {
                HttpBackend::Isahc => {
                    let builder = self.apply_headers(
                        isahc::Request::builder()
                            .method("POST")
                            .uri(&url)
                            .header("content-type", "application/json"),
                        kind,
                    );
                    let req = builder.body(body.to_string()).map_err(|e| format!("complete: {e}"))?;
                    let resp = self.http.send_async(req).await.map_err(http_error)?;
                    let status = resp.status();
                    if !status.is_success() {
                        let code = status.as_u16();
                        let mut b = String::new();
                        let _ = resp.into_body().read_to_string(&mut b).await;
                        return Err(http_status_detail(code, &b));
                    }
                    let mut b = String::new();
                    let _ = resp.into_body().read_to_string(&mut b).await;
                    serde_json::from_str(&b).map_err(|e| format!("complete: {e}"))?
                }
                HttpBackend::Ureq => {
                    // One blocking round trip on a worker thread; the whole
                    // body arrives at once (no streaming here by design).
                    let agent = self.ureq_agent.clone();
                    let url = url.clone();
                    let headers = self.request_headers(kind);
                    let body_str = body.to_string();
                    let txt = smol::unblock(move || {
                        let mut req = agent.post(&url);
                        for (k, v) in &headers {
                            req = req.set(k, v);
                        }
                        match req.set("content-type", "application/json").send_string(&body_str) {
                            Ok(resp) => resp.into_string().map_err(|e| format!("complete: {e}")),
                            Err(e) => Err(ureq_error(e)),
                        }
                    })
                    .await?;
                    serde_json::from_str(&txt).map_err(|e| format!("complete: {e}"))?
                }
            };
            let raw = match kind {
                ApiKind::Anthropic => v
                    .pointer("/content/0/text")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                ApiKind::OpenAi => v
                    .pointer("/choices/0/message/content")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                // Non-streaming Responses object: first message item's first
                // output-text part. Missing/empty shapes read as no opinion.
                ApiKind::OpenAiResponses => v
                    .get("output")
                    .and_then(Value::as_array)
                    .and_then(|items| {
                        items.iter().find_map(|it| {
                            (it.get("type").and_then(Value::as_str) == Some("message"))
                                .then(|| {
                                    it.get("content").and_then(Value::as_array).and_then(|parts| {
                                        parts.iter().find_map(|p| {
                                            (p.get("type").and_then(Value::as_str)
                                                == Some("output_text"))
                                            .then(|| p.get("text").and_then(Value::as_str))
                                            .flatten()
                                        })
                                    })
                                })
                                .flatten()
                        })
                    })
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
        // Per-model catalog metadata for thinking controls (`reasoning`,
        // `compat.thinkingFormat`, `thinkingLevelMap`). `None` keeps the
        // legacy raw-`reasoning_effort` behavior (tests); the agent passes
        // `Some(&self.model)`.
        model_meta: Option<&crate::config::Model>,
        // Retry-wait progress, called ~1/sec while backing off between
        // attempts (plus once at wait start/end) so the UI can show a live
        // "retrying in Ns" countdown. Never touches the transcript.
        on_retry: &mut dyn FnMut(&RetryWait),
        // Retry/failure notices (attempt failed, reconnected). Called instead
        // of `on_text` so notices stay out of the model text, the markdown
        // renderer, and the loop detector; the caller displays them and logs
        // them as transcript-only entries.
        on_notice: &mut dyn FnMut(&str),
    ) -> Result<(Message, Usage), String> {
        // Offline scripted model: synthesize locally, never touch the network.
        if self.fake {
            return crate::fake::fake_chat(history, &mut *on_text, &mut *on_think, &self.cancel);
        }
        // Repair orphaned tool blocks (trim cuts, resume folds, skipped calls)
        // so a bookkeeping glitch can't 400 the whole turn. Everything below
        // (request builders + context estimate) uses the cleaned sequence.
        let clean = sanitize_history(history);
        let history: &[Message] = &clean;
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
                    model_meta,
                ),
                None => self.openai_request(model, max_tokens, system, history, tools, thinking, model_meta),
            },
            ApiKind::OpenAiResponses => match url_override {
                Some(u) => self.responses_request_at(
                    u,
                    model,
                    max_tokens,
                    system,
                    history,
                    tools,
                    thinking,
                    allow_reasoning_effort,
                    model_meta,
                ),
                None => self.responses_request(
                    model,
                    max_tokens,
                    system,
                    history,
                    tools,
                    thinking,
                    allow_reasoning_effort,
                    model_meta,
                ),
            },
        };
        // Byte-identical replay support for speed diagnosis (see fn docs).
        maybe_dump_payload(&body);
        // Retry the whole request (connect + stream parse) on transient
        // pre-output errors, **forever**: once the user is seeing streaming
        // output (`emitted_text`/`saw_tool_calls`) we NEVER re-run the attempt
        // (and risk duplicating already-printed text) — a mid-stream failure
        // is surfaced as a hard error instead. A cancel request aborts the
        // whole loop immediately (no retry) via `self.cancel`.
        let cancel = self.cancel.clone();
        let cancel_rx = self.start_cancel_forwarder();
        let mut emitted_text = false;
        // True once the stream has delivered at least one tool_use block. A
        // mid-stream crash after partial tool output is *not* safe to transparently
        // retry: re-sending the whole request would lose the tool results already
        // produced and can duplicate work. Treat partial tool progress like
        // partial text — surface the error rather than replaying.
        let mut saw_tool_calls = false;
        // Timing telemetry for `PIR_DEBUG` (stderr, never the transcript):
        // prompt bytes once (the payload is rebuilt per attempt from `body`),
        // per-attempt spans below. Settles "is pir or the server slow?"
        // with numbers: big `headers_ms` = network/queueing before the
        // server answered; low tok/s = slow generation mid-stream.
        let debug = std::env::var_os("PIR_DEBUG").is_some();
        let body_bytes = body.to_string().len();
        let mut attempt: u32 = 0;
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err("request cancelled".to_string());
            }
            let send_start = Instant::now();
            // Build the request (JSON body serialised directly). The
            // shared isahc sockets are unbounded: reads are bounded by the
            // streaming stall watchdog, not a per-attempt socket timeout.
            // Race the connect/status-line against the cancel channel so a cancel
            // pressed while still waiting on the socket is honoured instantly:
            // the smol reactor wakes the moment `cancel_tx` fires, no polling.
            // Both backends converge here: a boxed async buffered reader the
            // shared SSE parsers consume, so transport never leaks downstream.
            enum ConnectOutcome {
                Body(Pin<Box<dyn AsyncBufRead + Unpin + Send>>),
                Cancelled,
                Err(String),
            }
            let outcome = match self.backend {
                HttpBackend::Isahc => {
                    let builder = self.apply_headers(
                        isahc::Request::builder()
                            .method("POST")
                            .uri(&url)
                            .header("content-type", "application/json"),
                        kind,
                    );
                    let req = match builder.body(body.to_string()) {
                        Ok(r) => r,
                        Err(e) => return Err(format!("http body: {e}")),
                    };
                    future::or(
                        async {
                            match self.http.send_async(req).await {
                                Ok(r) => {
                                    if !r.status().is_success() {
                                        let code = r.status().as_u16();
                                        let mut body_txt = String::new();
                                        let _ = r.into_body().read_to_string(&mut body_txt).await;
                                        ConnectOutcome::Err(http_status_detail(code, &body_txt))
                                    } else {
                                        let reader: Pin<Box<dyn AsyncBufRead + Unpin + Send>> =
                                            Box::pin(smol::io::BufReader::new(Box::pin(r.into_body())));
                                        ConnectOutcome::Body(reader)
                                    }
                                }
                                Err(e) => ConnectOutcome::Err(http_error(e)),
                            }
                        },
                        async {
                            let _ = cancel_rx.recv().await;
                            ConnectOutcome::Cancelled
                        },
                    )
                    .await
                }
                HttpBackend::Ureq => {
                    // Blocking client on a worker thread; the pump thread
                    // behind the channel feeds the same async parsers. A lost
                    // connect race detaches cleanly: the receiver is dropped
                    // and the pump's bounded send fails fast.
                    let agent = self.ureq_agent.clone();
                    let url = url.clone();
                    let headers = self.request_headers(kind);
                    let body_str = body.to_string();
                    future::or(
                        async {
                            match smol::unblock(move || ureq_send(&agent, &url, &headers, &body_str)).await
                            {
                                Ok(pump) => {
                                    let reader: Pin<Box<dyn AsyncBufRead + Unpin + Send>> =
                                        Box::pin(pump);
                                    ConnectOutcome::Body(reader)
                                }
                                Err(e) => ConnectOutcome::Err(e),
                            }
                        },
                        async {
                            let _ = cancel_rx.recv().await;
                            ConnectOutcome::Cancelled
                        },
                    )
                    .await
                }
            };
            // All three failure sources — a transport error, a non-2xx
            // status (isahc returns Ok for any HTTP status), or a stream
            // parse failure — fold into the attempt `result` below so every
            // one goes through the same retry decision. Direct `return`s here
            // used to make the `is_retryable` 429/5xx/transport arms dead
            // letters (a 500/connection-refused ended the turn instantly).
            let headers_ms = send_start.elapsed();
            // Phase-timing flags live here (visible to the `Ok` arm below);
            // the wrapping callbacks that set them live in the Resp branch.
            let mut first_think_ms: Option<u128> = None;
            let mut first_text_ms: Option<u128> = None;
            let result: Result<(Message, Usage), String> = match outcome {
                ConnectOutcome::Cancelled => return Err("request cancelled".to_string()),
                ConnectOutcome::Err(e) => Err(e),
                ConnectOutcome::Body(mut reader) => {
                    // Phase timing for PIR_DEBUG: stamp the first reasoning
                    // and first text token instants (relative to this
                    // attempt's send), distinguishing "thinking dribbled"
                    // from "content dribbled". The wrapped callbacks forward
                    // everything untouched; only the first-nonempty instants
                    // are recorded.
                    let mut on_think_wrap = |t: &str| {
                        if !t.is_empty() && first_think_ms.is_none() {
                            first_think_ms = Some(send_start.elapsed().as_millis());
                        }
                        on_think(t);
                    };
                    let mut on_text_wrap = |t: &str| {
                        if !t.is_empty() && first_text_ms.is_none() {
                            first_text_ms = Some(send_start.elapsed().as_millis());
                        }
                        on_text(t);
                    };
                    match kind {
                        ApiKind::Anthropic => {
                            stream_anthropic(
                                &mut reader,
                                &mut on_text_wrap,
                                &mut emitted_text,
                                &mut saw_tool_calls,
                                &cancel,
                                &cancel_rx,
                                &mut on_think_wrap,
                            )
                            .await
                        }
                        ApiKind::OpenAi => {
                            stream_openai(
                                &mut reader,
                                &mut on_text_wrap,
                                &mut emitted_text,
                                &mut saw_tool_calls,
                                &cancel,
                                &cancel_rx,
                                &mut on_think_wrap,
                            )
                            .await
                        }
                        ApiKind::OpenAiResponses => {
                            stream_responses(
                                &mut reader,
                                &mut on_text_wrap,
                                &mut emitted_text,
                                &mut saw_tool_calls,
                                &cancel,
                                &cancel_rx,
                                &mut on_think_wrap,
                            )
                            .await
                        }
                    }
                }
            };
            match result {
                Ok(r) => {
                    if debug {
                        debug_log(&chat_debug_line(
                                attempt + 1,
                                body_bytes,
                                headers_ms,
                                send_start.elapsed().saturating_sub(headers_ms),
                                r.1.output,
                                first_think_ms,
                                first_text_ms,
                            ));
                    }
                    if attempt > 0 {
                        on_notice(&format!(
                            "\n✓ reconnected on attempt {} — continuing\n",
                            attempt + 1
                        ));
                    }
                    return Ok(r);
                }
                Err(e) => {
                    if e == "request cancelled" {
                        return Err(e);
                    }
                    // Mid-stream failures after visible output stay fatal:
                    // re-issuing would duplicate already-printed text. A
                    // pre-output stall (no bytes yet) retries forever with
                    // exponential backoff like any transient failure.
                    if !is_retryable(&e) || emitted_text || saw_tool_calls {
                        return Err(e);
                    }
                    // Retryable and nothing shown yet: try again, forever.
                    // (Tests/scripts can cap total attempts via PIR_MAX_ATTEMPTS.)
                    let failed_attempt = attempt + 1;
                    if let Some(max) = max_attempts()
                        && u64::from(failed_attempt) >= max {
                            return Err(format!(
                                "gave up after {failed_attempt} attempts (PIR_MAX_ATTEMPTS={max}): {e}"
                            ));
                        }
                    // The wait doubles each attempt (see `retry_backoff`) so a
                    // struggling server gets progressively more room.
                    let timed_out = is_timeout(&e);
                    let backoff = retry_backoff(attempt, timed_out);
                    on_notice(&format!(
                        "\n\u{26a0} request failed (attempt {failed_attempt}), retrying in {:.0?} — keeps retrying until it succeeds (Ctrl-C to stop): {e}\n",
                        backoff
                    ));
                    // Report the wait so the UI can show a live countdown:
                    // once now, ~1/sec while waiting, once at the end with
                    // `remaining == ZERO` (the UI erases the line on that).
                    let mut report = |waited: Duration| {
                        on_retry(&RetryWait {
                            attempt: failed_attempt,
                            total: backoff,
                            remaining: backoff.saturating_sub(waited),
                        });
                    };
                    report(Duration::ZERO);
                    // Await the backoff in slices, raced against the cancel
                    // channel so a cancel mid-backoff is honoured instantly.
                    let mut waited = Duration::ZERO;
                    let mut last_secs = backoff.as_secs();
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
                        // Tick the countdown on whole-second changes only, so
                        // the UI redraws ~1/sec instead of 10/sec.
                        let secs = backoff.saturating_sub(waited).as_secs();
                        if secs != last_secs {
                            last_secs = secs;
                            report(waited);
                        }
                    }
                    report(backoff);
                    attempt += 1;
                }
            }
        }
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

    /// `model_meta` carries the catalog thinking metadata; see its doc.
    #[allow(clippy::too_many_arguments)]
    fn openai_request(
        &self,
        model: &str,
        max_tokens: u64,
        system: &str,
        history: &[Message],
        tools: &[ToolSpec],
        thinking: crate::config::ThinkingLevel,
        // Per-model catalog metadata (`reasoning`, `compat.thinkingFormat`,
        // `compat.supportsReasoningEffort`, `thinkingLevelMap`). `None` keeps
        // the legacy behavior (raw `reasoning_effort`); `Some` with
        // `reasoning == false` sends no thinking params at all (pi parity).
        model_meta: Option<&crate::config::Model>,
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
        // Thinking controls (pi `openai-completions` parity — see
        // `apply_openai_thinking`): the per-model `thinkingFormat` decides the
        // shape (`thinking` toggle, nested `reasoning`, `enable_thinking`, or
        // plain `reasoning_effort`) and `thinkingLevelMap` maps the level.
        apply_openai_thinking(&mut body, thinking, model_meta);
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
        model_meta: Option<&crate::config::Model>,
    ) -> (String, Value) {
        let (base, mut body) = self.openai_request(model, max_tokens, system, history, tools, thinking, model_meta);
        let _ = base;
        if !allow_effort {
            let obj = body.as_object_mut().unwrap();
            // The model rejects effort fields: drop both the plain and the
            // OpenRouter-nested shapes. The `thinking` toggle / language-model
            // `enable_thinking` switches are separate concerns and stay.
            obj.remove("reasoning_effort");
            obj.remove("reasoning");
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

    /// OpenAI Responses API request (`POST {base}/responses`). Models whose
    /// catalog entry says `openai-responses` live here (opencode-go's
    /// muse-spark/grok/gpt-5.6-luna); chat-style bodies 500 on them.
    /// `input` is the flat item list (system first, then converted history),
    /// output budget rides `max_output_tokens`, and thinking maps to the
    /// `reasoning.effort` object. `allow_effort == false` drops it (models
    /// that reject the field).
    #[allow(clippy::too_many_arguments)]
    fn responses_request(
        &self,
        model: &str,
        max_tokens: u64,
        system: &str,
        history: &[Message],
        tools: &[ToolSpec],
        thinking: crate::config::ThinkingLevel,
        allow_effort: bool,
        model_meta: Option<&crate::config::Model>,
    ) -> (String, Value) {
        let mut input = vec![json!({ "role": "system", "content": system })];
        for m in history.iter().filter(|m| !m.is_empty()) {
            input.extend(responses_input(m));
        }
        let mut body = Map::new();
        body.insert("model".into(), json!(model));
        body.insert("input".into(), Value::Array(input));
        body.insert("stream".into(), json!(true));
        body.insert("max_output_tokens".into(), json!(max_tokens));
        body.insert(
            "tools".into(),
            Value::Array(tools.iter().map(|t| json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.schema,
            })).collect()),
        );
        if allow_effort {
            // pi `openai-responses` parity: the effort rides
            // `reasoning.effort`, mapped through `thinkingLevelMap` (`off`
            // falls back to `"none"` unless explicitly null). Models without
            // catalog metadata keep the legacy raw-effort behavior.
            let effort: Option<String> = match model_meta {
                None => thinking.oai_effort().map(str::to_string),
                Some(m) if !m.reasoning || !m.supports_effort() => None,
                Some(m) if thinking.enabled() => m.mapped_effort(thinking),
                Some(m) if !m.off_is_null() => Some(
                    m.thinking_level_map
                        .get("off")
                        .and_then(|v| v.clone())
                        .unwrap_or_else(|| "none".to_string()),
                ),
                Some(_) => None,
            };
            if let Some(e) = effort {
                body.insert("reasoning".into(), json!({ "effort": e }));
            }
        }
        (format!("{}/responses", self.base_url), Value::Object(body))
    }

    /// [`Self::responses_request`] against an explicit base URL (per-model
    /// override, same convention as [`Self::openai_request_at`]).
    #[allow(clippy::too_many_arguments)]
    fn responses_request_at(
        &self,
        url: &str,
        model: &str,
        max_tokens: u64,
        system: &str,
        history: &[Message],
        tools: &[ToolSpec],
        thinking: crate::config::ThinkingLevel,
        allow_effort: bool,
        model_meta: Option<&crate::config::Model>,
    ) -> (String, Value) {
        let (base, body) = self.responses_request(
            model, max_tokens, system, history, tools, thinking, allow_effort, model_meta,
        );
        let _ = base;
        let mut url = url.trim_end_matches('/').to_string();
        if !url.ends_with("/responses") && !url.contains('?') {
            url.push_str("/responses");
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
/// failures (DNS, connection refused, TLS, timeouts, I/O), pre-output
/// stalled streams (peer silent before any bytes — a fresh attempt may
/// connect cleanly), and on transient
/// HTTP status codes (429 rate-limit, 500/502/503/504 server errors). Do
/// NOT retry on 4xx client errors other than 429 (e.g. 401 unauthorized,
/// 400 bad request) — those won't succeed on replay.
fn is_retryable(error: &str) -> bool {
    // Cancellation is never worth replaying — handled as fatal by the caller.
    // A stalled stream is retryable here; the caller decides via
    // emitted_text/saw_tool_calls (pre-output stalls retry forever with
    // backoff, mid-stream stalls after visible output stay fatal to avoid
    // duplicating already-printed text).
    if error == "request cancelled" {
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

/// Map a `ureq` failure to the same strings the isahc path produces, so
/// retries, stall detection, and REPL messages behave identically whichever
/// backend served the attempt. Transport Display already contains timeout
/// wording ("timed out") that `is_timeout` matches for fast retries.
fn ureq_error(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            let mut txt = String::new();
            let _ = resp.into_reader().read_to_string(&mut txt);
            http_status_detail(code, &txt)
        }
        e => e.to_string(),
    }
}

/// `AsyncBufRead` over a background thread pumping a blocking (ureq) body.
/// Chunks travel over a bounded channel (backpressure included); a transport
/// error is sticky and surfaces at EOF, so a mid-stream cut is never
/// mistaken for a clean end the way a bare channel close would be.
struct ChannelBody {
    rx: Pin<Box<smol::channel::Receiver<std::io::Result<Vec<u8>>>>>,
    buf: Vec<u8>,
    pos: usize,
    failed: Option<String>,
}

impl ChannelBody {
    /// Spawn the pump thread for `reader` (a blocking ureq response body)
    /// and return the async adapter. The thread exits when the body ends,
    /// errors, or the receiver is dropped (a bounded send then fails fast).
    fn pump<R: std::io::Read + Send + 'static>(reader: R) -> Self {
        let (tx, rx) = smol::channel::bounded::<std::io::Result<Vec<u8>>>(8);
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut chunk = vec![0u8; 8192];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send_blocking(Ok(chunk[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send_blocking(Err(e));
                        break;
                    }
                }
            }
        });
        ChannelBody { rx: Box::pin(rx), buf: Vec::new(), pos: 0, failed: None }
    }

    /// Fill `buf` from the channel; `Ok(true)` means bytes are available.
    fn poll_fill(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<bool>> {
        use smol::stream::Stream as _;
        if self.pos < self.buf.len() {
            return Poll::Ready(Ok(true));
        }
        match self.rx.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                self.buf = chunk;
                self.pos = 0;
                Poll::Ready(Ok(true))
            }
            Poll::Ready(Some(Err(e))) => {
                self.failed = Some(e.to_string());
                Poll::Ready(Ok(false))
            }
            Poll::Ready(None) => Poll::Ready(Ok(false)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl smol::io::AsyncRead for ChannelBody {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.failed.is_some() {
            let msg = self.failed.clone().unwrap_or_default();
            return Poll::Ready(Err(std::io::Error::other(msg)));
        }
        match self.poll_fill(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(false)) => match self.failed.take() {
                Some(msg) => Poll::Ready(Err(std::io::Error::other(msg))),
                None => Poll::Ready(Ok(0)),
            },
            Poll::Ready(Ok(true)) => {
                let n = (self.buf.len() - self.pos).min(out.len());
                out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
                self.pos += n;
                if self.pos >= self.buf.len() {
                    self.buf.clear();
                    self.pos = 0;
                }
                Poll::Ready(Ok(n))
            }
        }
    }
}

impl smol::io::AsyncBufRead for ChannelBody {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<&[u8]>> {
        let this = self.get_mut();
        match this.poll_fill(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(_)) => {
                if this.pos < this.buf.len() {
                    Poll::Ready(Ok(&this.buf[this.pos..]))
                } else if let Some(msg) = this.failed.take() {
                    Poll::Ready(Err(std::io::Error::other(msg)))
                } else {
                    Poll::Ready(Ok(&[]))
                }
            }
        }
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        let this = self.get_mut();
        this.pos = (this.pos + amt).min(this.buf.len());
        if this.pos >= this.buf.len() {
            this.buf.clear();
            this.pos = 0;
        }
    }
}

/// Blocking ureq POST for the `Ureq` backend. Runs inside `smol::unblock`
/// (never on the executor). On 2xx spawns the pump thread and returns its
/// channel; anything else becomes the same strings the isahc path produces.
fn ureq_send(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    body: &str,
) -> Result<ChannelBody, String> {
    let mut req = agent.post(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let req = req.set("content-type", "application/json");
    match req.send_string(body) {
        Ok(resp) => Ok(ChannelBody::pump(resp.into_reader())),
        Err(e) => Err(ureq_error(e)),
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
/// Repair orphaned tool blocks before serializing a request.
///
/// Several paths can leave `history` with tool calls that have no results or
/// results that have no call: context `trim` cutting mid-turn, session resume
/// folding assistant blocks into a user message, or a preflight `terminate`
/// skipping the remaining calls. Either shape makes the provider reject the
/// whole request with HTTP 400 (`role 'tool' must be a response to a
/// preceding message with 'tool_calls'` / `tool_calls must be followed by
/// tool messages`), killing the turn for want of bookkeeping.
///
/// This rewrites the sequence into valid form, preserving order and content:
/// - `ToolUse` blocks found inside a *user* message are lifted into their own
///   preceding assistant message (the resume-fold artifact).
/// - `ToolResult`s with no matching open call become plain text (content kept,
///   so nothing the tools said is silently lost).
/// - calls left without results gain a synthetic error result, so no
///   `tool_calls` dangles.
/// - result blocks are emitted immediately after their assistant message
///   (ahead of any user text from the same message), keeping the
///   assistant → tool adjacency strict providers require.
///
/// Clean histories pass through with the same messages and blocks.
/// Emit a `PIR_DEBUG` diagnostics line. `PIR_DEBUG=1` (or `true`) goes to
/// stderr; any other value is treated as a file path to append to. The file
/// form exists because stderr bypasses the spinner's screen protocol, so a
/// footer repaint can wipe a freshly printed line off the display before it
/// is read — a file never loses it.
fn debug_log(line: &str) {
    let val = match std::env::var_os("PIR_DEBUG") {
        None => return,
        Some(v) => v,
    };
    if val == "1" || val == "true" {
        eprintln!("{line}");
        return;
    }
    let path = std::path::PathBuf::from(&val);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write;
        let _ = writeln!(f, "{line}");
    } else {
        // Unwritable path: fall back to stderr rather than losing it silently.
        eprintln!("{line}");
    }
}

/// Per-SSE-event arrival log for `PIR_DEBUG_CHUNKS=<path>`: one line per
/// handled data event, `"<ms-since-stream-start> <kind> <bytes>"`, where kind
/// is `text` / `reasoning` / `tool` / `usage` / `error` / `other`. No content
/// is ever recorded (privacy + size). Lets a slow turn's gap structure be
/// read off directly: dribble (even spacing) vs stall-then-burst (one huge
/// gap) vs phase split (gaps only in one kind). Silent unless the env var is
/// set; all writes best-effort. Construct per parser invocation with
/// [`ChunkLog::open`] (path resolved once per process).
struct ChunkLog {
    w: Option<std::io::BufWriter<std::fs::File>>,
    t0: Instant,
}

fn chunk_log_path() -> Option<std::path::PathBuf> {
    static PATH: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        std::env::var_os("PIR_DEBUG_CHUNKS")
            .map(std::path::PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
    })
    .clone()
}

impl ChunkLog {
    fn open() -> Self {
        let w = chunk_log_path().and_then(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
                .ok()
                .map(std::io::BufWriter::new)
        });
        ChunkLog { w, t0: Instant::now() }
    }

    /// Test/diagnostic constructor with an explicit path (no env involved).
    #[cfg(test)]
    fn open_at(path: &std::path::Path) -> Self {
        let w = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
            .map(std::io::BufWriter::new);
        ChunkLog { w, t0: Instant::now() }
    }

    fn event(&mut self, kind: &str, bytes: usize) {
        if let Some(w) = self.w.as_mut() {
            let _ = writeln!(w, "{} {kind} {bytes}", self.t0.elapsed().as_millis());
            // Flush every event: a few thousand syscalls per turn is nothing,
            // and the log stays readable live (`tail -f`) and after a kill.
            let _ = w.flush();
        }
    }
}

/// One-line timing summary for `PIR_DEBUG` (see [`debug_log`]: stderr or a
/// file, never the transcript). `think_ms`/`text_ms` are the first-reasoning
/// / first-content instants (None when that kind never arrived) — they split
/// a slow turn into "thinking dribbled" vs "content dribbled". Pure for tests.
fn chat_debug_line(
    attempt: u32,
    prompt_bytes: usize,
    headers: Duration,
    stream: Duration,
    out_tokens: u64,
    think_ms: Option<u128>,
    text_ms: Option<u128>,
) -> String {
    let secs = stream.as_secs_f64().max(0.001);
    let phase = |v: Option<u128>| v.map(|m| m.to_string()).unwrap_or_else(|| "-".to_string());
    format!(
        "pir-debug chat: attempt={} prompt_bytes={} headers_ms={} stream_ms={} out_tokens={} ({:.1} tok/s) think_first_ms={} text_first_ms={}",
        attempt,
        prompt_bytes,
        headers.as_millis(),
        stream.as_millis(),
        out_tokens,
        out_tokens as f64 / secs,
        phase(think_ms),
        phase(text_ms),
    )
}

/// Dump the exact request body to a file when `PIR_DEBUG_PAYLOAD=<path>` is
/// set (the header key is never part of the body, so the file is safe to
/// share/curl). Lets anyone replay pir's byte-identical request through
/// another client to isolate client-vs-provider speed differences.
fn maybe_dump_payload(body: &Value) {
    let Some(path) = std::env::var_os("PIR_DEBUG_PAYLOAD") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let _ = std::fs::write(path, body.to_string());
}

fn sanitize_history(history: &[Message]) -> Vec<Message> {
    // Emit synthetic error results for still-open calls, closing them.
    fn close_open(out: &mut Vec<Message>, open: &mut Vec<String>) {
        if open.is_empty() {
            return;
        }
        let ids = std::mem::take(open);
        out.push(Message {
            role: Role::User,
            blocks: ids
                .into_iter()
                .map(|id| Block::ToolResult {
                    tool_use_id: id,
                    content: "skipped: no tool result was recorded for this call \
                        (synthesized by pir to satisfy the provider API)"
                        .to_string(),
                    is_error: true,
                })
                .collect(),
        });
    }

    let mut out: Vec<Message> = Vec::with_capacity(history.len());
    // Call ids from the latest assistant message still awaiting results.
    let mut open: Vec<String> = Vec::new();
    for m in history {
        match m.role {
            Role::Assistant => {
                // A new assistant message breaks adjacency: close any calls
                // the previous one left dangling first.
                close_open(&mut out, &mut open);
                let mut calls = Vec::new();
                for b in &m.blocks {
                    if let Block::ToolUse { id, .. } = b {
                        calls.push(id.clone());
                    }
                }
                if !m.blocks.is_empty() {
                    out.push(m.clone());
                }
                open = calls;
            }
            Role::User => {
                let mut texts = Vec::new();
                let mut uses = Vec::new();
                let mut matched = Vec::new();
                let mut orphans = Vec::new();
                for b in &m.blocks {
                    match b {
                        Block::ToolUse { .. } => uses.push(b.clone()),
                        Block::ToolResult { tool_use_id, content, .. } => {
                            if open.contains(tool_use_id) {
                                matched.push(b.clone());
                            } else {
                                orphans.push(Block::Text(format!(
                                    "[pir: unpaired tool result for call '{tool_use_id}' — \
                                    no matching tool call in history:]\n{content}"
                                )));
                            }
                        }
                        _ => texts.push(b.clone()),
                    }
                }
                if !uses.is_empty() {
                    // Resume-fold artifact: these belong to an assistant message.
                    close_open(&mut out, &mut open);
                    let ids: Vec<String> = uses
                        .iter()
                        .filter_map(|b| match b {
                            Block::ToolUse { id, .. } => Some(id.clone()),
                            _ => None,
                        })
                        .collect();
                    out.push(Message { role: Role::Assistant, blocks: uses });
                    open = ids;
                }
                if !matched.is_empty() {
                    for b in &matched {
                        if let Block::ToolResult { tool_use_id, .. } = b {
                            open.retain(|id| id != tool_use_id);
                        }
                    }
                    out.push(Message { role: Role::User, blocks: matched });
                    // A results message always carries a turn's *complete* set
                    // of results, so a partial consume means corruption: close
                    // the rest now, while still adjacent to their calls.
                    close_open(&mut out, &mut open);
                } else if !open.is_empty() {
                    // New turn content with calls still open and no results
                    // here: close them first so adjacency holds.
                    close_open(&mut out, &mut open);
                }
                texts.extend(orphans);
                if !texts.is_empty() {
                    out.push(Message { role: Role::User, blocks: texts });
                }
            }
        }
    }
    // Dangling calls at the very end (e.g. trim cut right after them).
    close_open(&mut out, &mut open);
    out
}

/// Classify one parsed SSE data payload for chunk logging. Two vocabularies
/// share this helper: Anthropic (`type` + `delta.type`) and OpenAI chat
/// (`choices[0].delta.{content,reasoning*,tool_calls}`, top-level `usage`).
/// Pure.
fn sse_kind_chat(v: &Value) -> &'static str {
    if v.get("error").is_some() {
        return "error";
    }
    // Usage-only chunks (stream_options.include_usage) carry no choices.
    if v.get("usage").is_some() {
        return "usage";
    }
    // OpenAI chat shape (it carries no `type` field at all).
    if let Some(choice) = v.get("choices").and_then(|c| c.get(0)) {
        let delta = &choice["delta"];
        if delta.get("tool_calls").is_some() {
            return "tool";
        }
        let has = |k: &str| delta.get(k).and_then(Value::as_str).map_or(false, |s| !s.is_empty());
        if has("content") {
            return "text";
        }
        if has("reasoning") || has("reasoning_content") || has("reasoning_text") {
            return "reasoning";
        }
        return "other";
    }
    match v.get("type").and_then(Value::as_str).unwrap_or("") {
        "content_block_delta" => match v.get("delta").and_then(|d| d.get("type")).and_then(Value::as_str).unwrap_or("") {
            "text_delta" => "text",
            "thinking_delta" => "reasoning",
            "input_json_delta" => "tool",
            _ => "other",
        },
        "message_delta" => "usage",
        "error" => "error",
        _ => "other",
    }
}

/// Classify a Responses event name for chunk logging. Pure.
fn sse_kind_responses(ev: &str) -> &'static str {
    match ev {
        "response.output_text.delta" => "text",
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => "reasoning",
        "response.output_item.done" => "tool",
        "response.completed" => "usage",
        "response.failed" | "error" => "error",
        _ => "other",
    }
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

/// Convert one history message to Responses API input items (flat list).
/// Text keeps role blocks; tool calls/results become `function_call` /
/// `function_call_output` items keyed by call id; thinking blocks are dropped
/// (never re-sent, like every other builder). Result blocks are emitted in
/// block order so each output has its call nearby.
fn responses_input(m: &Message) -> Vec<Value> {
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
                        "type": "function_call_output",
                        "call_id": tool_use_id,
                        "output": content,
                    }));
                }
            }
        }
        Role::Assistant => {
            let text = m.text().trim().to_string();
            if !text.is_empty() {
                out.push(json!({ "role": "assistant", "content": text }));
            }
            for b in &m.blocks {
                if let Block::ToolUse { id, name, input } = b {
                    out.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": input.to_string(),
                    }));
                }
            }
        }
    }
    out
}

fn openai_message(m: &Message) -> Vec<Value> {
    let mut out = Vec::new();
    match m.role {
        Role::User => {
            let text = m.text().trim().to_string();
            if !text.is_empty() {
                // Content-block form (pi parity: pi sends
                // `[{type:"text", text}]`, never a bare string). Valid
                // OpenAI and native to Anthropic.
                out.push(json!({ "role": "user", "content": [{ "type": "text", "text": text }] }));
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
    let mut chunks = ChunkLog::open();
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
        chunks.event(sse_kind_chat(&v), data.len());

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
    let mut chunks = ChunkLog::open();
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
        chunks.event(sse_kind_chat(&v), data.len());

        if let Some(u) = v.get("usage")
            && !u.is_null() {
                if let Some(p) = u["prompt_tokens"].as_u64() { usage.input = p; }
                if let Some(c) = u["completion_tokens"].as_u64() { usage.output = c; }
            }
        let Some(choice) = v["choices"].get(0) else { continue };
        let delta = &choice["delta"];
        if let Some(t) = delta["content"].as_str()
            && !t.is_empty() {
                *emitted_text = true;
                on_text(t);
                text.push_str(t);
            }
        // Reasoning chain-of-thought. The field name varies by provider and
        // gateway: OpenAI o-series sends `delta.reasoning`, DeepSeek-compatible
        // APIs (DeepSeek proper, Ollama Cloud) send `delta.reasoning_content`,
        // and some gateways send `delta.reasoning_text`. Forward whichever
        // arrives to `on_think` — an unrecognized field means a reasoning
        // model's whole thinking phase is silently swallowed and the user
        // stares at a bare spinner for minutes (exactly what happened with
        // deepseek-v4-flash on ollama-cloud). First non-empty match wins so
        // a gateway echoing two names can't duplicate the text.
        if let Some(t) = ["reasoning", "reasoning_content", "reasoning_text"]
            .into_iter()
            .filter_map(|key| delta[key].as_str())
            .find(|t| !t.is_empty())
        {
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
                if let Some(id) = tc["id"].as_str()
                    && !id.is_empty() { slot.1 = id.to_string(); }
                if let Some(name) = tc["function"]["name"].as_str()
                    && !name.is_empty() { slot.2 = name.to_string(); }
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

/// Stream the OpenAI Responses API (`POST {base}/responses`, SSE). Same
/// robustness contract as [`stream_openai`] (cancel race, stall watchdog,
/// `emitted_text`/`saw_tool_calls` no-retry-after-output semantics) with the
/// Responses event vocabulary: `event: <name>` lines select the handler and
/// `data:` carries the payload (a bare `"type"` field in data is accepted
/// too, for gateways that omit the event line).
///
/// - `response.output_text.delta` `{"delta"}` → streamed text.
/// - `response.reasoning_summary_text.delta` / `response.reasoning_text.delta`
///   → thinking.
/// - `response.output_item.done` with `item.type == "function_call"` → a
///   tool call (accumulated by call id, flushed at the end).
/// - `response.completed` → usage envelope; `response.failed` / `error` → Err.
#[allow(clippy::too_many_arguments)]
async fn stream_responses<R: AsyncBufRead + Unpin>(
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
    let mut calls: Vec<(String, String, String)> = Vec::new(); // (id, name, args)
    let mut event = String::new();

    let mut line = String::new();
    let mut last_byte = Instant::now();
    let mut chunks = ChunkLog::open();
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err("request cancelled".to_string());
        }
        if last_byte.elapsed() > stall_timeout() {
            return Err("stream: stalled (no data for 180s)".to_string());
        }
        line.clear();
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
        let trimmed = line.trim_end();
        if let Some(name) = trimmed.strip_prefix("event:") {
            event = name.trim().to_string();
            continue;
        }
        let Some(data) = trimmed.strip_prefix("data:") else { continue };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Event name from the `event:` line, else the payload's own type.
        let ev = if event.is_empty() {
            v.get("type").and_then(Value::as_str).unwrap_or("").to_string()
        } else {
            std::mem::take(&mut event)
        };
        chunks.event(sse_kind_responses(&ev), data.len());
        match ev.as_str() {
            "response.output_text.delta" => {
                let t = v
                    .get("delta")
                    .or_else(|| v.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !t.is_empty() {
                    *emitted_text = true;
                }
                on_text(t);
                text.push_str(t);
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let t = v
                    .get("delta")
                    .or_else(|| v.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                on_think(t);
                thinking.push_str(t);
            }
            "response.output_item.done" => {
                let item = &v["item"];
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let id = item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                    let args = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    match calls.iter_mut().find(|(i, _, _)| *i == id) {
                        Some(slot) => {
                            if slot.1.is_empty() {
                                slot.1 = name;
                            }
                            slot.2.push_str(&args);
                        }
                        None => calls.push((id, name, args)),
                    }
                }
            }
            "response.completed" => {
                let u = &v["response"]["usage"];
                let u = if u.is_null() { &v["usage"] } else { u };
                if let Some(p) = u["input_tokens"].as_u64() {
                    usage.input = p;
                }
                if let Some(c) = u["output_tokens"].as_u64() {
                    usage.output = c;
                }
                break;
            }
            "response.failed" => {
                let msg = v["response"]["error"]["message"]
                    .as_str()
                    .or_else(|| v["error"]["message"].as_str())
                    .or_else(|| v["message"].as_str())
                    .unwrap_or("unknown API error");
                return Err(msg.to_string());
            }
            "error" => {
                let msg = v["message"].as_str().unwrap_or("unknown API error");
                return Err(msg.to_string());
            }
            _ => {}
        }
    }
    // Flush dangling state if the stream was cut early.
    let mut blocks: Vec<Block> = Vec::new();
    if !thinking.trim().is_empty() {
        blocks.push(Block::Thinking { text: std::mem::take(&mut thinking) });
    }
    if !text.trim().is_empty() {
        blocks.push(Block::Text(std::mem::take(&mut text)));
    }
    for (n, (id, name, args)) in calls.into_iter().enumerate() {
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

    /// Serializes the env-mutating retry tests: parallel test threads share
    /// one process environment, so anything touching PIR_RETRY_* /
    /// PIR_MAX_ATTEMPTS holds this while asserting.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        // SAFETY: edition 2024 marks env mutation unsafe; pir confines
        // it to startup config and explicit session toggles.
        unsafe { std::env::set_var("PIR_STALL_TIMEOUT_SECS", "1"); }
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
        // SAFETY: edition 2024 marks env mutation unsafe; pir confines
        // it to startup config and explicit session toggles.
        unsafe { std::env::remove_var("PIR_STALL_TIMEOUT_SECS"); }
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
        // Serialize with the other env-sensitive retry tests (parallel test
        // threads share one process environment).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("PIR_RETRY_BASE_SECS");
            std::env::remove_var("PIR_RETRY_MAX_SECS");
        }
        // Timeout tier: 10s, 20s, 40s … capped at 300s.
        assert_eq!(retry_backoff(0, true), Duration::from_secs(10));
        assert_eq!(retry_backoff(1, true), Duration::from_secs(20));
        assert_eq!(retry_backoff(5, true), Duration::from_secs(300)); // hits cap
        assert_eq!(retry_backoff(10, true), Duration::from_secs(300));
        // Other-transient tier: 30s, 60s, 120s … capped at 600s.
        assert_eq!(retry_backoff(0, false), Duration::from_secs(30));
        assert_eq!(retry_backoff(1, false), Duration::from_secs(60));
        assert_eq!(retry_backoff(2, false), Duration::from_secs(120));
        assert_eq!(retry_backoff(5, false), Duration::from_secs(600)); // hits cap
        assert_eq!(retry_backoff(100, false), Duration::from_secs(600));
    }

    #[test]
    fn http_backend_parse() {
        assert_eq!(HttpBackend::parse("isahc"), Some(HttpBackend::Isahc));
        assert_eq!(HttpBackend::parse("ureq"), Some(HttpBackend::Ureq));
        assert_eq!(HttpBackend::parse(" UREQ "), Some(HttpBackend::Ureq));
        assert_eq!(HttpBackend::parse("curl"), None);
        assert_eq!(HttpBackend::parse(""), None);
        assert_eq!(HttpBackend::default(), HttpBackend::Isahc);
        assert_eq!(HttpBackend::Isahc.name(), "isahc");
        assert_eq!(HttpBackend::Ureq.name(), "ureq");
    }

    #[test]
    fn request_headers_single_source() {
        // Both transports apply this vec: auth must never drift between them.
        let client = Client::new(ApiKind::OpenAi, "http://x", "k".to_string());
        let h = client.request_headers(ApiKind::OpenAi);
        assert!(
            h.contains(&("user-agent".to_string(), format!("pir/{}", env!("CARGO_PKG_VERSION")))),
            "user-agent present: {h:?}"
        );
        assert!(h.contains(&("Authorization".to_string(), "Bearer k".to_string())), "bearer: {h:?}");
        let a = Client::new(ApiKind::Anthropic, "http://x", "k".to_string());
        let h = a.request_headers(ApiKind::Anthropic);
        assert!(h.contains(&("x-api-key".to_string(), "k".to_string())), "api key: {h:?}");
        assert!(
            h.contains(&("anthropic-version".to_string(), "2023-06-01".to_string())),
            "version: {h:?}"
        );
    }

    #[test]
    fn channel_body_streams_then_eofs() {
        let (tx, rx) = smol::channel::bounded::<std::io::Result<Vec<u8>>>(8);
        tx.send_blocking(Ok(b"hel".to_vec())).unwrap();
        tx.send_blocking(Ok(b"lo".to_vec())).unwrap();
        drop(tx);
        let text = smol::block_on(async {
            use smol::io::AsyncReadExt as _;
            let mut body = ChannelBody { rx: Box::pin(rx), buf: Vec::new(), pos: 0, failed: None };
            let mut s = String::new();
            body.read_to_string(&mut s).await.unwrap();
            s
        });
        assert_eq!(text, "hello");
    }

    #[test]
    fn channel_body_surfaces_midstream_error() {
        // Bytes first, then a transport failure: the partial read succeeds,
        // the tail reports the error (never a silent clean EOF).
        let (tx, rx) = smol::channel::bounded::<std::io::Result<Vec<u8>>>(8);
        tx.send_blocking(Ok(b"partial".to_vec())).unwrap();
        tx.send_blocking(Err(std::io::Error::other("boom"))).unwrap();
        drop(tx);
        smol::block_on(async {
            use smol::io::AsyncReadExt as _;
            let mut body = ChannelBody { rx: Box::pin(rx), buf: Vec::new(), pos: 0, failed: None };
            let mut head = [0u8; 7];
            body.read_exact(&mut head).await.unwrap();
            assert_eq!(&head, b"partial");
            let mut tail = String::new();
            let err = body.read_to_string(&mut tail).await.unwrap_err().to_string();
            assert!(err.contains("boom"), "sticky error surfaces: {err}");
        });
    }

    #[test]
    fn ureq_backend_streams_mock_sse() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap().to_string();
        let _srv = thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            use std::io::Write as _;
            let body = "{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi-ureq\"}}]}\n\n";
            let frame = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {body}data: [DONE]\n\n");
            let _ = sock.write_all(frame.as_bytes());
            let _ = sock.flush();
            thread::sleep(Duration::from_millis(200));
        });
        let mut client = Client::new(ApiKind::OpenAi, &format!("http://{addr}"), "test-key".to_string());
        client.set_backend(HttpBackend::Ureq);
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
            None,
            &mut |_w: &RetryWait| {},
            &mut |_n: &str| {},
        );
        assert!(res.is_ok(), "ureq backend must stream, got {res:?}");
        assert!(text.contains("hi-ureq"), "expected streamed text, got {text:?}");
    }

    #[test]
    fn ureq_backend_maps_error_status() {
        // HTTP 400 is not retryable: single attempt, no env mutation needed.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap().to_string();
        let _srv = thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            use std::io::Write as _;
            let body = "{\"error\":{\"message\":\"bad key\"}}";
            let frame = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(frame.as_bytes());
            let _ = sock.flush();
            thread::sleep(Duration::from_millis(200));
        });
        let mut client = Client::new(ApiKind::OpenAi, &format!("http://{addr}"), "test-key".to_string());
        client.set_backend(HttpBackend::Ureq);
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
            None,
            &mut |_w: &RetryWait| {},
            &mut |_n: &str| {},
        );
        let err = res.unwrap_err();
        assert!(err.contains("HTTP 400"), "status mapped: {err}");
        assert!(err.contains("bad key"), "API message kept: {err}");
    }

    #[test]
    fn ureq_backend_cancel_is_prompt() {
        // Server holds the connection open; the flag flips mid-connect and
        // the turn must abort promptly, not hang on the socket.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap().to_string();
        let _srv = thread::spawn(move || {
            let (_sock, _) = listener.accept().expect("accept");
            thread::sleep(Duration::from_secs(30));
        });
        let mut client = Client::new(ApiKind::OpenAi, &format!("http://{addr}"), "test-key".to_string());
        client.set_backend(HttpBackend::Ureq);
        let cancel = Arc::new(AtomicBool::new(false));
        client.set_cancel(cancel.clone());
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
            None,
            &mut |_w: &RetryWait| {},
            &mut |_n: &str| {},
        );
        let elapsed = started.elapsed();
        assert!(res.is_err(), "expected cancellation error, got {res:?}");
        assert_eq!(res.unwrap_err(), "request cancelled");
        assert!(
            elapsed < Duration::from_secs(5),
            "ureq cancel took {elapsed:?}, must be prompt"
        );
    }

    #[test]
    fn timeout_constants_sane() {
        // Reads the shared env: hold the lock + clear overrides so a
        // concurrently-running retry test can't leak its vars in here.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("PIR_MAX_ATTEMPTS");
        }
        assert!(CONNECT_TIMEOUT.as_secs() >= 5);
        // The streaming *status-line* read timeout is now generous, so a slow
        // / "thinking" provider has time to send its first byte before we
        // retry. The stall watchdog (between SSE events once streaming) stays
        // short so a Ctrl-C/Ctrl-D is honoured promptly. Read timeouts double
        // with NO cap — a slow provider is given ever more time per attempt.
        assert!(READ_TIMEOUT_INIT.as_secs() >= 15);
        const _: () = assert!(READ_TIMEOUT_GROWTH >= 2);
        assert!(STALL_TIMEOUT.as_secs() >= 30);
        // Retries never give up on their own: no attempt cap by default, and
        // the backoff doubles per attempt (capped) for both tiers.
        assert_eq!(max_attempts(), None);
        assert!(retry_backoff(1, false) > retry_backoff(0, false));
        assert!(retry_backoff(1, true) > retry_backoff(0, true));
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
            None,
            &mut |_w: &RetryWait| {},
            &mut |_n: &str| {},
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
            None,
            &mut |_w: &RetryWait| {},
            &mut |_n: &str| {},
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
            None,
            &mut |_w: &RetryWait| {},
            &mut |_n: &str| {},
        );
        assert!(res.is_ok(), "response that lands before cancel must win, got {res:?}");
        assert!(text.contains("ok"), "expected streamed text, got {text:?}");
    }

    #[test]
    fn retries_forever_until_provider_recovers() {
        // Two 500s then a success: with no PIR_MAX_ATTEMPTS the loop must
        // keep going and return the recovered response, reporting each wait.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("PIR_RETRY_BASE_SECS", "0");
            std::env::remove_var("PIR_MAX_ATTEMPTS");
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap().to_string();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_srv = hits.clone();
        let _srv = thread::spawn(move || {
            use std::io::Write as _;
            // Headroom past the 3 expected hits: a pooled dead connection
            // can cost an extra accept, and the loop must still be listening
            // when the real retry lands. `Connection: close` keeps the
            // client from parking these sockets back in its pool.
            for _ in 0..10 {
                let (mut sock, _) = listener.accept().expect("accept");
                let n = hits_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n < 2 {
                    let _ = sock.write_all(
                        b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    let _ = sock.flush();
                } else {
                    let body = "{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"recovered\"}}]}";
                    let frame = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {body}\ndata: [DONE]\n\n"
                    );
                    let _ = sock.write_all(frame.as_bytes());
                    let _ = sock.flush();
                    break;
                }
            }
        });
        let client = Client::new(ApiKind::OpenAi, &format!("http://{addr}"), "test-key".to_string());
        let mut text = String::new();
        let mut waits = 0usize;
        let mut notices = String::new();
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
            None,
            &mut |_w: &RetryWait| waits += 1,
            &mut |n: &str| notices.push_str(n),
        );
        unsafe { std::env::remove_var("PIR_RETRY_BASE_SECS"); }
        assert!(res.is_ok(), "must recover after transient 500s, got {res:?}");
        assert!(text.contains("recovered"), "expected recovered text, got {text:?}");
        // Notices travel the notice channel now, never the model text: the
        // transcript stays pure model output while attempts are still reported.
        assert!(!text.contains("attempt"), "model text must not carry notices: {text:?}");
        assert!(
            notices.contains("attempt 1") && notices.contains("attempt 2"),
            "notices must show both failed attempts, got {notices:?}"
        );
        assert!(notices.contains("reconnected on attempt 3"), "must note recovery, got {notices:?}");
        assert!(waits >= 2, "countdown must have been reported per wait, got {waits}");
    }

    #[test]
    fn max_attempts_caps_retries_for_scripts() {
        // Always-500 server with PIR_MAX_ATTEMPTS=2: must give up after
        // exactly 2 attempts instead of looping forever.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("PIR_RETRY_BASE_SECS", "0");
            std::env::set_var("PIR_MAX_ATTEMPTS", "2");
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap().to_string();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_srv = hits.clone();
        let _srv = thread::spawn(move || {
            use std::io::Write as _;
            // Serve a few more than the cap so a runaway loop can't deadlock
            // on accept; the client must stop after 2.
            for _ in 0..5 {
                let Ok((mut sock, _)) = listener.accept() else { break };
                hits_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = sock.write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
                );
                let _ = sock.flush();
            }
        });
        let client = Client::new(ApiKind::OpenAi, &format!("http://{addr}"), "test-key".to_string());
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
            None,
            &mut |_w: &RetryWait| {},
            &mut |_n: &str| {},
        );
        unsafe {
            std::env::remove_var("PIR_RETRY_BASE_SECS");
            std::env::remove_var("PIR_MAX_ATTEMPTS");
        }
        assert!(res.is_err(), "must give up under PIR_MAX_ATTEMPTS, got {res:?}");
        let err = res.unwrap_err();
        assert!(
            err.contains("gave up after 2 attempts"),
            "error must name the attempt count, got: {err:?} (transcript: {text:?})"
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    fn user_text(s: &str) -> Message {
        Message { role: Role::User, blocks: vec![Block::Text(s.to_string())] }
    }

    fn asst_tool(id: &str, name: &str) -> Message {
        Message {
            role: Role::Assistant,
            blocks: vec![Block::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input: serde_json::json!({}),
            }],
        }
    }

    fn user_result(id: &str, content: &str) -> Message {
        Message {
            role: Role::User,
            blocks: vec![Block::ToolResult {
                tool_use_id: id.to_string(),
                content: content.to_string(),
                is_error: false,
            }],
        }
    }

    /// Every `role: tool` message in an OpenAI payload must reference a
    /// `tool_call_id` from the immediately preceding assistant message.
    fn assert_openai_tools_valid(body: &Value) {
        let msgs = body.get("messages").and_then(Value::as_array).expect("messages");
        let mut open: Vec<String> = Vec::new();
        for m in msgs {
            let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("?");
            match role {
                "assistant" => {
                    open = m
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .map(|cs| {
                            cs.iter()
                                .filter_map(|c| {
                                    c.get("id").and_then(|v| v.as_str()).map(str::to_string)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                }
                "tool" => {
                    let id = m.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("");
                    assert!(
                        open.contains(&id.to_string()),
                        "orphan tool message for '{id}' after assistant calls {open:?}"
                    );
                    open.retain(|x| x != id);
                }
                _ => {}
            }
        }
    }

    #[test]
    fn stream_openai_reasoning_content_forwarded_to_think() {
        // DeepSeek-compatible APIs (DeepSeek, Ollama Cloud) stream the
        // chain-of-thought as `delta.reasoning_content`, not `delta.reasoning`.
        // It must reach `on_think` and the Thinking block — previously it was
        // silently dropped and the whole reasoning phase showed as a bare spinner.
        const STREAM: &str = concat!(
            r#"data: {"choices":[{"delta":{"role":"assistant","reasoning_content":"let me think"}}]}"#,
            "\n",
            r#"data: {"choices":[{"delta":{"reasoning_content":" about clang"}}]}"#,
            "\n",
            r#"data: {"choices":[{"delta":{"content":"result"}}]}"#,
            "\n",
            "data: [DONE]\n",
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let (_keep_sender_alive, rx) = smol_channel::<()>(1);
        // Shared buffers: the `run` closure must be 'static for the deadline
        // thread, so plain `&mut` captures are illegal here.
        let think_buf = Arc::new(Mutex::new(String::new()));
        let text_buf = Arc::new(Mutex::new(String::new()));
        let run = {
            let think_buf = think_buf.clone();
            let text_buf = text_buf.clone();
            move || {
                let mut reader =
                    smol::io::BufReader::new(smol::io::Cursor::new(STREAM.as_bytes()));
                smol::block_on(stream_openai(
                    &mut reader,
                    &mut |t: &str| text_buf.lock().unwrap().push_str(t),
                    &mut false,
                    &mut false,
                    &cancel,
                    &rx,
                    &mut |t: &str| think_buf.lock().unwrap().push_str(t),
                ))
            }
        };
        let (msg, _usage) = match run_stream_with_deadline(run, 5) {
            Ok(v) => v,
            Err(e) => panic!("unexpected stream error: {e}"),
        };
        let think = think_buf.lock().unwrap().clone();
        let text = text_buf.lock().unwrap().clone();
        assert_eq!(think, "let me think about clang", "reasoning_content must reach on_think");
        assert!(text.contains("result"));
        assert!(
            msg.blocks.iter().any(|b| matches!(b, Block::Thinking { .. })),
            "Thinking block must be recorded, got {:?}",
            msg.blocks
        );
    }

    #[test]
    fn chat_debug_line_format() {
        // The PIR_DEBUG timing line must carry prompt size, both spans, and
        // the token rate: that tuple is what separates "slow network" from
        // "slow generation" from "huge prompt".
        let s = chat_debug_line(2, 42137, Duration::from_millis(812), Duration::from_secs(38), 214, Some(900), Some(38100));
        assert!(s.starts_with("pir-debug chat:"), "machine-greppable prefix: {s}");
        assert!(s.contains("attempt=2"), "{s}");
        assert!(s.contains("prompt_bytes=42137"), "{s}");
        assert!(s.contains("headers_ms=812"), "{s}");
        assert!(s.contains("out_tokens=214"), "{s}");
        assert!(s.contains("tok/s"), "{s}");
        assert!(s.contains("think_first_ms=900"), "{s}");
        assert!(s.contains("text_first_ms=38100"), "{s}");
    }

    #[test]
    fn debug_payload_dump_roundtrips() {
        // `PIR_DEBUG_PAYLOAD=<path>` must persist the exact body so a turn
        // can be replayed byte-identical through curl.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = std::env::temp_dir().join(format!("pir_body_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        unsafe { std::env::set_var("PIR_DEBUG_PAYLOAD", &path); }
        let body = serde_json::json!({"model": "m", "stream": true});
        maybe_dump_payload(&body);
        unsafe { std::env::remove_var("PIR_DEBUG_PAYLOAD"); }
        let back: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("payload file"))
                .expect("valid json");
        assert_eq!(back, body, "dumped body must parse back identically");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn debug_log_to_file_appends() {
        // `PIR_DEBUG=<path>` must append the line to the file (stderr can be
        // wiped by a spinner repaint before it is read).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = std::env::temp_dir().join(format!("pir_dbg_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        unsafe { std::env::set_var("PIR_DEBUG", &path); }
        debug_log("pir-debug chat: attempt=1");
        debug_log("pir-debug chat: attempt=2");
        unsafe { std::env::remove_var("PIR_DEBUG"); }
        let body = std::fs::read_to_string(&path).expect("log file");
        assert!(
            body.lines().count() == 2 && body.contains("attempt=2"),
            "both lines appended: {body:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn client_sends_streaming_friendly_headers() {
        // SSE must not be content-encoded: isahc enables transparent gzip/br
        // by default, advertising `Accept-Encoding` — and some proxies buffer
        // compressed streams, turning a live token drip into sludge while the
        // byte-identical body streams instantly for curl (which sends no
        // Accept-Encoding). Capture the raw request off the wire and assert
        // pir asks for identity, like curl.
        use std::io::Read as _;
        use std::io::Write as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(false).unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let cap_srv = captured.clone();
        let srv = thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            // Read headers, then exactly Content-Length body bytes.
            let head_end = loop {
                let n = sock.read(&mut tmp).expect("read");
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = find_subslice(&buf, b"\r\n\r\n") {
                    break Some(p + 4);
                }
            };
            let head_end = head_end.expect("complete headers");
            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length:").or_else(|| l.strip_prefix("content-length:")))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            while buf.len() < head_end + len {
                let n = sock.read(&mut tmp).expect("read body");
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            *cap_srv.lock().unwrap() = buf;
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            let _ = sock.flush();
        });
        let client = Client::new(ApiKind::OpenAi, &format!("http://{addr}"), "k".to_string());
        let res = smol::block_on(async {
            let req = isahc::Request::builder()
                .method("POST")
                .uri(format!("http://{addr}/v1/chat/completions"))
                .header("content-type", "application/json")
                .body("{}")
                .unwrap();
            client.http.send_async(req).await
        });
        assert!(res.is_ok(), "local send must succeed: {res:?}");
        srv.join().expect("server");
        let raw = captured.lock().unwrap().clone();
        let head = String::from_utf8_lossy(&raw);
        let head = head.split("\r\n\r\n").next().unwrap_or("").to_lowercase();
        for enc in ["gzip", "deflate", "br", "zstd"] {
            assert!(
                !head.contains(&format!("accept-encoding:").to_string()) || !head.contains(enc),
                "must not advertise compressed encoding '{enc}':\n{head}"
            );
        }
    }

    fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    #[test]
    fn client_sends_identity_and_session_headers() {
        // Vendor contract (opencode.ai/docs/go): identify as our own agent
        // (never a bare HTTP-library UA) and send the stable per-conversation
        // session id -- but ONLY when one is set (opencode-go via
        // make_client), never leaking the header to other providers.
        use std::io::Read as _;
        use std::io::Write as _;
        fn head_for(configure: impl FnOnce(&mut Client)) -> String {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let addr = listener.local_addr().unwrap().to_string();
            let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
            let cap_srv = captured.clone();
            let srv = thread::spawn(move || {
                let (mut sock, _) = listener.accept().expect("accept");
                sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let head_end = loop {
                    let n = sock.read(&mut tmp).expect("read");
                    if n == 0 {
                        break None;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = find_subslice(&buf, b"\r\n\r\n") {
                        break Some(p + 4);
                    }
                };
                let head_end = head_end.expect("complete headers");
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let len: usize = head
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("Content-Length:")
                            .or_else(|| l.strip_prefix("content-length:"))
                    })
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                while buf.len() < head_end + len {
                    let n = sock.read(&mut tmp).expect("read body");
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                *cap_srv.lock().unwrap() = buf;
                let _ = sock.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
                let _ = sock.flush();
            });
            let mut client =
                Client::new(ApiKind::OpenAi, &format!("http://{addr}"), "k".to_string());
            configure(&mut client);
            let res = smol::block_on(async {
                let req = client.apply_headers(
                    isahc::Request::builder()
                        .method("POST")
                        .uri(format!("http://{addr}/v1/chat/completions"))
                        .header("content-type", "application/json"),
                    ApiKind::OpenAi,
                );
                let req = req.body("{}").unwrap();
                client.http.send_async(req).await
            });
            assert!(res.is_ok(), "local send must succeed: {res:?}");
            srv.join().expect("server");
            let raw = captured.lock().unwrap().clone();
            let full = String::from_utf8_lossy(&raw).to_string();
            full.split("\r\n\r\n").next().unwrap_or("").to_lowercase()
        }
        // No session set (every other provider): UA present, no session header.
        let plain = head_for(|_| {});
        assert!(plain.contains("user-agent: pir/"), "UA names our agent:\n{plain}");
        assert!(!plain.contains("x-opencode-session"), "no session leak:\n{plain}");
        // Session set (make_client does this for opencode-go): both headers.
        let sess = head_for(|c| c.set_session_id(Some("sess-1".to_string())));
        assert!(sess.contains("user-agent: pir/"), "UA names our agent:\n{sess}");
        assert!(
            sess.contains("x-opencode-session: sess-1"),
            "session header present:\n{sess}"
        );
    }

    #[test]
    fn sse_kinds_classify_phases() {
        // Chunk-log phase labels: the gap analysis lives or dies on these.
        let text = serde_json::json!({"type": "content_block_delta", "delta": {"type": "text_delta"}});
        assert_eq!(sse_kind_chat(&text), "text");
        let think = serde_json::json!({"type": "content_block_delta", "delta": {"type": "thinking_delta"}});
        assert_eq!(sse_kind_chat(&think), "reasoning");
        let tool = serde_json::json!({"type": "content_block_delta", "delta": {"type": "input_json_delta"}});
        assert_eq!(sse_kind_chat(&tool), "tool");
        let usage = serde_json::json!({"type": "message_delta"});
        assert_eq!(sse_kind_chat(&usage), "usage");
        let other = serde_json::json!({"type": "message_start"});
        assert_eq!(sse_kind_chat(&other), "other");
        // OpenAI chat shape (no `type` field): delta keys select the phase.
        let otext = serde_json::json!({"choices": [{"delta": {"content": "hi"}}]});
        assert_eq!(sse_kind_chat(&otext), "text");
        let oreason = serde_json::json!({"choices": [{"delta": {"reasoning_content": "hmm"}}]});
        assert_eq!(sse_kind_chat(&oreason), "reasoning");
        let otool = serde_json::json!({"choices": [{"delta": {"tool_calls": []}}]});
        assert_eq!(sse_kind_chat(&otool), "tool");
        let ousage = serde_json::json!({"usage": {}});
        assert_eq!(sse_kind_chat(&ousage), "usage");
        let ostop = serde_json::json!({"choices": [{"finish_reason": "stop"}]});
        assert_eq!(sse_kind_chat(&ostop), "other");
        assert_eq!(sse_kind_responses("response.output_text.delta"), "text");
        assert_eq!(sse_kind_responses("response.reasoning_summary_text.delta"), "reasoning");
        assert_eq!(sse_kind_responses("response.output_item.done"), "tool");
        assert_eq!(sse_kind_responses("response.completed"), "usage");
        assert_eq!(sse_kind_responses("response.failed"), "error");
        assert_eq!(sse_kind_responses("response.created"), "other");
    }

    #[test]
    fn chunk_log_writes_arrival_lines() {
        // `ms kind bytes`, one line per event, no content — the analyzer's input.
        let path = std::env::temp_dir().join(format!("pir_chunks_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut log = ChunkLog::open_at(&path);
        log.event("text", 12);
        log.event("reasoning", 200);
        drop(log); // flush the BufWriter before reading back
        let body = std::fs::read_to_string(&path).expect("chunk log");
        let mut lines = body.lines();
        let l1 = lines.next().expect("line 1");
        let l2 = lines.next().expect("line 2");
        assert!(l1.ends_with(" text 12"), "{l1:?}");
        assert!(l2.ends_with(" reasoning 200"), "{l2:?}");
        assert!(l1.split_whitespace().next().unwrap().parse::<u128>().is_ok(), "leading ms: {l1:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sanitize_clean_history_passes_through() {
        let h = vec![
            user_text("hi"),
            asst_tool("c1", "read_file"),
            user_result("c1", "contents"),
            user_text("thanks"),
        ];
        let clean = sanitize_history(&h);
        assert_eq!(clean.len(), h.len());
        assert_eq!(format!("{:?}", clean), format!("{:?}", h));
    }

    #[test]
    fn sanitize_orphan_result_becomes_text() {
        // The reported 400: a tool result with no preceding tool call
        // (trim cut the assistant message away).
        let h = vec![user_text("hi"), user_result("ghost", "stale output")];
        let clean = sanitize_history(&h);
        let has_tool_result = clean.iter().flat_map(|m| m.blocks.iter()).any(|b| {
            matches!(b, Block::ToolResult { .. })
        });
        assert!(!has_tool_result, "no ToolResult may survive: {clean:?}");
        assert!(
            clean.iter().any(|m| m.text().contains("stale output")),
            "content must be preserved as text: {clean:?}"
        );
    }

    #[test]
    fn sanitize_dangling_calls_gain_synthetic_results() {
        // Assistant tool calls with no results (trim cut / skipped calls).
        let h = vec![user_text("hi"), asst_tool("c1", "bash")];
        let clean = sanitize_history(&h);
        assert_eq!(clean.len(), 3);
        let last = &clean[2];
        assert_eq!(last.role, Role::User);
        match &last.blocks[..] {
            [Block::ToolResult { tool_use_id, is_error, .. }] => {
                assert_eq!(tool_use_id, "c1");
                assert!(is_error);
            }
            other => panic!("expected synthetic result, got {other:?}"),
        }
    }

    #[test]
    fn sanitize_resume_fold_splits_roles() {
        // Session resume folds assistant blocks into the user message:
        // ToolUse inside a User message must be lifted back out.
        let h = vec![Message {
            role: Role::User,
            blocks: vec![
                Block::Text("do it".to_string()),
                Block::ToolUse {
                    id: "c9".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!({}),
                },
                Block::ToolResult {
                    tool_use_id: "c9".to_string(),
                    content: "done".to_string(),
                    is_error: false,
                },
            ],
        }];
        let clean = sanitize_history(&h);
        assert_eq!(clean.len(), 3);
        assert_eq!(clean[0].role, Role::Assistant);
        assert!(matches!(clean[0].blocks[0], Block::ToolUse { .. }));
        assert_eq!(clean[1].role, Role::User);
        assert!(matches!(clean[1].blocks[0], Block::ToolResult { .. }));
        assert_eq!(clean[2].role, Role::User);
        assert!(clean[2].text().contains("do it"));
    }

    #[test]
    fn sanitize_partial_results_close_immediately() {
        // Two calls, one result (preflight terminate skipped the second):
        // the missing one is synthesized adjacent, not left dangling.
        let h = vec![{
            let mut m = asst_tool("c1", "bash");
            m.blocks.push(Block::ToolUse {
                id: "c2".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({}),
            });
            m
        }, user_result("c1", "ok")];
        let clean = sanitize_history(&h);
        assert_eq!(clean.len(), 3);
        match &clean[2].blocks[..] {
            [Block::ToolResult { tool_use_id, is_error, .. }] => {
                assert_eq!(tool_use_id, "c2");
                assert!(is_error);
            }
            other => panic!("expected synthetic result for c2, got {other:?}"),
        }
    }

    #[test]
    fn openai_payload_has_no_orphan_tools_after_sanitize() {
        // End to end: corrupted histories must serialize to valid OpenAI
        // payloads (this is the exact 400 the user hit).
        let client = Client::new(ApiKind::OpenAi, "http://x", "k".to_string());
        let cases = vec![
            // orphan result
            vec![user_text("hi"), user_result("ghost", "stale")],
            // dangling call
            vec![user_text("hi"), asst_tool("c1", "bash")],
            // resume fold
            vec![Message {
                role: Role::User,
                blocks: vec![
                    Block::Text("do it".to_string()),
                    Block::ToolUse {
                        id: "c9".to_string(),
                        name: "bash".to_string(),
                        input: serde_json::json!({}),
                    },
                    Block::ToolResult {
                        tool_use_id: "c9".to_string(),
                        content: "done".to_string(),
                        is_error: false,
                    },
                ],
            }],
        ];
        for h in &cases {
            let clean = sanitize_history(h);
            let (_url, body) = client.openai_request(
                "m",
                16,
                "sys",
                &clean,
                &[],
                crate::config::ThinkingLevel::Off,
                None,
            );
            assert_openai_tools_valid(&body);
        }
    }

    /// Reasoning model builders mirroring `models-store.json` entries.
    fn reasoning_model(
        format: Option<&str>,
        map: &[(&str, Option<&str>)],
    ) -> crate::config::Model {
        crate::config::Model {
            id: "m".into(),
            name: None,
            context: Some(1_000_000),
            max_tokens: Some(384_000),
            api_override: None,
            url_override: None,
            no_reasoning_effort: false,
            reasoning: true,
            thinking_format: format.map(str::to_string),
            supports_reasoning_effort: None,
            thinking_level_map: map
                .iter()
                .map(|(k, v)| (k.to_string(), v.map(str::to_string)))
                .collect(),
            price_per_1k: None,
        }
    }

    fn non_reasoning_model() -> crate::config::Model {
        let mut m = reasoning_model(None, &[]);
        m.reasoning = false;
        m
    }

    fn chat_body(
        format: Option<&str>,
        map: &[(&str, Option<&str>)],
        thinking: crate::config::ThinkingLevel,
    ) -> Value {
        let client = Client::new(ApiKind::OpenAi, "http://x", "k".to_string());
        let m = reasoning_model(format, map);
        let (_url, body) =
            client.openai_request("m", 16, "sys", &[], &[], thinking, Some(&m));
        body
    }

    #[test]
    fn deepseek_format_sends_thinking_toggle_and_mapped_effort() {
        use crate::config::ThinkingLevel as L;
        // The opencode-go deepseek-v4.1-flash shape: high/max only.
        let map: &[(&str, Option<&str>)] = &[
            ("minimal", None),
            ("low", None),
            ("medium", None),
            ("high", Some("high")),
            ("max", Some("max")),
        ];
        // High: toggle enabled + mapped effort. This is the slow-thinking
        // fix: previously only a bare `reasoning_effort` was sent.
        let body = chat_body(Some("deepseek"), map, L::High);
        assert_eq!(body["thinking"], serde_json::json!({ "type": "enabled" }));
        assert_eq!(body["reasoning_effort"], serde_json::json!("high"));
        // Max must map to "max", not pir's collapsed default "high".
        let body = chat_body(Some("deepseek"), map, L::Max);
        assert_eq!(body["thinking"], serde_json::json!({ "type": "enabled" }));
        assert_eq!(body["reasoning_effort"], serde_json::json!("max"));
        // Off: explicit disable toggle, no effort (pi parity).
        let body = chat_body(Some("deepseek"), map, L::Off);
        assert_eq!(body["thinking"], serde_json::json!({ "type": "disabled" }));
        assert!(body.get("reasoning_effort").is_none(), "off sends no effort: {body}");
        // Hidden-but-persisted level (medium): toggle still enables thinking
        // (pi sends it when forced); the effort falls back to the default.
        let body = chat_body(Some("deepseek"), map, L::Medium);
        assert_eq!(body["thinking"], serde_json::json!({ "type": "enabled" }));
        assert_eq!(body["reasoning_effort"], serde_json::json!("medium"));
    }

    #[test]
    fn deepseek_off_null_skips_disable_toggle() {
        use crate::config::ThinkingLevel as L;
        // `off: null` means thinking cannot be disabled (pi parity).
        let body = chat_body(Some("deepseek"), &[("off", None)], L::Off);
        assert!(body.get("thinking").is_none(), "no toggle when off is null: {body}");
    }

    #[test]
    fn non_reasoning_models_send_no_thinking_params() {
        use crate::config::ThinkingLevel as L;
        let client = Client::new(ApiKind::OpenAi, "http://x", "k".to_string());
        let m = non_reasoning_model();
        for level in [L::Low, L::Medium, L::High, L::Max] {
            let (_url, body) =
                client.openai_request("m", 16, "sys", &[], &[], level, Some(&m));
            assert!(body.get("thinking").is_none(), "{level:?}: {body}");
            assert!(body.get("reasoning_effort").is_none(), "{level:?}: {body}");
            assert!(body.get("reasoning").is_none(), "{level:?}: {body}");
        }
    }

    #[test]
    fn legacy_no_meta_keeps_raw_effort() {
        use crate::config::ThinkingLevel as L;
        // No catalog metadata: exact legacy behavior (raw effort names).
        let client = Client::new(ApiKind::OpenAi, "http://x", "k".to_string());
        let (_url, body) =
            client.openai_request("m", 16, "sys", &[], &[], L::Medium, None);
        assert_eq!(body["reasoning_effort"], serde_json::json!("medium"));
        let (_url, body) =
            client.openai_request("m", 16, "sys", &[], &[], L::Minimal, None);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn openrouter_format_uses_nested_reasoning_object() {
        use crate::config::ThinkingLevel as L;
        let map: &[(&str, Option<&str>)] = &[("high", Some("high")), ("off", Some("none"))];
        let body = chat_body(Some("openrouter"), map, L::High);
        assert_eq!(body["reasoning"], serde_json::json!({ "effort": "high" }));
        assert!(body.get("reasoning_effort").is_none(), "nested, not flat: {body}");
        // Off falls back to the explicit off value (pi `?? "none"`).
        let body = chat_body(Some("openrouter"), &[("high", Some("high"))], L::Off);
        assert_eq!(body["reasoning"], serde_json::json!({ "effort": "none" }));
    }

    #[test]
    fn qwen_format_sets_enable_thinking() {
        use crate::config::ThinkingLevel as L;
        let body = chat_body(Some("qwen"), &[], L::High);
        assert_eq!(body["enable_thinking"], serde_json::json!(true));
        assert_eq!(body["reasoning_effort"], serde_json::json!("high"));
        let body = chat_body(Some("qwen"), &[], L::Off);
        assert_eq!(body["enable_thinking"], serde_json::json!(false));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn effort_opt_out_drops_effort_but_keeps_toggle() {
        use crate::config::ThinkingLevel as L;
        // Models that reject effort fields (kimi-k2.6 style): the toggle
        // stays, both effort shapes go.
        let client = Client::new(ApiKind::OpenAi, "http://x", "k".to_string());
        let m = reasoning_model(Some("deepseek"), &[("high", Some("high"))]);
        let (_url, body) = client.openai_request_at(
            "https://example.com/v1",
            "m",
            16,
            "sys",
            &[],
            &[],
            L::High,
            false,
            Some(&m),
        );
        assert_eq!(body["thinking"], serde_json::json!({ "type": "enabled" }));
        assert!(body.get("reasoning_effort").is_none(), "opt-out drops effort: {body}");
    }

    #[test]
    fn responses_request_maps_effort_through_level_map() {
        use crate::config::ThinkingLevel as L;
        let client = Client::new(ApiKind::OpenAiResponses, "http://x", "k".to_string());
        let m = reasoning_model(None, &[("max", Some("max"))]);
        let (_url, body) =
            client.responses_request("m", 16, "sys", &[], &[], L::Max, true, Some(&m));
        assert_eq!(body["reasoning"], serde_json::json!({ "effort": "max" }));
        // Off → explicit "none" (pi parity).
        let (_url, body) =
            client.responses_request("m", 16, "sys", &[], &[], L::Off, true, Some(&m));
        assert_eq!(body["reasoning"], serde_json::json!({ "effort": "none" }));
    }
}




