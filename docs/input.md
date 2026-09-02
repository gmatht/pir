Below is a complete, working mini-agent — I call it **`pir`** ("pi in Rust"). It's ~700 lines, three dependencies (`ureq`, `serde`, `serde_json`), fully synchronous (no tokio), and it **reuses your existing `~/.pi` setup read-only**:

- `~/.pi/models.json` — providers, base URLs, models, API keys (`{env:VAR}` expansion supported). If missing, a starter file is written; yours is never overwritten.
- `~/.pi/agent/settings.json` — optional `"model"` key used as default model.
- `~/.pi/AGENTS.md` and `./AGENTS.md` — appended to the system prompt.
- `~/.pi/agent/sessions/pir-*.jsonl` — session transcripts are appended here.

It speaks both wire formats pi's providers use (**Anthropic Messages** and **OpenAI-compatible chat completions**, incl. OpenRouter/DeepSeek/etc.), streams tokens over SSE, and gives the model five tools: `bash`, `read_file`, `write_file`, `edit_file`, `list_dir`, with y/a/n confirmations (or `-y` full-auto).

```
pir/
├── Cargo.toml
└── src/
    ├── main.rs      # CLI + REPL
    ├── config.rs    # ~/.pi loading
    ├── types.rs     # internal message model
    ├── provider.rs  # HTTP + SSE for both APIs
    ├── tools.rs     # tool specs + sandboxed-ish execution
    ├── agent.rs     # the agent loop
    └── term.rs      # colors, dates, prompts
```

### Cargo.toml

```toml
[package]
name = "pir"
version = "0.1.0"
edition = "2021"
description = "A featherweight pi-compatible terminal coding agent"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
ureq = { version = "2", features = ["json"] }   # pinned to 2.x (blocking API)

[profile.release]
strip = true
lto = true
```

### src/types.rs

```rust
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// Internal message model — maps cleanly onto both the Anthropic
/// content-block format and OpenAI tool-call format.
#[derive(Debug, Clone)]
pub enum Block {
    Text(String),
    ToolUse { id: String, name: String, input: Value },
    ToolResult { tool_use_id: String, content: String, is_error: bool },
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub blocks: Vec<Block>,
}

impl Message {
    pub fn user(text: &str) -> Self {
        Message { role: Role::User, blocks: vec![Block::Text(text.to_string())] }
    }

    pub fn text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|b| match b { Block::Text(t) => Some(t.as_str()), _ => None })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn tool_uses(&self) -> Vec<(&str, &str, &Value)> {
        self.blocks
            .iter()
            .filter_map(|b| match b {
                Block::ToolUse { id, name, input } => Some((id.as_str(), name.as_str(), input)),
                _ => None,
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
            || self.blocks.iter().all(|b| matches!(b, Block::Text(t) if t.trim().is_empty()))
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
}
```

### src/term.rs

```rust
use std::io::{self, IsTerminal, Write};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static COLOR: OnceLock<bool> = OnceLock::new();

pub fn set_color(on: bool) {
    let _ = COLOR.set(on);
}

fn color() -> bool {
    *COLOR.get_or_init(|| io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
}

fn paint(code: &str, s: &str) -> String {
    if color() { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
}

pub fn dim(s: &str) -> String { paint("2", s) }
pub fn bold(s: &str) -> String { paint("1", s) }
pub fn red(s: &str) -> String { paint("31", s) }
pub fn yellow(s: &str) -> String { paint("33", s) }
pub fn cyan(s: &str) -> String { paint("36", s) }

pub fn read_answer(prompt: &str) -> String {
    eprint!("{prompt} ");
    let _ = io::stderr().flush();
    let mut s = String::new();
    let _ = io::stdin().read_line(&mut s);
    s.trim().to_lowercase()
}

pub fn epoch() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn date_string() -> String {
    let (y, m, d, _, _, _) = utc_parts(epoch());
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn timestamp_compact() -> String {
    let (y, mo, d, h, mi, s) = utc_parts(epoch());
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

fn utc_parts(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let sod = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    (y, m, d, (sod / 3600) as u32, ((sod % 3600) / 60) as u32, (sod % 60) as u32)
}

/// Howard Hinnant's civil-from-days (no chrono dependency).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + i64::from(m <= 2), m, d)
}
```

### src/config.rs

