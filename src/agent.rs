use crate::config::{self, ApiKind, Model, Provider};
use crate::goal::{GoalStatus, GoalStore};
use crate::notify::{AgentEvent, SharedBus};
use crate::plugin::{EventKind, Outcome, Registry};
use crate::provider::Client;
use crate::term;
use crate::types::{Block, Message, Role, Usage};
use crate::session::SessionStatus;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
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
    /// notifications can show *what* finished, not just "turn done". `pub(crate)`
    /// so the REPL can read it for the `&`-detach-to-background prompt label.
    pub(crate) last_prompt: String,
    /// When true, the agent runs silently (no token streaming or per-tool
    /// prints to the terminal). Used for backgrounded sessions, which still
    /// persist everything to the session log and emit notifications.
    quiet: bool,
    /// Shared request to silence streaming *mid-turn* (used to "background" a
    /// running foreground turn: the REPL flips this so the worker stops writing
    /// to stdout and the terminal can return to the idle prompt). Hoisted out
    /// of the agent so the REPL can toggle it without owning the agent.
    quiet_req: Arc<AtomicBool>,
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
    /// Local, per-session authority flag (the "su based security" toggle).
    /// When true (default), the agent stays confined to its sandbox identity
    /// and must not escalate to the invoking user's authority. When false, the
    /// agent is authorized to act with the *invoking user's full authority* for
    /// this session. This is a self-imposed, in-session authorization only — it
    /// never changes any system-wide configuration (no sudoers/wrappers are
    /// touched). Persisted next to the session log so a resumed session keeps
    /// its choice.
    su_security_enabled: bool,
    /// Reasoning / "extended thinking" level for this session (see
    /// `config::ThinkingLevel`). Threaded through to the provider request
    /// (Anthropic thinking budget / OpenAI reasoning effort). `Off` (the
    /// default) sends no thinking control at all — matching the prior behaviour.
    /// Persisted next to the session log so a resumed session keeps the level.
    thinking: config::ThinkingLevel,
    /// Whether the model's reasoning/thinking content is shown on the terminal
    /// as it streams. When false, thinking blocks are still collected + logged
    /// but suppressed from the live output (toggle with `/thinking show`/
    /// `/thinking hide`; persisted per session).
    show_thinking: bool,
    /// Cached provider list (loaded once, reused for model switches / resume).
    /// Avoids re-reading and re-parsing `~/.pi/agent/models-store.json` on every
    /// `/model` switch, resume, and `apply_persisted_model` call.
    cached_providers: Vec<Provider>,
}

/// What `load_session` restored. The REPL (and `/fg`/`/resume`) renders
/// [`SessionResume::banner`] so `-r` makes it clear which session came back,
/// shows its first/last prompts and the tail of its final output, and seeds the
/// line editor's arrow-up history with [`prompts`] so the user can scroll back
/// through the session's prior prompts.
pub struct SessionResume {
    pub turns: usize,
    /// One-line summary (kept for callers that want a compact line).
    pub summary: String,
    /// The session's first user prompt (full text).
    pub first_prompt: String,
    /// The session's last user prompt (full text).
    pub last_prompt: String,
    /// The tail of the session's last assistant message (full text).
    pub last_output: String,
    /// Every non-empty user prompt, in order — used to seed arrow-up history.
    pub prompts: Vec<String>,
}

