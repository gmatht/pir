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
use std::sync::{Arc, Mutex};

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
    /// Follow-up prompts queued by extension backends during `on_turn_end`
    /// (e.g. the worktree extension asking the model to fix failing tests).
    /// The REPL drains these into its prompt queue after the turn finishes.
    continuations: Vec<String>,
    /// The most recent user prompt this agent is/was working on. Recorded so
    /// notifications can show *what* finished, not just "turn done".
    last_prompt: String,
    /// When true, the agent runs silently (no token streaming or per-tool
    /// prints to the terminal). Used for backgrounded sessions, which still
    /// persist everything to the session log and emit notifications.
    quiet: bool,
    /// Cooperative cancellation flag. Set by the REPL (e.g. on ctrl-c) to ask
    /// the running turn to stop at the next safe boundary. The turn checks it
    /// before each model call and after each tool batch, so it never aborts
    /// mid-tool; the in-progress step always completes first.
    cancel: Arc<AtomicBool>,
    /// Shared buffer the REPL fills with keystrokes the user types *while* a
    /// turn runs. The thinking spinner reads it so the user's input stays
    /// visible during "thinking" instead of being clobbered by competing stdout
    /// writers. The REPL owns the only other reference; it only ever writes.
    typeahead: Arc<Mutex<String>>,
    /// Optional cumulative token budget (in/out combined). When set, a turn
    /// stops *before* the next model call once the budget is exceeded, printing
    /// a banner. Off by default (None) — opt in via `--budget N` or
    /// `PIR_TOKEN_BUDGET`.
    token_budget: Option<u64>,
    /// Per-session undo stack of (target, backup) pairs. Before `write_file` /
    /// `edit_file` run, the previous file contents are snapshotted to a sidecar
    /// under `.pir/undo/`; `/undo` restores the most recent snapshot. Only file
    /// edits are checkpointed (bash is out of scope — the user can `git` it).
    undo_stack: Vec<(PathBuf, PathBuf)>,
}