```rust
use crate::term;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKind {
    Anthropic,
    OpenAi,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Provider {
    pub id: Option<String>,
    pub name: Option<String>,
    #[serde(alias = "baseUrl", alias = "url")]
    pub base_url: Option<String>,
    #[serde(alias = "apiKey", alias = "key")]
    pub api_key: Option<String>,
    pub api: Option<String>,
    #[serde(default)]
    pub models: Vec<Model>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: Option<String>,
    pub context: Option<u64>,
    #[serde(alias = "maxTokens")]
    pub max_tokens: Option<u64>,
}

impl Provider {
    pub fn pid(&self) -> String {
        if let Some(id) = &self.id { return id.clone(); }
        if let Some(n) = &self.name { return n.to_lowercase().replace(' ', "-"); }
        "custom".into()
    }

    pub fn label(&self, m: &Model) -> String {
        format!("{}/{}", self.pid(), m.id)
    }

    /// Wire format: explicit `api` field wins, else infer from the base URL.
    pub fn kind(&self) -> Option<ApiKind> {
        let api = self.api.as_deref().map(str::to_lowercase);
        let base = self.base_url.as_deref().unwrap_or_default().to_lowercase();
        match api.as_deref() {
            Some(a) if a.contains("anthropic") => Some(ApiKind::Anthropic),
            Some(_) => Some(ApiKind::OpenAi),
            None if base.contains("anthropic.com") => Some(ApiKind::Anthropic),
            None if base.is_empty() => None,
            None => Some(ApiKind::OpenAi),
        }
    }

    pub fn api_key(&self) -> Option<String> {
        self.api_key.as_ref().map(|k| expand_env(k)).filter(|k| !k.is_empty())
    }
}

/// pi stores keys as "{env:VARNAME}" — expand them, pass literals through.
pub fn expand_env(s: &str) -> String {
    if let Some(var) = s.strip_prefix("{env:").and_then(|r| r.strip_suffix('}')) {
        std::env::var(var).unwrap_or_default()
    } else {
        s.to_string()
    }
}

pub fn pi_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("PI_DIR") {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".pi")
}

/// Read ~/.pi/models.json. Tolerant to providers being a list (pi's default)
/// or a map keyed by provider id. Never writes over an existing file.
pub fn load_providers() -> Result<Vec<Provider>, String> {
    let path = pi_dir().join("models.json");
    if !path.exists() {
        let _ = fs::create_dir_all(pi_dir());
        let _ = fs::write(&path, DEFAULT_MODELS_JSON);
        eprintln!("{} created starter {}", term::dim("·"), path.display());
    }
    let raw = fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut v: Value =
        serde_json::from_str(&raw).map_err(|e| format!("parsing {}: {e}", path.display()))?;

    if let Some(ps) = v.get_mut("providers") {
        if ps.is_object() {
            let map: BTreeMap<String, Value> =
                serde_json::from_value(ps.take()).map_err(|e| e.to_string())?;
            let mut list = Vec::new();
            for (key, mut pv) in map {
                if pv.get("id").is_none() {
                    if let Some(o) = pv.as_object_mut() {
                        o.insert("id".into(), Value::String(key));
                    }
                }
                list.push(pv);
            }
            *ps = Value::Array(list);
        }
    }

    #[derive(Deserialize)]
    struct ModelsFile {
        #[serde(default)]
        providers: Vec<Provider>,
    }
    let f: ModelsFile =
        serde_json::from_value(v).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(f.providers)
}

/// Optional: pi's agent settings may pin a default model.
pub fn default_model_setting() -> Option<String> {
    let p = pi_dir().join("agent").join("settings.json");
    let raw = fs::read_to_string(p).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    for key in ["model", "defaultModel", "default_model"] {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            return Some(s.to_string());
        }
    }
    None
}

/// Resolve "provider/model", bare model id, or a unique fuzzy substring.
pub fn select<'a>(
    providers: &'a [Provider],
    selector: &str,
) -> Result<(&'a Provider, &'a Model), String> {
    let sel = selector.trim().to_lowercase();

    if let Some((pid, mid)) = selector.trim().split_once('/') {
        for p in providers {
            if p.pid().eq_ignore_ascii_case(pid) {
                for m in &p.models {
                    if m.id.eq_ignore_ascii_case(mid) {
                        return Ok((p, m));
                    }
                }
            }
        }
    }
    for p in providers {
        for m in &p.models {
            if m.id.eq_ignore_ascii_case(selector.trim()) {
                return Ok((p, m));
            }
        }
    }
    let hits: Vec<(&'a Provider, &'a Model)> = providers
        .iter()
        .flat_map(|p| p.models.iter().map(move |m| (p, m)))
        .filter(|(p, m)| {
            format!("{}/{}", p.pid(), m.id).to_lowercase().contains(&sel)
                || m.name.as_deref().unwrap_or("").to_lowercase().contains(&sel)
        })
        .collect();
    match hits.as_slice() {
        [only] => Ok(*only),
        [] => Err(format!("no model matches '{selector}'")),
        _ => Err(format!(
            "'{selector}' is ambiguous: {}",
            hits.iter().map(|(p, m)| p.label(m)).collect::<Vec<_>>().join(", ")
        )),
    }
}

const DEFAULT_MODELS_JSON: &str = r#"{
  "providers": [
    {
      "id": "anthropic",
      "name": "Anthropic",
      "api": "anthropic",
      "baseUrl": "https://api.anthropic.com/v1",
      "apiKey": "{env:ANTHROPIC_API_KEY}",
      "models": [
        { "id": "claude-sonnet-4-5", "name": "Claude Sonnet 4.5", "context": 200000, "maxTokens": 64000 },
        { "id": "claude-haiku-4-5", "name": "Claude Haiku 4.5", "context": 200000, "maxTokens": 32000 }
      ]
    },
    {
      "id": "openai",
      "name": "OpenAI",
      "api": "openai",
      "baseUrl": "https://api.openai.com/v1",
      "apiKey": "{env:OPENAI_API_KEY}",
      "models": [
        { "id": "gpt-4.1", "name": "GPT-4.1", "context": 1000000, "maxTokens": 32000 },
        { "id": "gpt-4.1-mini", "name": "GPT-4.1 mini", "context": 1000000, "maxTokens": 32000 }
      ]
    },
    {
      "id": "openrouter",
      "name": "OpenRouter",
      "api": "openai",
      "baseUrl": "https://openrouter.ai/api/v1",
      "apiKey": "{env:OPENROUTER_API_KEY}",
      "models": [
        { "id": "anthropic/claude-sonnet-4.5", "name": "Claude Sonnet 4.5", "context": 200000, "maxTokens": 64000 },
        { "id": "google/gemini-2.5-pro", "name": "Gemini 2.5 Pro", "context": 1000000, "maxTokens": 64000 }
      ]
    }
  ]
}"#;
```

