use crate::config::{self, ApiKind, Model, Provider};
use crate::goal::{GoalStatus, GoalStore};
use crate::notify::{AgentEvent, SharedBus};
use crate::plugin::{Outcome, Registry};
use crate::provider::Client;
use crate::term;
use crate::types::{Block, Message, Role, Usage};
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct Agent {
    pub provider: Provider,
    pub model: Model,
    client: Client,
    registry: Registry,
    system: String,
    history: Vec<Message>,
    pub usage: Usage,
    log: Option<fs::File>,
    pub log_path: Option<PathBuf>,
    goal_store: Option<GoalStore>,
    notify: SharedBus,
    /// When true, the agent runs silently (no token streaming or per-tool
    /// prints to the terminal). Used for backgrounded sessions, which still
    /// persist everything to the session log and emit notifications.
    quiet: bool,
    /// Cooperative cancellation flag. Set by the REPL (e.g. on ctrl-c) to ask
    /// the running turn to stop at the next safe boundary. The turn checks it
    /// before each model call and after each tool batch, so it never aborts
    /// mid-tool; the in-progress step always completes first.
    cancel: Arc<AtomicBool>,
}

impl Agent {
    /// `resume_from`, if set, continues the given session's log file instead
    /// of starting a fresh one (its parent-shell tag is preserved). `quiet`
    /// suppresses all terminal output (used for backgrounded sessions). `bus`
    /// is the shared notification bus all agents publish to.
    pub fn new(
        provider: Provider,
        model: Model,
        full_auto: bool,
        quiet: bool,
        bus: SharedBus,
        resume_from: Option<&PathBuf>,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let client = make_client(&provider)?;
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        let mut registry = Registry::new(cwd.clone(), full_auto);
        crate::register_all(&mut registry);

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

        let (log, log_path) = open_log(resume_from);

        // Goal-continuation: when resuming an existing session, attach any
        // goal file that lives next to the log so `pir -c` can continue where
        // it left off. Fresh sessions start with no goal until the model sets
        // one via the `update_goal` tool.
        let goal_store = if resume_from.is_some() {
            GoalStore::attach(log_path.as_deref())
        } else {
            None
        };

        Ok(Agent {
            provider,
            model,
            client,
            registry,
            system,
            history: Vec::new(),
            usage: Usage::default(),
            log,
            log_path,
            goal_store,
            notify: bus,
            quiet,
            cancel,
        })
    }