impl SessionResume {
    /// Render a banner describing what was resumed: which session file, its
    /// first/last prompts, and the tail of its last assistant output.
    pub fn banner(&self, session: &Path) -> String {
        let name = session
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| session.display().to_string());
        let w = term::terminal_width().min(100);
        let rule = "─".repeat(w);
        let mut out = String::new();
        out.push_str(&term::bold(&format!("resumed session: {name}  ({} turns)", self.turns)));
        out.push('\n');
        if !self.first_prompt.is_empty() {
            out.push_str(&format!(
                "{} first prompt: {}\n",
                term::dim("·"),
                term::dim(&self.first_prompt.lines().next().unwrap_or("").trim())
            ));
        }
        if !self.last_prompt.is_empty() {
            out.push_str(&format!(
                "{} last  prompt: {}\n",
                term::dim("·"),
                term::dim(&self.last_prompt.lines().next().unwrap_or("").trim())
            ));
        }
        if !self.last_output.is_empty() {
            out.push_str(&term::dim(&rule));
            out.push('\n');
            out.push_str(&term::dim("last output (tail):\n"));
            out.push_str(&tail_lines(&self.last_output, 40));
            out.push('\n');
            out.push_str(&term::dim(&rule));
        }
        out
    }
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

        // Load the provider list once and cache it on the agent, so model
        // switches and resume lookups don't re-read the (possibly large)
        // models-store.json every time. An empty/unreadable store falls back to
        // an empty cache — every later `select`/switch simply fails to resolve
        // and the caller surfaces the error.
        let cached_providers = config::load_providers().unwrap_or_default();
        let quiet_req = Arc::new(AtomicBool::new(false));
        // The "go silent" switch must exist before the registry (and its
        // backends) are built, because they capture a clone of it. The REPL
        // holds the same `Arc`, so flipping it (to background a running turn)
        // silences any in-flight terminal output the backends emit — e.g. the
        // bash tool's live elapsed clock — without the REPL owning the worker.
        let mut registry = Registry::new(cwd.clone(), full_auto, cancel.clone());
        // Share the REPL's "go silent" switch with the tool backends so a
        // backgrounded turn silences their in-flight terminal output too.
        registry.set_quiet_handle(quiet_req.clone());
        crate::register_all(&mut registry);
        registry.session_started(&cwd);
        // Emit SessionStart so backends (e.g. the pi-extensions bridge) can
        // spawn their child processes / load resources now that the agent and
        // its cwd are known.
        registry.emit(EventKind::SessionStart, &json!({ "cwd": cwd.display().to_string() }));

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
            quiet_req,
            cancel,
            typeahead,
            last_prompt: String::new(),
            continuations: Vec::new(),
            token_budget: None,
            undo_stack: Vec::new(),
            su_security_enabled: true,
            thinking: config::ThinkingLevel::Off,
            show_thinking: true,
            cached_providers,
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

    /// Change whether this agent prints to the terminal. The TUI REPL builds a
    /// quiet agent (ratatui owns the screen) and tails the session log instead
    /// of letting the turn stream to stdout.
    pub fn set_quiet(&mut self, q: bool) {
        self.quiet = q;
    }

    /// Request silent streaming for a turn that is *already running* on a
    /// worker thread. The REPL uses this to "background" the foreground turn:
    /// once set, the worker stops printing to stdout (the terminal    /// the idle prompt) keeps running the
    /// background. Read-only here — ownership stays with the REPL.
    pub fn request_quiet(&self) {
        self.quiet_req.store(true, Ordering::SeqCst);
    }

    /// Share the REPL's foreground "go quiet" handle with this agent, replacing
    /// the agent's private handle. The REPL holds the same `Arc`, so flipping
    /// it detaches (silences) the running turn without owning the agent.
    pub fn set_quiet_handle(&mut self, handle: Arc<AtomicBool>) {
        self.quiet_req = handle;
    }

    /// True when the turn should not write to the terminal, either because the
    /// agent was built quiet (background job) or the REPL asked an in-flight
    /// foreground turn to go quiet (detach to background).
    fn silent(&self) -> bool {
        self.quiet || self.quiet_req.load(Ordering::SeqCst)
    }

    /// Read-only access to the chosen provider/model (for spawning background
    /// sessions that continue on the same configuration).
    pub fn provider(&self) -> Provider {
        self.provider.clone()
    }
    pub fn model(&self) -> Model {
        self.model.clone()
    }

    /// Collect startup banners from every extension backend (e.g. the worktree
    /// extension reporting the agent's current worktree). Printed by the REPL
    /// before the first prompt.
    pub fn startup_reports(&mut self) -> Vec<String> {
        self.registry.startup_reports()
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
    /// no longer resolves. Returns true if it switched. Uses the cached
    /// provider list (loaded once in `new`) rather than re-reading the store.
    pub fn apply_persisted_model(&mut self) -> bool {
        let Some(label) = self.persisted_model_label() else { return false };
        match crate::config::select(&self.cached_providers, &label) {
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

    /// Whether this session runs with the su-based security boundary on
    /// (agent confined to its sandbox identity, default) or off (agent is
    /// authorized to act with the invoking user's full authority for this
    /// session only). This is purely a local, in-session authorization flag —
    /// it never edits system files. Persisted next to the session log so a
    /// resumed session keeps its choice.
    pub fn su_security_enabled(&self) -> bool {
        self.su_security_enabled
    }

    /// Set the local su-security authorization for this session. Returns the
    /// reason it was recorded at (for audit). `reason` is required when turning
    /// the boundary OFF, because disabling it lets the agent act with the
    /// invoking user's full authority for this session.
    pub fn set_su_security(&mut self, enabled: bool, reason: &str) -> String {
        self.su_security_enabled = enabled;
        let note = if reason.trim().is_empty() {
            "(no reason given)".to_string()
        } else {
            reason.trim().to_string()
        };
        self.persist_su_security();
        if enabled {
            "su-based security ENABLED for this session (agent confined to its sandbox identity)".to_string()
        } else {
            format!(
                "su-based security DISABLED for this session — agent authorized to act with the \
                 invoking user's full authority (reason: {note}). This affects only this session; \
                 no system-wide configuration was changed."
            )
        }
    }

    /// Persist the local su-security choice next to the session log
    /// (`<log>.susec`) so a resumed session keeps it. Best-effort; a missing
    /// log (one-shot) is silently skipped.
    fn persist_su_security(&self) {
        if let Some(p) = &self.log_path {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let path = p.with_extension("susec");
            let _ = std::fs::write(&path, if self.su_security_enabled { "1" } else { "0" });
        }
    }

    /// Load a previously persisted su-security choice (from `<log>.susec`) for
    /// a resumed session. Returns true if a value was restored.
    pub fn apply_persisted_su_security(&mut self) -> bool {
        let Some(p) = self.log_path.as_ref() else { return false };
        match std::fs::read_to_string(p.with_extension("susec")) {
            Ok(s) => {
                self.su_security_enabled = s.trim() == "1";
                true
            }
            Err(_) => false,
        }
    }

    /// Current reasoning / "extended thinking" level for this session.
    pub fn thinking_level(&self) -> config::ThinkingLevel {
        self.thinking
    }

    /// Whether the model's reasoning/thinking content is shown on the terminal.
    pub fn show_thinking(&self) -> bool {
        self.show_thinking
    }

    /// Set the reasoning level for this session (persisted so a resumed session
    /// keeps it). Returns a short human-readable status line.
    pub fn set_thinking(&mut self, level: config::ThinkingLevel) -> String {
        self.thinking = level;
        self.persist_thinking();
        if level.enabled() {
            let budget = self
                .thinking
                .anthropic_budget(self.model.context.unwrap_or(200_000));
            match self.provider.kind() {
                Some(ApiKind::Anthropic) => match budget {
                    Some(b) => format!(
                        "thinking: {}  (Anthropic budget ≈ {} tokens — may exceed the model's max unless it supports extended thinking)",
                        level.as_str(), b
                    ),
                    None => format!(
                        "thinking: {}  (model context too small for a meaningful thinking budget; will be ignored)",
                        level.as_str()
                    ),
                },
                Some(ApiKind::OpenAi) => match level.oai_effort() {
                    Some(e) => format!("thinking: {}  (OpenAI reasoning_effort = {e})", level.as_str()),
                    None => format!(
                        "thinking: {}  (no OpenAI reasoning_effort for this level; will be ignored)",
                        level.as_str()
                    ),
                },
                None => format!("thinking: {}", level.as_str()),
            }
        } else {
            "thinking: off".to_string()
        }
    }

    /// Toggle whether the model's reasoning/thinking is shown on the terminal.
    /// `on` enables display; `off` suppresses it (the thinking blocks are still
    /// collected + logged). Persisted per session. Returns a status line.
    pub fn set_show_thinking(&mut self, on: bool) -> String {
        self.show_thinking = on;
        self.persist_thinking();
        if on {
            "thinking display: on  (model reasoning will be shown as it streams)"
        } else {
            "thinking display: off  (model reasoning will be collected but hidden — use `/thinking show` to reveal it)"
        }
        .to_string()
    }

    /// Persist the thinking level + show-thinking flag next to the session log
    /// (`<log>.thinking`) so a resumed session keeps them. Best-effort; a
    /// missing log (one-shot) is silently skipped.
    fn persist_thinking(&self) {
        if let Some(p) = &self.log_path {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let body = format!("{}\n{}", self.thinking.as_str(), if self.show_thinking { "1" } else { "0" });
            let _ = std::fs::write(p.with_extension("thinking"), body);
        }
    }

    /// Load a previously persisted thinking choice (from `<log>.thinking`) for a
    /// resumed session. Returns true if a value was restored.
    pub fn apply_persisted_thinking(&mut self) -> bool {
        let Some(p) = self.log_path.as_ref() else { return false };
        let Ok(s) = std::fs::read_to_string(p.with_extension("thinking")) else { return false };
        let mut lines = s.lines();
        if let Some(lvl) = lines.next().and_then(config::ThinkingLevel::parse) {
            self.thinking = lvl;
        } else {
            return false;
        }
        if let Some(flag) = lines.next() {
            self.show_thinking = flag.trim() == "1";
        }
        true
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
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");

        // No active goal yet. Only `set_objective` can bootstrap one — so a
        // fresh session can start a goal purely through the tool (as the tool
        // description promises) instead of needing the `/goal` slash command.
        // Every other action requires a goal to already exist.
        let store = match self.goal_store.as_mut() {
            Some(s) => s,
            None => {
                if action == "set_objective" {
                    let obj = input
                        .get("objective")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    self.start_goal(&obj);
                    match self.goal_store.as_mut() {
                        Some(s) => s,
                        None => {
                            return Some(Outcome {
                                content: "Could not start a goal (no session log and no writable cwd). Use /goal <objective> instead.".into(),
                                is_error: true,
                            })
                        }
                    }
                } else {
                    return Some(Outcome {
                        content: "No active goal. Call update_goal with action set_objective first.".into(),
                        is_error: true,
                    });
                }
            }
        };

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
    /// resumed session keeps its prior conversation. Returns a [`SessionResume`]
    /// describing what was loaded (and the prior prompts, for arrow-up history).
    /// When nothing was loaded, `turns == 0` and the rest is empty.
    pub fn load_session(&mut self, session: &PathBuf) -> SessionResume {
        let mut turns = 0usize;
        let mut prompts: Vec<String> = Vec::new();
        let mut first_prompt = String::new();
        let mut last_user_prompt = String::new();
        let mut last_assistant = String::new();
        let Some(f) = File::open(session).ok() else {
            return SessionResume {
                turns: 0,
                summary: String::new(),
                first_prompt: String::new(),
                last_prompt: String::new(),
                last_output: String::new(),
                prompts: Vec::new(),
            };
        };
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
                pending = Some(Message { role: Role::User, blocks: blocks.clone() });
                turns += 1;
                // Capture this prompt's full text for the banner + arrow-up history.
                let text = blocks
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text(t) => Some(t.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
                    .trim()
                    .to_string();
                if !text.is_empty() {
                    if first_prompt.is_empty() {
                        first_prompt = text.clone();
                    }
                    last_user_prompt = text.clone();
                    prompts.push(text);
                }
            } else {
                // Assistant message: if we already have a pending user turn,
                // pair them; otherwise just queue the assistant alone. Remember
                // its text as the latest assistant output (shown as the tail).
                if let Some(mut u) = pending.take() {
                    u.blocks.extend(blocks.clone());
                    self.history.push(u);
                } else {
                    self.history.push(Message { role: Role::Assistant, blocks: blocks.clone() });
                }
                let text = blocks
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text(t) => Some(t.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
                    .trim()
                    .to_string();
                if !text.is_empty() {
                    last_assistant = text;
                }
            }
        }
        if let Some(m) = pending.take() {
            self.history.push(m);
        }

        let summary = if turns > 0 {
            format!(
                "resumed session ({} turns){}",
                turns,
                if first_prompt.is_empty() {
                    String::new()
                } else {
                    format!(": {}", first_prompt.lines().next().unwrap_or("").trim())
                }
            )
        } else {
            String::new()
        };
        SessionResume {
            turns,
            summary,
            first_prompt,
            last_prompt: last_user_prompt,
            last_output: last_assistant,
            prompts,
        }
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
        self.drive_goal(None)
    }

    /// Drive the active goal to completion. If `max_steps` is `Some(n)`, stop
    /// after at most `n` model turns even if the goal isn't terminal yet — this
    /// caps runaway loops without *limiting* the model: it still spends as many
    /// tokens per step as it needs, we only log how many steps/tokens were used
    /// and then hand control back. `None` (the default, used by `/continue` and
    /// `pir -c`) means no cap. Returns a plaintext summary.
    pub fn drive_goal(&mut self, max_steps: Option<usize>) -> String {
        // Snapshot objective + pre-checks so we don't hold a borrow of
        // `goal_store` across the `turn` call (which needs `&mut self`).
        let (objective, already_done) = match &self.goal_store {
            Some(s) => (s.goal.objective.clone(), s.goal.status == GoalStatus::Complete),
            None => {
                return "no goal started — start one with /goal <objective>".to_string();
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

        let mut steps = 0usize;
        let start_in = self.usage.input;
        let start_out = self.usage.output;
        loop {
            // Optional step cap (off by default). Logs the work done so far and
            // yields instead of looping forever; it does NOT truncate the model
            // or hide progress.
            if let Some(limit) = max_steps {
                if steps >= limit {
                    out.push_str(&format!(
                        "step cap ({limit}) reached — {steps} step(s) run, {} in / {} out tokens used this run\n",
                        self.usage.input.saturating_sub(start_in),
                        self.usage.output.saturating_sub(start_out),
                    ));
                    break;
                }
            }
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
            let res = self.turn(&prompt);
            steps += 1;
            if let Err(e) = res {
                out.push_str(&format!("goal run errored ({e}); stopping\n"));
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
        out.push_str(&format!(
            "goal {}: {}  ({} step(s), {} in / {} out tokens this run)\n",
            final_status,
            objective,
            steps,
            self.usage.input.saturating_sub(start_in),
            self.usage.output.saturating_sub(start_out),
        ));
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
        // Record that a turn is now in flight (so a crash/network failure mid-turn
        // leaves a discoverable "unfinished" session owned by this live process).
        self.mark_status(SessionStatus::Active, self.goal_pending(), "");
        let specs = self.registry.specs();
        let tty = crate::term::is_terminal();
        // `spinner` is hoisted out of the per-message loop so the "thinking…"
        // indicator can persist *below* the agent's text (a footer) between
        // model calls, and so the next streamed token can erase it in place
        // (via \r) before printing more text. It lives in a `RefCell` (and the
        // "already stopped this call" flag in a `Cell`) so both the text and
        // thinking stream callbacks can stop it without tripping the borrow
        // checker — see `stop_spinner` below.
        let spinner: RefCell<Option<term::Spinner>> = RefCell::new(None);
        let stopped_here = Cell::new(false);

        // Stop the "thinking…" spinner (and its REPL prompt block) exactly once
        // per model call, the moment the first token — text *or* reasoning —
        // arrives. Shared by both stream callbacks so the spinner's 80ms
        // redraws can never clobber streaming output.
        let stop_spinner = || {
            if !stopped_here.get() {
                stopped_here.set(true);
                if let Some(mut s) = spinner.borrow_mut().take() {
                    s.stop();
                }
            }
        };

        loop {
            // Cooperative cancellation: bail out at this safe boundary (start
            // of a new model call) if the REPL requested a stop.
            if self.cancel.load(Ordering::SeqCst) {
                self.cancel.store(false, Ordering::SeqCst);
                self.mark_status(SessionStatus::Interrupted, self.goal_pending(), "cancelled");
                if !self.silent() {
                    if let Some(mut s) = spinner.borrow_mut().take() {
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
                    if !self.silent() {
                        if let Some(mut s) = spinner.borrow_mut().take() {
                            s.stop();
                        }
                        term::out(&format!(
                            "\r\x1b[K{}\n",
                            term::yellow(&format!(
                                "✗ token budget reached ({} used / {} limit) — stopping turn",
                                used, budget
                            ))
                        ));
                    }
                    self.mark_status(SessionStatus::Interrupted, self.goal_pending(), "token budget reached");
                    self.notify.publish(self.turn_done_event(), false);
                    if !self.silent() {
                        self.continuations.extend(self.registry.on_turn_end(user));
                    }
                    return Ok(());
                }
            }
            self.trim();

            // Emit TurnStart so backends know an assistant turn is beginning
            // (this is the point the model stream starts).
            self.registry.emit(EventKind::TurnStart, &json!({ "prompt": user }));

            // Reset the per-call "already stopped" latch so a *new* spinner on
            // this model call (the footer re-shown after tools ran) can be
            // stopped by the first streamed token. The latch is shared between
            // the text and reasoning callbacks via `stop_spinner`.
            stopped_here.set(false);

            // While we wait for the model's first token, show a spinner so it's
            // obvious the agent is "thinking". It stops the instant the stream
            // starts emitting text (and is skipped entirely when quiet / not a
            // tty). After the agent's text has printed, the spinner is shown
            // again *below* the text as a footer (see the end of the loop), so
            // it keeps indicating "thinking" while tools run / between calls.
            // `self.typeahead` (filled by the REPL) is rendered on the spinner
            // line so the user sees what they're typing while the model thinks.
            if !self.silent() {
                *spinner.borrow_mut() = Some(term::Spinner::start("thinking", self.typeahead.clone(), tty));
            }
            // Stop the footer spinner (if running) the moment the model emits
            // its first token, so the agent's text starts on a clean line.
            // `stopped_here` tracks whether *this* call has already cleared it,
            // so subsequent tokens in the same stream don't touch it again.
            let mut on_text = |t: &str| {
                if !self.silent() {
                    stop_spinner();
                    term::out(t);
                }
            };
            // Reasoning/thinking content. When show-thinking is off the thinking
            // blocks are still collected + parsed (and logged), they're just not
            // printed to the live terminal. The spinner is stopped the moment
            // reasoning begins (via `stop_spinner`), so its 80ms redraws don't
            // clobber the dimmed thinking text as it streams — the REPL prompt
            // is then drawn *after* the thinking completes (back at the idle
            // prompt) instead of sitting on top of the reasoning and hiding
            // most of it.
            //
            // Deferral: the spinner line doubles as the user's live typing
            // echo (the REPL records keystrokes into `typeahead` and the
            // spinner thread renders them). Printing reasoning *while the
            // user is typing* would wipe that in-progress line, so thinking
            // is held in a buffer until the keyboard has been idle for at
            // least `KEYBOARD_IDLE_BEFORE_THINKING_MS` (1s). It is force-
            // flushed on stop_spinner/boundaries so nothing is lost or
            // reordered relative to the reply.
            let show_thinking = self.show_thinking;
            let mut think_buf = String::new();
            let mut on_think = |t: &str| {
                if !self.silent() && show_thinking {
                    stop_spinner();
                    think_buf.push_str(t);
                    if term::raw::keyboard_idle_long_enough() {
                        term::out(&format!("{}", term::dim(&std::mem::take(&mut think_buf))));
                    }
                }
            };
            let result = self.client.chat(
                &self.model.id,
                self.model.max_tokens.unwrap_or(8192),
                &self.system,
                &self.history,
                &specs,
                &mut on_text,
                self.thinking,
                self.model.context.unwrap_or(200_000),
                &mut on_think,
            );
            // Flush any thinking that arrived while the user was still typing
            // (deferred above) BEFORE the reply text / tool output prints, so
            // reasoning never appears interleaved after the response it
            // preceded — and the buffer can't leak into the next model call.
            if !think_buf.is_empty() {
                term::out(&format!("{}", term::dim(&std::mem::take(&mut think_buf))));
            }
            // Ensure the footer spinner is stopped (covers the no-output case),
            // then move to a fresh line below the agent's text.
            if !self.silent() {
                if let Some(mut s) = spinner.borrow_mut().take() {
                    s.stop();
                }
                println!();
            }
            let (assistant, usage) = match result {
                Ok(r) => r,
                Err(e) => {
                    // Surface the failure visibly in the main stream (red banner)
                    // as well as stderr, so a mid-turn provider error isn't lost
                    // below already-printed tokens. The on-screen notification
                    // feed also gets an Error event.
                    if !think_buf.is_empty() {
                        term::out(&format!("{}", term::dim(&std::mem::take(&mut think_buf))));
                    }
                    if !self.silent() {
                        term::out(&format!("\r\x1b[K{}\n", term::red(&format!("✗ turn error: {e}"))));
                    } else {
                        eprintln!("{} {e}", term::red("error:"));
                    }
                    self.notify.publish(
                        AgentEvent::error(e.clone(), self.project_label(), self.last_prompt.clone()),
                        false,
                    );
                    if !self.silent() {
                        self.continuations.extend(self.registry.on_turn_end(user));
                    }
                    self.mark_status(SessionStatus::Interrupted, self.goal_pending(), &format!("turn error: {e}"));
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
                self.registry.emit(EventKind::AgentEnd, &json!({}));
                self.notify.publish(self.turn_done_event(), false);
                if !self.silent() {
                    self.registry.emit(EventKind::TurnEnd, &json!({ "prompt": user }));
                    self.continuations.extend(self.registry.on_turn_end(user));
                }
                self.mark_status(SessionStatus::Completed, self.goal_pending(), "");
                return Ok(());
            }

            let mut results = Message { role: Role::User, blocks: Vec::new() };
            for (id, name, input) in &calls {
                if !self.silent() {
                    term::out(&format!("{} {}", term::cyan("»"), describe_call(name, input)));
                }
                // Pre-flight extension hook: any backend may block this tool
                // call (permission gates, protected paths, etc.). When blocked,
                // we feed the reason back as the tool result so the model sees
                // *why* and can adapt, and stop asking for more tools this turn
                // if the hook requested `terminate`.
                if let Some((reason, terminate)) = self.registry.preflight_tool(name, input) {
                    if !self.silent() {
                        term::out(&term::yellow(&format!("  · blocked: {reason}")));
                    }
                    results.blocks.push(Block::ToolResult {
                        tool_use_id: id.clone(),
                        content: format!("blocked by extension: {reason}"),
                        is_error: true,
                    });
                    if terminate {
                        break;
                    }
                    continue;
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
                if !self.silent() {
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
            if !self.silent() {
                *spinner.borrow_mut() = Some(term::Spinner::start("thinking", self.typeahead.clone(), tty));
            }

            // Cooperative cancellation: stop after this batch of tools
            // completes (the in-progress step always finishes first).
            if self.cancel.load(Ordering::SeqCst) {
                self.cancel.store(false, Ordering::SeqCst);
                self.mark_status(SessionStatus::Interrupted, self.goal_pending(), "cancelled");
                if !self.silent() {
                    if let Some(mut s) = spinner.borrow_mut().take() {
                        s.stop();
                    }
                    term::out(&term::dim("· turn cancelled"));
                }
                self.notify.publish(self.turn_done_event(), false);
                return Ok(());
            }
        }
    }

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

    /// Dispatch a slash command to an extension backend (e.g. the
    /// `pi-extensions` bridge), by bare `name` (no leading `/`). Returns `None`
    /// when no extension registered this command (so the REPL can report it as
    /// unknown). Backends are reached through the shared `Registry`.
    pub fn run_registered_command(&mut self, name: &str, args: &str) -> Option<crate::plugin::Outcome> {
        self.registry.run_command(name, args)
    }

    /// List every tool spec the registry currently exposes (built-in +
    /// extension). Used by the `/ext` REPL diagnostic.
    pub fn registry_spec_names(&self) -> Vec<String> {
        self.registry.specs().iter().map(|s| s.name.to_string()).collect()
    }

    /// List every extension-registered slash command. Used by `/ext`.
    pub fn registry_command_names(&self) -> Vec<(String, String)> {
        self.registry
            .commands()
            .into_iter()
            .map(|c| (c.name, c.description))
            .collect()
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

    /// Persist this session's liveness/end-status sidecar so unfinished
    /// conversations can be discovered and resumed later. Called by `turn`.
    fn mark_status(&self, status: SessionStatus, goal_pending: bool, reason: &str) {
        if let Some(p) = &self.log_path {
            crate::session::write_status(
                p,
                status,
                std::process::id(),
                &self.last_prompt,
                goal_pending,
                reason,
            );
        }
    }

    /// True if a goal is attached and not yet complete (so this session still
    /// has unfinished work even when the last turn ended cleanly).
    fn goal_pending(&self) -> bool {
        self.goal_store
            .as_ref()
            .map(|s| !s.goal.status.is_terminal())
            .unwrap_or(false)
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
        // The `{env:VAR}` reference (if any) was already resolved by
        // `expand_env`; an `Err` here means the variable is unset/empty, which
        // we name explicitly so the user isn't left with a generic failure.
        if let Some(k) = provider.api_key.as_deref() {
            if let Some(var) = k.strip_prefix("{env:").and_then(|r| r.strip_suffix('}')) {
                return format!(
                    "no API key for '{}' — the env var {var} is unset or empty (referenced in {}, or set apiKey directly)",
                    provider.pid(),
                    config::pi_dir().join("models-store.json").display()
                );
            }
        }
        format!(
            "no API key for '{}' — export the env var referenced in {}, or set apiKey directly",
            provider.pid(),
            config::pi_dir().join("models-store.json").display()
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
                    Block::Thinking { text } => text.len(),
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
        "update_goal" => {
            let action = input.get("action").and_then(Value::as_str).unwrap_or("?");
            let detail = match action {
                "set_objective" => s("objective").to_string(),
                "add_steps" => input
                    .get("steps")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default(),
                "set_status" => s("status").to_string(),
                "set_step" => format!("#{} -> {}", input["step_id"], s("step_status")),
                "note" => s("note").to_string(),
                other => other.to_string(),
            };
            format!("goal  {action} {detail}")
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

/// Return the last `n` lines of `s`, indented so the block reads as a terminal
/// "tail". Used by the resume banner to show the final page of a session's
/// output without dumping the whole transcript.
fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..]
        .iter()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn log_line(log: &mut Option<fs::File>, m: &Message) {
    let Some(f) = log.as_mut() else { return };
    let role = if m.role == Role::User { "user" } else { "assistant" };
    let entry = json!({
        "ts": term::epoch(),
        "role": role,
        "blocks": m.blocks.iter().map(|b| match b {
            Block::Text(t) => json!({ "type": "text", "text": t }),
            Block::Thinking { text } => json!({ "type": "thinking", "text": text }),
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

#[cfg(test)]
mod goal_bootstrap_tests {
    use super::*;
    use crate::config::Provider;
    use crate::notify::shared_bus;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;

    fn fresh_agent() -> Agent {
        let p: Provider =
            serde_json::from_str(r#"{"id":"test","baseUrl":"https://example.invalid/v1","apiKey":"x","api":"openai","models":[{"id":"m"}]}"#).unwrap();
        let m = p.models[0].clone();
        Agent::new(
            p,
            m,
            true,
            false,
            shared_bus(),
            None,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(String::new())),
        )
        .expect("agent")
    }

    #[test]
    fn set_objective_bootstraps_goal_in_fresh_session() {
        let mut a = fresh_agent();
        assert!(a.goal_snapshot().is_none(), "should start with no goal");
        let out = a.run_goal_tool(
            "update_goal",
            &serde_json::json!({"action":"set_objective","objective":"ship the thing"}),
        );
        let o = out.expect("outcome");
        assert!(!o.is_error, "set_objective should not error: {}", o.content);
        assert!(o.content.contains("objective set: ship the thing"));
        assert!(a.goal_snapshot().is_some(), "goal should now exist");
    }

    #[test]
    fn non_set_objective_errors_without_goal() {
        let mut a = fresh_agent();
        let out = a.run_goal_tool(
            "update_goal",
            &serde_json::json!({"action":"add_steps","steps":["x"]}),
        );
        let o = out.expect("outcome");
        assert!(o.is_error, "add_steps without a goal must error");
        assert!(o.content.contains("No active goal"));
    }

    #[test]
    fn describe_call_shows_update_goal_args() {
        let d = describe_call(
            "update_goal",
            &serde_json::json!({"action":"set_step","step_id":3,"step_status":"done"}),
        );
        assert!(d.contains("goal"), "got {d}");
        assert!(d.contains("set_step"), "got {d}");
        assert!(d.contains("#3"), "got {d}");
    }
}