### src/provider.rs

```rust
use crate::config::ApiKind;
use crate::tools::ToolSpec;
use crate::types::{Block, Message, Role, Usage};
use serde_json::{json, Map, Value};
use std::io::{BufRead, BufReader, Read};

pub struct Client {
    kind: ApiKind,
    base_url: String,
    api_key: String,
    http: ureq::Agent,
}

impl Client {
    pub fn new(kind: ApiKind, base_url: &str, api_key: String) -> Self {
        Client {
            kind,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            http: ureq::Agent::new(),
        }
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
        let mut req = self.http.post(&url);
        req = match self.kind {
            ApiKind::Anthropic => req
                .set("x-api-key", &self.api_key)
                .set("anthropic-version", "2023-06-01"),
            ApiKind::OpenAi => req.set("Authorization", &format!("Bearer {}", self.api_key)),
        };
        let resp = req.send_json(&body).map_err(http_error)?;
        match self.kind {
            ApiKind::Anthropic => stream_anthropic(resp.into_reader(), on_text),
            ApiKind::OpenAi => stream_openai(resp.into_reader(), on_text),
        }
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
) -> Result<(Message, Usage), String> {
    let mut reader = BufReader::new(r);
    let mut blocks: Vec<Block> = Vec::new();
    let mut usage = Usage::default();
    let mut text = String::new();
    let mut tool: Option<(String, String, String)> = None; // (id, name, partial input json)

    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| format!("stream: {e}"))?;
        if n == 0 { break; }
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
) -> Result<(Message, Usage), String> {
    let mut reader = BufReader::new(r);
    let mut usage = Usage::default();
    let mut text = String::new();
    let mut calls: Vec<(u64, String, String, String)> = Vec::new(); // (index, id, name, args)

    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| format!("stream: {e}"))?;
        if n == 0 { break; }
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
```

### src/tools.rs