    /// Inject (or refresh) the current goal snapshot into the system prompt so
    /// the model always sees the live plan without it being part of `history`.
    fn refresh_system(&mut self) {
        let mut system = String::from(
            "You are pir, a minimal terminal coding agent (a lightweight Rust \
             reimplementation of pi).\n\nEnvironment:\n",
        );
        system.push_str(&format!(
            "- cwd: {}\n- platform: {}\n- date: {}\n",
            std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| ".".into()),
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
        if let Some(store) = &self.goal_store {
            system.push_str("\n# Current goal (persisted — survives interrupts; resume with `pir -c`)\n\n");
            system.push_str(&store.goal.summary());
            system.push_str("\nWork the next pending step. Update progress with the update_goal tool.\n");
        }
        self.system = system;
    }

    pub fn label(&self) -> String {
        format!("{}/{}", self.provider.pid(), self.model.id)
    }

    /// Whether this agent is currently running in the background.
    pub fn is_quiet(&self) -> bool {
        self.quiet
    }

    /// Read-only access to the chosen provider/model (for spawning background
    /// sessions that continue on the same configuration).
    pub fn provider(&self) -> Provider {
        self.provider.clone()
    }
    pub fn model(&self) -> Model {
        self.model.clone()
    }

    /// The path of the session transcript (used to foreground a session).
    pub fn log_path(&self) -> Option<&PathBuf> {
        self.log_path.as_ref()
    }

    pub fn switch(&mut self, provider: Provider, model: Model) -> Result<(), String> {
        self.client = make_client(&provider)?;
        self.provider = provider;
        self.model = model;
        Ok(())
    }

    pub fn clear(&mut self) {
        self.history.clear();
    }

    /// Begin a new goal for this session, persisting it next to the log.
    pub fn start_goal(&mut self, objective: &str) {
        let log_path = self.log_path.clone().unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join("pir.goal.json")
        });
        let store = GoalStore::new(&log_path, objective);
        store.save();
        self.goal_store = Some(store);
        self.refresh_system();
    }

    /// Reattach to an existing goal file path (used by `pir -c`).
    pub fn attach_goal(&mut self, log_path: &Path) {
        if let Some(store) = GoalStore::attach(Some(log_path)) {
            self.goal_store = Some(store);
            self.refresh_system();
        }
    }

    pub fn goal_snapshot(&self) -> Option<String> {
        self.goal_store.as_ref().map(|s| s.goal.summary())
    }

    /// Intercept the `update_goal` tool: mutate and persist the goal, then
    /// return its feedback so the model sees the change. Other tools fall
    /// through to the registry. Returns `None` for non-goal tools.
    fn run_goal_tool(&mut self, name: &str, input: &Value) -> Option<Outcome> {
        if name != "update_goal" {
            return None;
        }
        let store = match self.goal_store.as_mut() {
            Some(s) => s,
            None => {
                return Some(Outcome {
                    content: "No active goal. Call update_goal with action set_objective first.".into(),
                    is_error: true,
                })
            }
        };
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");

        let report = match action {
            "set_objective" => {
                if let Some(o) = input.get("objective").and_then(Value::as_str) {
                    if !o.trim().is_empty() {
                        store.goal.objective = o.trim().to_string();
                    }
                }
                format!("objective set: {}", store.goal.objective)
            }
            "add_steps" => match input.get("steps").and_then(Value::as_array) {
                Some(arr) => {
                    let descs: Vec<String> = arr
                        .iter()
                        .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                        .filter(|s| !s.is_empty())
                        .collect();
                    if descs.is_empty() {
                        "add_steps: no non-empty step descriptions provided".to_string()
                    } else {
                        let ids = store.goal.add_steps(&descs);
                        format!("added steps {}", ids.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(" "))
                    }
                }
                None => "add_steps: missing 'steps' array".to_string(),
            },
            "set_status" => match input.get("status").and_then(Value::as_str) {
                Some(s) => match crate::goal::parse_goal_status(s) {
                    Some(st) => {
                        store.goal.status = st;
                        format!("goal status -> {}", st.label())
                    }
                    None => format!("set_status: unknown status '{s}' (active|complete|blocked|aborted)"),
                },
                None => "set_status: missing 'status'".to_string(),
            },
            "set_step" => {
                let id = match input.get("step_id").and_then(Value::as_u64) {
                    Some(n) => n as usize,
                    None => return Some(Outcome { content: "set_step: missing integer 'step_id'".into(), is_error: true }),
                };
                let st = match input.get("step_status").and_then(Value::as_str) {
                    Some(s) => match crate::goal::parse_step_status(s) {
                        Some(st) => st,
                        None => {
                            return Some(Outcome {
                                content: format!("set_step: unknown step status '{s}' (pending|in_progress|done|blocked)"),
                                is_error: true,
                            })
                        }
                    },
                    None => {
                        return Some(Outcome { content: "set_step: missing 'step_status'".into(), is_error: true })
                    }
                };
                let note = input.get("note").and_then(Value::as_str).unwrap_or("");
                store.goal.update_step(id, st, note);
                format!("step #{id} -> {}", st.label())
            }
            "note" => {
                let n = input.get("note").and_then(Value::as_str).unwrap_or("").trim();
                if !n.is_empty() {
                    if store.goal.notes.is_empty() {
                        store.goal.notes = n.to_string();
                    } else {
                        store.goal.notes.push_str(&format!("\n{n}"));
                    }
                }
                "note recorded".to_string()
            }
            other => return Some(Outcome { content: format!("update_goal: unknown action '{other}'"), is_error: true }),
        };

        let summary = {
            store.save();
            store.goal.summary()
        };
        // `store` (the mutable borrow) is dropped here, so `self` can be
        // re-borrowed by `refresh_system`.
        self.refresh_system();
        Some(Outcome { content: format!("{report}\n\n{summary}"), is_error: false })
    }

    /// Replay the persisted transcript of `session` back into history so a
    /// resumed session keeps its prior conversation. Also prints a short
    /// summary. Returns the number of turns replayed.
    pub fn load_session(&mut self, session: &PathBuf) -> usize {
        let mut turns = 0usize;
        let Some(f) = File::open(session).ok() else { return 0 };
        let mut pending: Option<Message> = None;
        for line in std::io::BufReader::new(f).lines().flatten() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let role = v.get("role").and_then(|r| r.as_str()).unwrap_or("");
            let blocks: Vec<Block> = v
                .get("blocks")
                .and_then(|b| b.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|b| match b.get("type").and_then(|t| t.as_str()) {
                            Some("text") => b.get("text").and_then(|t| t.as_str()).map(|t| Block::Text(t.to_string())),
                            Some("tool_use") => Some(Block::ToolUse {
                                id: b.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                                name: b.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                                input: b.get("input").cloned().unwrap_or(serde_json::Value::Null),
                            }),
                            Some("tool_result") => Some(Block::ToolResult {
                                tool_use_id: b.get("tool_use_id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                                content: b.get("content").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                                is_error: b.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false),
                            }),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();

            if role == "user" {
                // A fresh user prompt begins a new turn.
                if let Some(m) = pending.take() {
                    self.history.push(m);
                }
                pending = Some(Message { role: Role::User, blocks });
                turns += 1;
            } else {
                // Assistant message: if we already have a pending user turn,
                // pair them; otherwise just queue the assistant alone.
                if let Some(mut u) = pending.take() {
                    u.blocks.extend(blocks);
                    self.history.push(u);
                } else {
                    self.history.push(Message { role: Role::Assistant, blocks });
                }
            }
        }
        if let Some(m) = pending.take() {
            self.history.push(m);
        }

        if turns > 0 {
            let first = self.history.iter().find_map(|m| {
                if m.role == Role::User {
                    m.text().lines().next().map(|l| l.to_string())
                } else {
                    None
                }
            });
            println!(
                "{}",
                term::dim(&format!(
                    "resumed session ({} turns){}",
                    turns,
                    first.map(|f| format!(": {f}")).unwrap_or_default()
                ))
            );
        }
        turns
    }

    /// Drive the agent to complete the active goal. Repeatedly prompts the
    /// model with the next pending step (or a completion nudge) until the goal
    /// reaches a terminal state or the model stops making tool calls. This is
    /// the `pir -c` / `/continue` entry point and is resilient to interrupts:
    /// each `update_goal` call persists, so re-running `pir -c` picks up where
    /// the last run stopped.
    pub fn continue_goal(&mut self) {
        // Snapshot objective + pre-checks so we don't hold a borrow of
        // `goal_store` across the `turn` call (which needs `&mut self`).
        let (objective, already_done) = match &self.goal_store {
            Some(s) => (s.goal.objective.clone(), s.goal.status == GoalStatus::Complete),
            None => {
                eprintln!("{} no active goal — start one with /goal <objective>", term::red("error:"));
                return;
            }
        };
        if already_done {
            println!("{} goal already complete: {}", term::cyan("·"), objective);
            if let Some(s) = &self.goal_store {
                println!("{}", term::dim(&s.goal.summary()));
            }
            return;
        }

        println!("{} goal: {}", term::bold("continue"), objective);
        if let Some(s) = &self.goal_store {
            println!("{}", term::dim(&s.goal.summary()));
        }

        loop {
            // Read the live goal into locals; no outstanding borrow past here.
            let (terminal, pending) = match &self.goal_store {
                Some(s) => (
                    s.goal.status.is_terminal(),
                    s.goal
                        .next_step()
                        .map(|st| st.description.clone())
                        .unwrap_or_else(|| "proceed with the goal".to_string()),
                ),
                None => (true, String::new()),
            };
            if terminal {
                break;
            }

            let prompt = format!(
                "[continue goal] Next step: {pending}\n\
                 Work on it now. If the plan changed, revise the goal with update_goal, \
                 then mark the step done and move to the next. Stop only once all steps are \
                 done or the goal is complete/blocked."
            );
            let before = self.usage.output;
            if self.turn(&prompt).is_err() {
                println!("{} goal run errored; stopping", term::red("error:"));
                break;
            }
            let delta = self.usage.output - before;
            if delta == 0 {
                // Model produced no tool calls and nothing progressed.
                println!("{} model yielded without further progress; stopping", term::yellow("!"));
                break;
            }
        }

        let final_status = match &self.goal_store {
            Some(s) => s.goal.status.label().to_string(),
            None => "unknown".to_string(),
        };
        println!("{} goal {}: {}", term::bold("done"), final_status, objective);
        if let Some(s) = &self.goal_store {
            println!("{}", term::dim(&s.goal.summary()));
            if let Some(p) = s.path().to_str() {
                println!("{}", term::dim(&format!("goal saved: {p}")));
            }
        }
    }

    /// Print the active goal snapshot to the user (used by `/goal`).
    pub fn show_goal(&self) {
        match &self.goal_store {
            Some(s) => {
                println!("{}", term::bold("goal"));
                print!("{}", s.goal.summary());
                if let Some(p) = s.path().to_str() {
                    println!("{}", term::dim(&format!("saved: {p}")));
                }
            }
            None => println!("{} no active goal; start one with /goal <objective>", term::yellow("·")),
        }
    }

    /// One user turn = the full tool-use loop, which runs until the model
    /// answers with plain text (no tool calls). There is no fixed step cap;
    /// it yields only when the model stops asking for tools. Returns `Ok(())`
    /// if the turn completed (model finished with no further tool calls) or
    /// `Err(message)` if the provider/tool loop aborted. The caller decides
    /// which [`AgentEvent`] to surface (the REPL fires `Idle`; one-shot /
    /// background fire `TurnDone`/`Error`) so there is a single notification
    /// decision point per context.
    pub fn turn(&mut self, user: &str) -> Result<(), String> {
        let msg = Message::user(user);
        log_line(&mut self.log, &msg);
        self.history.push(msg);

        let specs = self.registry.specs();

        loop {
            // Cooperative cancellation: bail out at this safe boundary (start
            // of a new model call) if the REPL requested a stop.
            if self.cancel.load(Ordering::SeqCst) {
                self.cancel.store(false, Ordering::SeqCst);
                if !self.quiet {
                    println!("{}", term::dim("· turn cancelled"));
                }
                self.notify.publish(self.turn_done_event(), false);
                return Ok(());
            }
            self.trim();

            let mut on_text = |t: &str| {
                if !self.quiet {
                    print!("{t}");
                    let _ = std::io::stdout().flush();
                }
            };
            let result = self.client.chat(
                &self.model.id,
                self.model.max_tokens.unwrap_or(8192),
                &self.system,
                &self.history,
                &specs,
                &mut on_text,
            );
            if !self.quiet {
                println!();
            }
            let (assistant, usage) = match result {
                Ok(r) => r,
                Err(e) => {
                    if !self.quiet {
                        eprintln!("{} {e}", term::red("error:"));
                    }
                    self.notify.publish(AgentEvent::Error { message: e.clone() }, false);
                    if !self.quiet {
                        self.registry.on_turn_end(user);
                    }
                    return Err(e);
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
                self.notify.publish(self.turn_done_event(), false);
                if !self.quiet {
                    self.registry.on_turn_end(user);
                }
                return Ok(());
            }

            let mut results = Message { role: Role::User, blocks: Vec::new() };
            for (id, name, input) in &calls {
                if !self.quiet {
                    println!("{} {}", term::cyan("»"), describe_call(name, input));
                }
                let outcome = match self.run_goal_tool(name, input) {
                    Some(o) => o,
                    None => self.registry.execute(name, input),
                };
                if !self.quiet {
                    println!("{}", term::dim(&format!("  {}", first_line(&outcome.content))));
                }
                results.blocks.push(Block::ToolResult {
                    tool_use_id: id.clone(),
                    content: outcome.content,
                    is_error: outcome.is_error,
                });
            }
            log_line(&mut self.log, &results);
            self.history.push(results);

            // Cooperative cancellation: stop after this batch of tools
            // completes (the in-progress step always finishes first).
            if self.cancel.load(Ordering::SeqCst) {
                self.cancel.store(false, Ordering::SeqCst);
                if !self.quiet {
                    println!("{}", term::dim("· turn cancelled"));
                }
                self.notify.publish(self.turn_done_event(), false);
                return Ok(());
            }
        }
    }

    /// Publish an exit notification to the shared bus (called from one-shot /
    /// background paths). `oneshot = true` so `when: "oneshot"` policy applies.
    pub fn notify_on_exit(&self, event: AgentEvent) {
        self.notify.publish(event, true);
    }

    /// Build the `TurnDone` event for the current session's cumulative usage.
    /// Used by the one-shot / background exit paths, which have no meaningful
    /// per-turn duration.
    pub fn turn_done_event(&self) -> AgentEvent {
        AgentEvent::TurnDone {
            duration: std::time::Duration::ZERO,
            in_tokens: self.usage.input,
            out_tokens: self.usage.output,
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

fn open_log(resume_from: Option<&PathBuf>) -> (Option<fs::File>, Option<PathBuf>) {
    let dir = session_dir();
    if fs::create_dir_all(&dir).is_err() {
        return (None, None);
    }
    let path = match resume_from {
        Some(p) => p.clone(),
        None => dir.join(format!(
            "pir-{}-sh{}.jsonl",
            term::timestamp_compact(),
            term::parent_shell_pid()
        )),
    };
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => (Some(f), Some(path)),
        Err(_) => (None, None),
    }
}

/// Where session transcripts live. When running as a non-root per-project user
/// (`ai_X`), prefer the project's own `.pir/sessions` directory (which
/// `pir project init` chowns to that user); otherwise fall back to the global
/// `~/.pi/agent/sessions`.
fn session_dir() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Some(d) = crate::user::session_dir_for(&cwd) {
        return d;
    }
    // The project-local `.pir/sessions` dir may not exist on a fresh project;
    // if we can create it (i.e. we own `.pir`), prefer it over the global one.
    let local = cwd.join(".pir").join("sessions");
    if let Some(parent) = local.parent() {
        if parent.exists() && std::fs::create_dir_all(&local).is_ok() {
            return local;
        }
    }
    config::pi_dir().join("agent").join("sessions")
}
