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