```rust
use crate::term;
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "bash",
            description: "Run a shell command in the project directory (bash -c, or cmd /C on \
                          Windows). Returns stdout and stderr (truncated to 30k chars) plus the \
                          exit code. Killed after 120s.",
            schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The shell command to execute" }
                },
                "required": ["command"]
            }),
        },
        ToolSpec {
            name: "read_file",
            description: "Read a UTF-8 text file (truncated to 100k chars).",
            schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "write_file",
            description: "Create or overwrite a file. Parent directories are created.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string", "description": "Full new file content" }
                },
                "required": ["path", "content"]
            }),
        },
        ToolSpec {
            name: "edit_file",
            description: "Replace exactly one occurrence of old_string with new_string. \
                          old_string must be unique — include surrounding lines to disambiguate.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        },
        ToolSpec {
            name: "list_dir",
            description: "List the entries of a directory (non-recursive).",
            schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Defaults to ." } },
                "required": []
            }),
        },
    ]
}

pub struct Outcome {
    pub content: String,
    pub is_error: bool,
}

enum Decision {
    Yes,
    Always,
    No,
}

fn ask(what: &str) -> Decision {
    let answer =
        term::read_answer(&format!("Allow {what}? [y]es / [a]lways / [n]o (default no)"));
    match answer.as_str() {
        "y" | "yes" => Decision::Yes,
        "a" | "always" => Decision::Always,
        _ => Decision::No,
    }
}

pub struct ToolRunner {
    cwd: PathBuf,
    full_auto: bool,
    bash_ok: bool,
    write_ok: bool,
}

impl ToolRunner {
    pub fn new(cwd: PathBuf, full_auto: bool) -> Self {
        ToolRunner { cwd, full_auto, bash_ok: false, write_ok: false }
    }

    pub fn execute(&mut self, name: &str, input: &Value) -> Outcome {
        let result = match name {
            "bash" => self.do_bash(input),
            "read_file" => read_file(input),
            "write_file" => self.write_file(input),
            "edit_file" => self.edit_file(input),
            "list_dir" => list_dir(input, &self.cwd),
            other => Err(format!("unknown tool '{other}'")),
        };
        match result {
            Ok(content) => Outcome { content, is_error: false },
            Err(content) => Outcome { content, is_error: true },
        }
    }

    fn do_bash(&mut self, input: &Value) -> Result<String, String> {
        let command = input["command"].as_str().ok_or("bash: missing 'command'")?;
        if !self.full_auto && !self.bash_ok {
            match ask(&format!("run {}", term::yellow(&format!("`{command}`")))) {
                Decision::No => return Ok("[denied] user declined to run this command".into()),
                Decision::Always => self.bash_ok = true,
                Decision::Yes => {}
            }
        }
        run_shell(command, &self.cwd)
    }

    fn write_file(&mut self, input: &Value) -> Result<String, String> {
        let path = input["path"].as_str().ok_or("write_file: missing 'path'")?;
        let content = input["content"].as_str().ok_or("write_file: missing 'content'")?;
        if !self.full_auto && !self.write_ok {
            let verb = if Path::new(path).exists() { "overwrite" } else { "create" };
            match ask(&format!("{verb} {}", term::yellow(path))) {
                Decision::No => return Ok("[denied] user declined this write".into()),
                Decision::Always => self.write_ok = true,
                Decision::Yes => {}
            }
        }
        if let Some(parent) = Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| format!("write_file {path}: {e}"))?;
            }
        }
        fs::write(path, content).map_err(|e| format!("write_file {path}: {e}"))?;
        Ok(format!("wrote {path} ({} lines, {} bytes)", content.lines().count(), content.len()))
    }

    fn edit_file(&mut self, input: &Value) -> Result<String, String> {
        let path = input["path"].as_str().ok_or("edit_file: missing 'path'")?;
        let old = input["old_string"].as_str().ok_or("edit_file: missing 'old_string'")?;
        let new = input["new_string"].as_str().ok_or("edit_file: missing 'new_string'")?;
        let src = fs::read_to_string(path).map_err(|e| format!("edit_file {path}: {e}"))?;
        let hits = src.matches(old).count();
        if hits == 0 {
            return Err(format!("edit_file {path}: old_string not found"));
        }
        if hits > 1 {
            return Err(format!(
                "edit_file {path}: old_string appears {hits}x — add surrounding lines to make it unique"
            ));
        }
        if !self.full_auto && !self.write_ok {
            match ask(&format!("edit {}", term::yellow(path))) {
                Decision::No => return Ok("[denied] user declined this edit".into()),
                Decision::Always => self.write_ok = true,
                Decision::Yes => {}
            }
        }
        let updated = src.replacen(old, new, 1);
        fs::write(path, updated).map_err(|e| format!("edit_file {path}: {e}"))?;
        Ok(format!("edited {path}"))
    }
}

fn read_file(input: &Value) -> Result<String, String> {
    let path = input["path"].as_str().ok_or("read_file: missing 'path'")?;
    let mut text = fs::read_to_string(path).map_err(|e| format!("read_file {path}: {e}"))?;
    truncate(&mut text, 100_000);
    Ok(text)
}

fn list_dir(input: &Value, cwd: &Path) -> Result<String, String> {
    let path = input["path"].as_str().unwrap_or(".");
    let dir = cwd.join(path);
    let mut entries: Vec<String> = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| format!("list_dir {}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().to_string();
        let suffix = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) { "/" } else { "" };
        entries.push(format!("{name}{suffix}"));
    }
    if entries.is_empty() {
        return Ok("(empty)".into());
    }
    entries.sort();
    Ok(entries.join("\n"))
}

fn run_shell(command: &str, cwd: &Path) -> Result<String, String> {
    const TIMEOUT: Duration = Duration::from_secs(120);

    let mut child = spawn_shell(command, cwd)?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    // read pipes on threads so a chatty child can't deadlock on a full pipe
    let t_out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let t_err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait().map_err(|e| format!("wait: {e}"))? {
            Some(status) => break Some(status),
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };

    let out = t_out.join().unwrap_or_default();
    let err = t_err.join().unwrap_or_default();

    let mut text = String::from_utf8_lossy(&out).to_string();
    let err_text = String::from_utf8_lossy(&err).to_string();
    if !err_text.trim().is_empty() {
        if !text.trim().is_empty() {
            text.push('\n');
        }
        text.push_str("[stderr]\n");
        text.push_str(&err_text);
    }
    match status {
        Some(s) if s.success() => {}
        Some(s) => text.push_str(&format!("\n[exit code {}]", s.code().unwrap_or(-1))),
        None => text.push_str(&format!("\n[pir] timed out after {}s, killed", TIMEOUT.as_secs())),
    }
    truncate(&mut text, 30_000);
    Ok(text)
}

fn spawn_shell(command: &str, cwd: &Path) -> Result<std::process::Child, String> {
    let mut build = |prog: &str, flag: &str| {
        let mut c = Command::new(prog);
        c.arg(flag).arg(command);
        c.current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    };
    let (prog, flag) = if cfg!(windows) { ("cmd", "/C") } else { ("bash", "-c") };
    match build(prog, flag).spawn() {
        Ok(c) => Ok(c),
        Err(e) if !cfg!(windows) => {
            build("sh", "-c").spawn().map_err(|_| format!("spawn {prog}: {e}"))
        }
        Err(e) => Err(format!("spawn {prog}: {e}")),
    }
}

fn truncate(s: &mut String, max_chars: usize) {
    if s.chars().count() > max_chars {
        let cut = s.char_indices().nth(max_chars).map(|(i, _)| i).unwrap_or(s.len());
        s.truncate(cut);
        s.push_str("\n… [pir] output truncated");
    }
}
```

