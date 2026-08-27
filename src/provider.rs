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