impl Agent {
    /// `resume_from`, if set, continues the given session's log file instead
    /// of starting a fresh one (its parent-shell tag is preserved). `quiet`
    /// suppresses all terminal output (used for backgrounded sessions). `bus`
    /// is the shared notification bus all agents publish to. `typeahead` is a
    /// shared buffer the REPL fills with keystrokes typed while the turn runs;
    /// the thinking spinner reads it so the user's input is shown while the
    /// model thinks.
    pub fn new(
        provider: Provider,
        model: Model,
        full_auto: bool,
        quiet: bool,
        bus: SharedBus,
        resume_from: Option<&PathBuf>,
        cancel: Arc<AtomicBool>,
        typeahead: Arc<Mutex<String>>,
    ) -> Result<Self, String> {
        let client = make_client(&provider, cancel.clone())?;
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
            typeahead,
            last_prompt: String::new(),
            continuations: Vec::new(),
            token_budget: None,
            undo_stack: Vec::new(),
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

    /// Short project/cwd label for notifications (e.g. the basename of the cwd,
    /// "rpi"), so a pop-up can say which project finished. Empty if it can't be
    /// determined.
    pub fn project_label(&self) -> String {
        std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_default()
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
        self.client = make_client(&provider, self.cancel.clone())?;
        self.provider = provider;
        self.model = model;
        // Remember the active model next to the session log so a resumed
        // session starts on the same model instead of the global default.
        self.persist_model();
        Ok(())
    }

    /// Persist the active provider/model to a sidecar (`<log>.model`) so the
    /// choice survives a resume. Silent if there's no log (one-shot).
    fn persist_model(&self) {
        if let Some(p) = &self.log_path {
            let path = p.with_extension("model");
            let _ = std::fs::write(&path, format!("{}/{}", self.provider.pid(), self.model.id));
        }
    }

    /// Load a previously persisted model choice (from `<log>.model`) for a
    /// resumed session. Returns the `provider/model` label, or None.
    pub fn persisted_model_label(&self) -> Option<String> {
        let p = self.log_path.as_ref()?;
        let s = std::fs::read_to_string(p.with_extension("model")).ok()?;
        let s = s.trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    }

    /// If a model was persisted for this session (via `/model`), restore it on
    /// resume. Falls back to the existing provider/model when the persisted one
    /// no longer resolves. Returns true if it switched.
    pub fn apply_persisted_model(&mut self) -> bool {
        let Some(label) = self.persisted_model_label() else { return false };
        match crate::config::select(&crate::config::load_providers().unwrap_or_default(), &label) {
            Ok((p, m)) => {
                let _ = self.switch(p.clone(), m.clone());
                true
            }
            Err(_) => false,
        }
    }

    pub fn clear(&mut self) {
        self.history.clear();
    }

    /// Set the cumulative token budget (in+out, in tokens). Off by default;
    /// opt in via `--budget N` or `PIR_TOKEN_BUDGET`. Pass None to disable.
    pub fn set_token_budget(&mut self, budget: Option<u64>) {
        self.token_budget = budget;
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
    /// resumed session keeps its prior conversation. Returns (turns, summary)
    /// where `summary` is a short human-readable line suitable for the REPL to
    /// print (empty when nothing was loaded).
    pub fn load_session(&mut self, session: &PathBuf) -> (usize, String) {
        let mut turns = 0usize;
        let Some(f) = File::open(session).ok() else { return (0, String::new()) };
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

        let summary = if turns > 0 {
            let first = self.history.iter().find_map(|m| {
                if m.role == Role::User {
                    m.text().lines().next().map(|l| l.to_string())
                } else {
                    None
                }
            });
            format!(
                "resumed session ({} turns){}",
                turns,
                first.map(|f| format!(": {f}")).unwrap_or_default()
            )
        } else {
            String::new()
        };
        (turns, summary)
    }

    /// Drive the agent to complete the active goal. Repeatedly prompts the
    /// model with the next pending step (or a completion nudge) until the goal
    /// reaches a terminal state or the model stops making tool calls. This is
    /// the `pir -c` / `/continue` entry point and is resilient to interrupts:
    /// each `update_goal` call persists, so re-running `pir -c` picks up where
    /// the last run stopped.
    /// Drive the active goal to completion. Returns a plaintext summary of
    /// what happened (so either REPL front-end can render it — the streaming
    /// REPL prints it, the TUI pushes it into the conversation pane). Resilient
    /// to interrupts: each `update_goal` call persists, so re-running `pir -c`
    /// picks up where the last run stopped.
    pub fn continue_goal(&mut self) -> String {
        // Snapshot objective + pre-checks so we don't hold a borrow of
        // `goal_store` across the `turn` call (which needs `&mut self`).
        let (objective, already_done) = match &self.goal_store {
            Some(s) => (s.goal.objective.clone(), s.goal.status == GoalStatus::Complete),
            None => {
                return "no start one with /goal <objective>".to_string();
            }
        };
        if already_done {
            let mut out = format!("goal already complete: {}\n", objective);
            if let Some(s) = &self.goal_store {
                out.push_str(&s.goal.summary());
            }
            return out;
        }

        let mut out = String::new();
        out.push_str(&format!("goal: {}\n", objective));
        if let Some(s) = &self.goal_store {
            out.push_str(&s.goal.summary());
            out.push('\n');
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
                out.push_str("goal run errored; stopping\n");
                break;
            }
            let delta = self.usage.output - before;
            if delta == 0 {
                // Model produced no tool calls and nothing progressed.
                out.push_str("model yielded without further progress; stopping\n");
                break;
            }
        }

        let final_status = match &self.goal_store {
            Some(s) => s.goal.status.label().to_string(),
            None => "unknown".to_string(),
        };
        out.push_str(&format!("goal {}: {}\n", final_status, objective));
        if let Some(s) = &self.goal_store {
            out.push_str(&s.goal.summary());
            if let Some(p) = s.path().to_str() {
                out.push_str(&format!("\ngoal saved: {p}"));
            }
        }
        out
    }

    /// Return a plaintext snapshot of the active goal (used by `/goal` and the
    /// REPL front-end, which renders it however it likes).
    pub fn show_goal(&self) -> String {
        match &self.goal_store {
            Some(s) => {
                let mut out = term::bold("goal");
                out.push('\n');
                out.push_str(&s.goal.summary());
                if let Some(p) = s.path().to_str() {
                    out.push_str(&format!("\nsaved: {p}"));
                }
                out
            }
            None => "no active goal; start one with /goal <objective>".to_string(),
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
        self.last_prompt = user.to_string();
        let msg = Message::user(user);
        log_line(&mut self.log, &msg);
        self.history.push(msg);

        let specs = self.registry.specs();
        let tty = crate::term::is_terminal();
        // `spinner` is hoisted out of the per-message loop so the "thinking…"
        // indicator can persist *below* the agent's text (a footer) between
        // model calls, and so the next streamed token can erase it in place
        // (via \r) before printing more text.
        let mut spinner: Option<term::Spinner> = None;

        loop {
            // Cooperative cancellation: bail out at this safe boundary (start
            // of a new model call) if the REPL requested a stop.
            if self.cancel.load(Ordering::SeqCst) {
                self.cancel.store(false, Ordering::SeqCst);
                if !self.quiet {
                    if let Some(s) = spinner.as_mut() {
                        s.stop();
                    }
                    term::out(&term::dim("· turn cancelled"));
                }
                self.notify.publish(self.turn_done_event(), false);
                return Ok(());
            }
            // Optional cumulative token budget (off by default). Stop *before*
            // the next model call once in+out exceeds it, so a runaway turn can't
            // burn unbounded usage. Surfaced as a banner, not an error.
            if let Some(budget) = self.token_budget {
                let used = self.usage.input + self.usage.output;
                if used >= budget {
                    if !self.quiet {
                        if let Some(s) = spinner.as_mut() {
                            s.stop();
                        }
                        spinner = None;
                        term::out(&format!(
                            "\r\x1b[K{}\n",
                            term::yellow(&format!(
                                "✗ token budget reached ({} used / {} limit) — stopping turn",
                                used, budget
                            ))
                        ));
                    }
                    self.notify.publish(self.turn_done_event(), false);
                    if !self.quiet {
                        self.continuations.extend(self.registry.on_turn_end(user));
                    }
                    return Ok(());
                }
            }
            self.trim();

            // While we wait for the model's first token, show a spinner so it's
            // obvious the agent is "thinking". It stops the instant the stream
            // starts emitting text (and is skipped entirely when quiet / not a
            // tty). After the agent's text has printed, the spinner is shown
            // again *below* the text as a footer (see the end of the loop), so
            // it keeps indicating "thinking" while tools run / between calls.
            // `self.typeahead` (filled by the REPL) is rendered on the spinner
            // line so the user sees what they're typing while the model thinks.
            if !self.quiet {
                spinner = Some(term::Spinner::start("thinking", self.typeahead.clone(), tty));
            }
            // Stop the footer spinner (if running) the moment the model emits
            // its first token, so the agent's text starts on a clean line.
            // `stopped_here` tracks whether *this* call has already cleared it,
            // so subsequent tokens in the same stream don't touch it again.
            let mut stopped_here = false;
            let mut on_text = |t: &str| {
                if !self.quiet && !stopped_here {
                    stopped_here = true;
                    if let Some(s) = spinner.as_mut() {
                        s.stop();
                    }
                    spinner = None;
                }
                if !self.quiet {
                    term::out(t);
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
            // Ensure the footer spinner is stopped (covers the no-output case),
            // then move to a fresh line below the agent's text.
            if !self.quiet {
                if let Some(s) = spinner.as_mut() {
                    s.stop();
                }
                spinner = None;
                println!();
            }
            let (assistant, usage) = match result {
                Ok(r) => r,
                Err(e) => {
                    // Surface the failure visibly in the main stream (red banner)
                    // as well as stderr, so a mid-turn provider error isn't lost
                    // below already-printed tokens. The on-screen notification
                    // feed also gets an Error event.
                    if !self.quiet {
                        term::out(&format!("\r\x1b[K{}\n", term::red(&format!("✗ turn error: {e}"))));
                    } else {
                        eprintln!("{} {e}", term::red("error:"));
                    }
                    self.notify.publish(
                        AgentEvent::error(e.clone(), self.project_label(), self.last_prompt.clone()),
                        false,
                    );
                    if !self.quiet {
                        self.continuations.extend(self.registry.on_turn_end(user));
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
                    self.continuations.extend(self.registry.on_turn_end(user));
                }
                return Ok(());
            }

            let mut results = Message { role: Role::User, blocks: Vec::new() };
            for (id, name, input) in &calls {
                if !self.quiet {
                    term::out(&format!("{} {}", term::cyan("»"), describe_call(name, input)));
                }
                // Snapshot the target file before a destructive edit so `/undo`
                // can revert it. `write_file`/`edit_file` take `path`.
                if name == "write_file" || name == "edit_file" {
                    if let Some(p) = input.get("path").and_then(Value::as_str) {
                        self.checkpoint_file(Path::new(p));
                    }
                }
                let outcome = match self.run_goal_tool(name, input) {
                    Some(o) => o,
                    None => self.registry.execute(name, input),
                };
                if !self.quiet {
                    term::out(&term::dim(&format!("  {}", first_line(&outcome.content))));
                }
                results.blocks.push(Block::ToolResult {
                    tool_use_id: id.clone(),
                    content: outcome.content,
                    is_error: outcome.is_error,
                });
            }
            log_line(&mut self.log, &results);
            self.history.push(results);

            // Between model calls (while tools are being executed, and before
            // the next model call), show the spinner *below* the agent's text as
            // a footer so it's clear the agent is still working. The next
            // streamed token erases it in place via `\r`. `self.typeahead` is
            // rendered on the spinner line so typed-ahead input stays visible.
            if !self.quiet {
                spinner = Some(term::Spinner::start("thinking", self.typeahead.clone(), tty));
            }

            // Cooperative cancellation: stop after this batch of tools
            // completes (the in-progress step always finishes first).
            if self.cancel.load(Ordering::SeqCst) {
                self.cancel.store(false, Ordering::SeqCst);
                if !self.quiet {
                    if let Some(s) = spinner.as_mut() {
                        s.stop();
                    }
                    term::out(&term::dim("· turn cancelled"));
                }
                self.notify.publish(self.turn_done_event(), false);
                return Ok(());
            }
        }
    }

    /// Drain follow-up prompts queued by extension backends during the last
    /// `on_turn_end`. The REPL calls this after a turn finishes and pushes any
    /// returned prompts into its prompt queue (e.g. the worktree extension
    /// asking the model to fix failing tests). Each call returns the backlog
    /// once and resets it.
    pub fn take_continuations(&mut self) -> Vec<String> {
        std::mem::take(&mut self.continuations)
    }

    /// Snapshot `path` before a destructive file edit so it can be reverted
    /// with `/undo`. Copies the current contents (if any) to a sidecar under
    /// `.pir/undo/` keyed by a content hash + timestamp; pushes (target, backup)
    /// onto the undo stack. Best-effort: any failure is silently ignored so a
    /// read-only or missing file never breaks the edit.
    pub fn checkpoint_file(&mut self, path: &Path) {
        let Ok(src) = std::fs::read(path) else { return };
        let dir = self.undo_dir();
        let _ = std::fs::create_dir_all(&dir);
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        src.hash(&mut h);
        let name = format!(
            "{:016x}-{}.bak",
            h.finish(),
            term::timestamp_compact()
        );
        let backup = dir.join(name);
        if std::fs::write(&backup, &src).is_ok() {
            self.undo_stack.push((path.to_path_buf(), backup));
        }
    }

    fn undo_dir(&self) -> PathBuf {
        // Store undo sidecars next to the session logs so they're scoped to the
        // project and cleaned up with it. Prefer the project-local `.pir/undo`
        // when writable, else the global sessions dir.
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let local = cwd.join(".pir").join("undo");
        if std::fs::create_dir_all(&local).is_ok() {
            return local;
        }
        config::pi_dir().join("agent").join("sessions").join("undo")
    }

    /// Restore the most recent file checkpoint (`/undo`). Returns a status line.
    /// If `all` is true, restores every checkpoint on the stack (oldest→newest
    /// would re-introduce edits, so we restore newest-first, i.e. replay in
    /// reverse — but for simplicity `/undo` restores one; `/undo all` restores
    /// each target to its latest snapshot).
    pub fn undo(&mut self, all: bool) -> String {
        if self.undo_stack.is_empty() {
            return "nothing to undo".to_string();
        }
        if all {
            // Re-apply each target from its newest snapshot, deduplicating by
            // target so each file ends at its most-recent pre-edit state.
            let mut by_target: std::collections::HashMap<PathBuf, PathBuf> = std::collections::HashMap::new();
            for (target, backup) in self.undo_stack.iter().rev() {
                by_target.insert(target.clone(), backup.clone());
            }
            let mut n = 0;
            for (target, backup) in by_target {
                if std::fs::copy(&backup, &target).is_ok() {
                    n += 1;
                }
            }
            self.undo_stack.clear();
            return format!("restored {n} file(s) to their pre-edit state");
        }
        let (target, backup) = self.undo_stack.pop().expect("non-empty");
        match std::fs::copy(&backup, &target) {
            Ok(_) => format!("restored {}", target.display()),
            Err(e) => format!("undo failed for {}: {e}", target.display()),
        }
    }

    pub fn undo_available(&self) -> usize {
        self.undo_stack.len()
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
        AgentEvent::turn_done(
            std::time::Duration::ZERO,
            self.usage.input,
            self.usage.output,
            self.project_label(),
            self.last_prompt.clone(),
        )
    }

    /// Build the `Idle` event (returned to the REPL prompt). Carries the same
    /// project / last-prompt context so on-screen feed lines identify it.
    pub fn idle_event(&self) -> AgentEvent {
        AgentEvent::idle(self.project_label(), self.last_prompt.clone())
    }

    /// Build an `Error` event from a turn's error message.
    pub fn error_event(&self, message: String) -> AgentEvent {
        AgentEvent::error(message, self.project_label(), self.last_prompt.clone())
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
        term::out(&term::dim("[pir: context trimmed]"));
    }
}

fn make_client(provider: &Provider, cancel: Arc<AtomicBool>) -> Result<Client, String> {
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
    let mut client = Client::new(kind, &base, key);
    // Share the agent's cancellation flag so a Ctrl-C/Ctrl-D during an in-flight
    // model call aborts the streaming read promptly instead of blocking until
    // the whole response arrives.
    client.set_cancel(cancel);
    Ok(client)
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