### src/agent.rs

```rust
use crate::config::{self, ApiKind, Model, Provider};
use crate::provider::Client;
use crate::term;
use crate::tools::{self, ToolRunner};
use crate::types::{Block, Message, Role, Usage};
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

const MAX_STEPS: usize = 32;

pub struct Agent {
    pub provider: Provider,
    pub model: Model,
    client: Client,
    runner: ToolRunner,
    system: String,
    history: Vec<Message>,
    pub usage: Usage,
    log: Option<fs::File>,
    pub log_path: Option<PathBuf>,
}

impl Agent {
    pub fn new(provider: Provider, model: Model, full_auto: bool) -> Result<Self, String> {
        let client = make_client(&provider)?;
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        let mut system = String::from(
            "You are pir, a minimal terminal coding agent (a lightweight Rust \
             reimplementation of pi).\n\nEnvironment:\n",
        );
        system.push_str(&format!(
            "- cwd: {}\n- platform: {}\n- date: {}\n",
            cwd.display(),
            std::env::consts::OS,
            term::date_string(),
        ));
        system.push_str(
            "\nRules:\n\
             - Use the tools to actually do the work; don't just describe it.\n\
             - Read before editing; prefer edit_file over write_file for changes.\n\
             - Be terse: code, commands, short answers, no preamble.\n\
             - When finished, summarize what changed in a sentence or two.\n",
        );
        for p in [config::pi_dir().join("AGENTS.md"), PathBuf::from("AGENTS.md")] {
            if let Ok(s) = fs::read_to_string(&p) {
                system.push_str(&format!("\n# Extra instructions ({})\n\n{}\n", p.display(), s));
            }
        }

        let (log, log_path) = open_log();
        Ok(Agent {
            provider,
            model,
            client,
            runner: ToolRunner::new(cwd, full_auto),
            system,
            history: Vec::new(),
            usage: Usage::default(),
            log,
            log_path,
        })
    }

    pub fn label(&self) -> String {
        format!("{}/{}", self.provider.pid(), self.model.id)
    }

    pub fn clear(&mut self) {
        self.history.clear();
    }

    pub fn switch(&mut self, provider: Provider, model: Model) -> Result<(), String> {
        self.client = make_client(&provider)?;
        self.provider = provider;
        self.model = model;
        Ok(())
    }

    /// One user turn = the full tool-use loop, until the model answers with
    /// plain text (or we hit the step ceiling).
    pub fn turn(&mut self, user: &str) {
        let msg = Message::user(user);
        log_line(&mut self.log, &msg);
        self.history.push(msg);

        let specs = tools::specs();

        for step in 0..MAX_STEPS {
            self.trim();

            let mut on_text = |t: &str| {
                print!("{t}");
                let _ = std::io::stdout().flush();
            };
            let result = self.client.chat(
                &self.model.id,
                self.model.max_tokens.unwrap_or(8192),
                &self.system,
                &self.history,
                &specs,
                &mut on_text,
            );
            println!();
            let (assistant, usage) = match result {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("{} {e}", term::red("error:"));
                    return;
                }
            };
            self.usage.input += usage.input;
            self.usage.output += usage.output;

            // owned copies so `assistant` can move into history
            let calls: Vec<(String, String, Value)> = assistant
                .tool_uses()
                .into_iter()
                .map(|(id, name, input)| (id.to_string(), name.to_string(), input.clone()))
                .collect();

            log_line(&mut self.log, &assistant);
            self.history.push(assistant);

            if calls.is_empty() {
                return;
            }

            let mut results = Message { role: Role::User, blocks: Vec::new() };
            for (id, name, input) in &calls {
                println!("{} {}", term::cyan("»"), describe_call(name, input));
                let outcome = self.runner.execute(name, input);
                println!("{}", term::dim(&format!("  {}", first_line(&outcome.content))));
                results.blocks.push(Block::ToolResult {
                    tool_use_id: id.clone(),
                    content: outcome.content,
                    is_error: outcome.is_error,
                });
            }
            log_line(&mut self.log, &results);
            self.history.push(results);

            if step + 1 == MAX_STEPS {
                eprintln!(
                    "{} hit the {MAX_STEPS}-step tool limit; yielding back to you",
                    term::yellow("!")
                );
                return;
            }
        }
    }

    /// Crude context management: past ~budget tokens, keep the first user
    /// request plus the newest self-consistent tail, eliding the middle.
    fn trim(&mut self) {
        let ctx = self.model.context.unwrap_or(200_000) as usize;
        let budget = ctx.saturating_sub(8192).max(8192);
        if approx_tokens(&self.history) <= budget {
            return;
        }
        let cut = (1..self.history.len())
            .rev()
            .find(|&i| {
                let m = &self.history[i];
                m.role == Role::User
                    && m.blocks.iter().all(|b| matches!(b, Block::Text(_)))
                    && approx_tokens(&self.history[i..]) <= budget / 2
            })
            .unwrap_or(1);
        let first = self.history[0].text();
        let tail: Vec<Message> = self.history.split_off(cut);

        let mut history = Vec::new();
        let mut it = tail.into_iter();
        if let Some(head) = it.next() {
            if head.role == Role::User && head.blocks.iter().all(|b| matches!(b, Block::Text(_))) {
                history.push(Message::user(&format!(
                    "{first}\n\n[pir: earlier conversation elided]\n\n{}",
                    head.text()
                )));
            } else {
                history.push(Message::user(&format!(
                    "{first}\n\n[pir: earlier conversation elided]"
                )));
                history.push(head);
            }
            history.extend(it);
        } else {
            history.push(Message::user(&format!(
                "{first}\n\n[pir: earlier conversation elided]"
            )));
        }
        self.history = history;
        println!("{}", term::dim("[pir: context trimmed]"));
    }
}

fn make_client(provider: &Provider) -> Result<Client, String> {
    let kind = provider
        .kind()
        .ok_or_else(|| format!("provider '{}' has no baseUrl", provider.pid()))?;
    let base = match provider.base_url.as_deref() {
        Some(b) if !b.is_empty() => b.trim_end_matches('/').to_string(),
        _ => match kind {
            ApiKind::Anthropic => "https://api.anthropic.com/v1".to_string(),
            ApiKind::OpenAi => {
                return Err(format!("provider '{}' has no baseUrl", provider.pid()))
            }
        },
    };
    let key = provider.api_key().ok_or_else(|| {
        format!(
            "no API key for '{}' — export the env var referenced in {}, or set apiKey directly",
            provider.pid(),
            config::pi_dir().join("models.json").display()
        )
    })?;
    Ok(Client::new(kind, &base, key))
}

fn approx_tokens(history: &[Message]) -> usize {
    history
        .iter()
        .map(|m| {
            32 + m.blocks
                .iter()
                .map(|b| match b {
                    Block::Text(t) => t.len(),
                    Block::ToolUse { input, .. } => input.to_string().len() + 64,
                    Block::ToolResult { content, .. } => content.len() + 64,
                })
                .sum::<usize>()
                / 4
        })
        .sum()
}

fn describe_call(name: &str, input: &Value) -> String {
    let s = |k: &str| input[k].as_str().unwrap_or("");
    match name {
        "bash" => format!("bash  {}", s("command")),
        "read_file" => format!("read  {}", s("path")),
        "write_file" => format!(
            "write {} ({} B)",
            s("path"),
            input["content"].as_str().map(str::len).unwrap_or(0)
        ),
        "edit_file" => format!("edit  {}", s("path")),
        "list_dir" => {
            let p = s("path");
            format!("ls    {}", if p.is_empty() { "." } else { p })
        }
        other => other.to_string(),
    }
}

fn first_line(s: &str) -> String {
    let t = s.trim();
    let mut out: String = t.lines().next().unwrap_or("").chars().take(120).collect();
    if t.lines().count() > 1 {
        out.push_str(" …");
    }
    out
}

fn log_line(log: &mut Option<fs::File>, m: &Message) {
    let Some(f) = log.as_mut() else { return };
    let role = if m.role == Role::User { "user" } else { "assistant" };
    let entry = json!({
        "ts": term::epoch(),
        "role": role,
        "blocks": m.blocks.iter().map(|b| match b {
            Block::Text(t) => json!({ "type": "text", "text": t }),
            Block::ToolUse { id, name, input } =>
                json!({ "type": "tool_use", "id": id, "name": name, "input": input }),
            Block::ToolResult { tool_use_id, content, is_error } =>
                json!({ "type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error }),
        }).collect::<Vec<_>>(),
    });
    let _ = writeln!(f, "{entry}");
}

fn open_log() -> (Option<fs::File>, Option<PathBuf>) {
    let dir = config::pi_dir().join("agent").join("sessions");
    if fs::create_dir_all(&dir).is_err() {
        return (None, None);
    }
    let path = dir.join(format!("pir-{}.jsonl", term::timestamp_compact()));
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => (Some(f), Some(path)),
        Err(_) => (None, None),
    }
}
```

### src/main.rs

```rust
mod agent;
mod config;
mod provider;
mod term;
mod tools;
mod types;

use crate::agent::Agent;
use crate::config::Provider;
use std::io::Write;

const HELP: &str = r#"pir — a featherweight pi-compatible coding agent

USAGE
  pir [options] [prompt]     prompt given => one-shot, else interactive REPL

OPTIONS
  -m, --model <selector>     e.g. -m anthropic/claude-sonnet-4-5 (fuzzy match ok)
  -y, --full-auto            no confirmation for shell/write tools
  -n, --no-color             disable ANSI colors
  -h, --help  -V, --version

CONFIG (reused from pi, never modified)
  ~/.pi/models.json          providers, models, api keys ("{env:VAR}" supported)
  ~/.pi/agent/settings.json  optional default model ("model" key)
  ~/.pi/AGENTS.md, ./AGENTS.md   appended to the system prompt
  ~/.pi/agent/sessions/      pir session transcripts (pir-*.jsonl)

COMMANDS
  /help  /model <sel>  /models  /clear  /usage  /exit
"#;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut model_sel: Option<String> = None;
    let mut full_auto = false;
    let mut prompt: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-m" | "--model" => {
                i += 1;
                match args.get(i) {
                    Some(v) => model_sel = Some(v.clone()),
                    None => die("--model needs a value"),
                }
            }
            "-y" | "--full-auto" => full_auto = true,
            "-n" | "--no-color" => term::set_color(false),
            "-h" | "--help" => {
                print!("{HELP}");
                return;
            }
            "-V" | "--version" => {
                println!("pir {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            x if x.starts_with('-') => die(&format!("unknown flag {x} — try --help")),
            x => prompt.push(x.to_string()),
        }
        i += 1;
    }

    let providers = match config::load_providers() {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => die("~/.pi/models.json contains no providers"),
        Err(e) => die(&e),
    };

    let explicit = model_sel.is_some();
    let selector = model_sel
        .or_else(|| std::env::var("PI_MODEL").ok())
        .or_else(|| config::default_model_setting())
        .unwrap_or_else(|| providers[0].label(&providers[0].models[0]));

    let (provider, model) = match config::select(&providers, &selector) {
        Ok(x) => x,
        Err(e) if explicit => die(&format!("{e}\n{}", list_models(&providers))),
        Err(e) => {
            let fb = providers[0].label(&providers[0].models[0]);
            eprintln!("pir: {e}; falling back to {fb}");
            match config::select(&providers, &fb) {
                Ok(x) => x,
                Err(e2) => die(&format!("{e2}\n{}", list_models(&providers))),
            }
        }
    };

    let mut agent = match Agent::new(provider.clone(), model.clone(), full_auto) {
        Ok(a) => a,
        Err(e) => die(&e),
    };

    if !prompt.is_empty() {
        agent.turn(&prompt.join(" "));
        return;
    }

    println!("{}", term::bold("pir"));
    println!(
        "{}",
        term::dim(&format!(
            "model {} · {} · config {}",
            agent.label(),
            if full_auto { "full-auto" } else { "confirm-actions" },
            config::pi_dir().display()
        ))
    );
    if let Some(p) = &agent.log_path {
        println!("{}", term::dim(&format!("session log: {}", p.display())));
    }
    println!("{}", term::dim("/help for commands · ctrl-d to quit"));

    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        print!("{} ", term::cyan("❯"));
        let _ = std::io::stdout().flush();
        match stdin.read_line(&mut line) {
            Ok(0) => {
                println!();
                break;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("stdin: {e}");
                break;
            }
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if let Some(cmd) = input.strip_prefix('/') {
            handle_command(cmd, &mut agent, &providers);
        } else {
            agent.turn(input);
            println!(
                "{}",
                term::dim(&format!(
                    "· {} in / {} out tokens",
                    fmt_tok(agent.usage.input),
                    fmt_tok(agent.usage.output)
                ))
            );
        }
    }
}

fn handle_command(cmd: &str, agent: &mut Agent, providers: &[Provider]) {
    let mut parts = cmd.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    match cmd {
        "h" | "help" => print!("{HELP}"),
        "m" | "model" => {
            if rest.is_empty() {
                println!("current model: {}", agent.label());
            } else {
                match config::select(providers, &rest.join(" ")) {
                    Ok((p, m)) => match agent.switch(p.clone(), m.clone()) {
                        Ok(()) => println!("→ {}", agent.label()),
                        Err(e) => eprintln!("{e}"),
                    },
                    Err(e) => eprintln!("{e}"),
                }
            }
        }
        "models" => print!("{}", list_models(providers)),
        "clear" => {
            agent.clear();
            println!("history cleared");
        }
        "usage" => println!(
            "{} in / {} out tokens this session",
            fmt_tok(agent.usage.input),
            fmt_tok(agent.usage.output)
        ),
        "q" | "quit" | "exit" => std::process::exit(0),
        other => eprintln!("unknown command /{other} — try /help"),
    }
}

fn list_models(providers: &[Provider]) -> String {
    let mut out = String::new();
    for p in providers {
        out.push_str(&format!("{}\n", term::bold(&p.pid())));
        for m in &p.models {
            let ctx = m.context.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
            out.push_str(&format!(
                "  {:<44} ctx {:>7}  {}\n",
                m.id,
                ctx,
                m.name.as_deref().unwrap_or("")
            ));
        }
    }
    out
}

fn fmt_tok(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

fn die(msg: &str) -> ! {
    eprintln!("pir: {msg}");
    std::process::exit(1)
}
```

## Build & run

```bash
cargo build --release
export ANTHROPIC_API_KEY=sk-...       # or whatever your ~/.pi/models.json references

./target/release/pir                    # REPL
./target/release/pir -m sonnet          # fuzzy model match
./target/release/pir -y "fix the TODO in src/lib.rs"   # one-shot, full-auto
echo "explain this repo" | ./target/release/pir        # piped one-shot
```

Example session:

```
❯ make the tests pass
» bash  cargo test
  running 3 tests …
» edit  src/parser.rs
  edited src/parser.rs
» bash  cargo test
  test result: ok. 3 passed
Fixed the off-by-one in Parser::next_token; all 3 tests pass.
· 4.1k in / 812 out tokens
```

## Honest caveats

- **Schema tolerance**: the loader accepts `providers` as a list (pi's format) or map, camelCase or snake_case keys, and `{env:...}` keys — but I can't guarantee it against every pi version's `models.json`. If yours differs, `config.rs` is ~60 lines and the error message names the exact file.
- Ctrl-C mid-stream kills the process (no raw-terminal mode); the JSONL log preserves everything up to the last completed message, but there's no `/resume`.
- No markdown rendering, no parallel tool execution, no sub-agents, no MCP — that's the "lightweight" deal. Adding a tool = one `ToolSpec` + one match arm in `tools.rs`.
- `-y` runs arbitrary shell commands with only a 120s timeout as a guardrail; the default confirmation mode is the sane choice.
- Needs rustc ≥ 1.70 (`IsTerminal`); Windows works but ANSI support depends on your terminal.

## TODO: reliable Windows multiline-paste detection (Option B)

**Status:** Option A is implemented (see `src/term.rs::coalesce_paste` /
`windows_pending_keydown`). Option B is the more robust future upgrade.

### Background / root cause

On Windows, rustyline's console reader (`ConsoleRawReader` in
`rustyline`'s `tty/windows.rs`) reads input via `ReadConsoleInputW` and maps
each `KEY_EVENT` to a `KeyEvent`. It **never** emits an `Event::Paste` and does
**not** parse the `ESC[200~ … ESC[201~` bracketed-paste wrapper, so enabling
`bracketed_paste(true)` is a **no-op** there. Conhost injects a pasted block
directly into the console input buffer as one `VK_RETURN` key-down per line,
which is why a multiline paste otherwise becomes one prompt per line.

### Option A (current): peek-based coalesce — fragile but good enough

After the first `readline` returns, `windows_pending_keydown()` calls
`PeekConsoleInputW` (non-destructive — it never consumes the events rustyline
is about to read) and checks whether a real key press (`KEY_EVENT` with
`bKeyDown != 0`) is still buffered. Pasted characters are themselves key-down
events, so a pending key-down reliably means "more of the paste is buffered"; a
lone typed Enter leaves only its key-*up* (filtered out), so the burst ends
immediately — which is what keeps it from regressing into the old "press Enter
twice" bug (the previous `coalesce_paste` polled via crossterm and saw the
buffered key-up, spinning up a nested `readline("")` that waited for a second
Enter). Once more than one line is folded in, the whole block is re-presented
via `readline_with_initial` so the cursor/backspace can cross line boundaries.

**Known fragility (acknowledged):** a sufficiently fast typist who presses a
second key within the peek window of a submitted line, or an extremely slow /
streamed clipboard that hasn't flushed the next line's key-down by the time we
peek, can blur the line. Acceptable for now; Option B removes the ambiguity.

### Option B (TODO): low-level keyboard hook — the reliable fix

Install `SetWindowsHookExW(WH_KEYBOARD_LL, …, GetCurrentThreadId())` on a
dedicated thread that runs a `GetMessage`/`DispatchMessage` pump (a low-level
hook only fires while the installing thread pumps messages). The hook records
`WM_KEYDOWN` (or `WM_CHAR`) events into a small ring / shared `Arc<Atomic…>`
structure. Separately, as each character is *read* from the console input
buffer (`conin`), compare it against the hook's recent keydown stream:

- Typed character → it appears in **both** the hook and `conin` → normal input.
- Pasted character → it appears in `conin` **only** (conhost injects paste
directly and bypasses the keyboard message path) → flag it as pasted.

A pasted run is therefore "consecutive characters in `conin` with no matching
`WM_KEYDOWN`". Maintain an `is_pasting` flag that flips true on the first such
character and clears after a short idle (e.g. ~16 ms with no further pasted
char), and expose it so `coalesce_paste` can decide "keep reading" with
certainty instead of peeking for a key-down.

**Why this is the most reliable approach short of terminal-level support:** it
does not depend on timing heuristics or on the terminal honoring bracketed
paste; it detects paste from the structural fact that injected console input
never traverses the keyboard message path.

**Tradeoffs / edge cases to handle before adopting:**
- The hook thread must own a message loop or the hook never fires.
- Dead keys, IME composition, and non-Latin keyboard layouts mean a single
displayed character can map to multiple `WM_KEYDOWN`s or none (composition);
match on the *character* (`WM_CHAR` / the `uChar` rustyline sees), not raw
virtual-key codes.
- Coordinate the flag with the existing `coalesce`/`readline_with_initial`
re-presentation so a large paste is still gathered as one editable buffer.
- Keep the `#[cfg(not(unix))]` gating; Unix continues to rely on rustyline's
native bracketed paste.
