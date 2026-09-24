use crate::config::{self, ApiKind, Model, Provider};
use crate::goal::{GoalStatus, GoalStore};
use crate::notify::{AgentEvent, SharedBus};
use crate::plugin::{EventKind, Outcome, Registry};
use crate::provider::Client;
use crate::security::SecurityContext;
use crate::term;
use crate::types::{Block, Message, Role, Usage};
use crate::session::SessionStatus;
use serde_json::{json, Value};
use streamdown_ansi::sanitize::sanitize_for_terminal;
use std::cell::{Cell, RefCell};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Explain a persisted per-session su-security choice that contradicts the
/// project's current policy. Returns `None` when they agree (nothing to say).
///
/// Split out as a pure function so the precedence rule is unit-testable without
/// building a whole `Agent`: the persisted sidecar wins, but the operator is
/// always told, and told how to change it.
pub(crate) fn su_security_override_notice(
    persisted: bool,
    policy_on: bool,
    sidecar: &str,
) -> Option<String> {
    if persisted == policy_on {
        return None;
    }
    Some(if persisted {
        format!(
            "su-security ON for this session (persisted in {sidecar}) — overriding this project's \
             user-security = false, so agent commands are confined. \
             Run /su-security off <reason> to allow them the invoking identity."
        )
    } else {
        format!(
            "su-security OFF for this session (persisted in {sidecar}) — overriding this project's \
             user-security = true, so agent commands run as the invoking user. \
             Run /su-security on to re-confine them."
        )
    })
}

/// Process-wide "kill every detached job" switch, created by the first
/// `Agent::new` and shared with every backend via
/// `Registry::set_job_kill_handle`. The REPL flips it when the user presses
/// ESC/ctrl-c (or quits) so detached jobs die even while the agent that owns
/// them is running on the turn's worker thread. Backends also poll it inside
/// their own wait loops (a detached job's parent command observes it too).
static JOB_KILL_FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// The process-wide job-kill switch (read by the REPL / wait loops). It
/// exists only after the first agent was built; before that there are no
/// jobs to kill.
pub fn job_kill_flag() -> Option<Arc<AtomicBool>> {
    JOB_KILL_FLAG.get().cloned()
}

use std::sync::{Arc, Mutex};

/// Output format for agent sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Human-readable terminal output (default).
    Plain,
    /// NDJSON events emitted to stdout for machine consumption.
    Json,
}

impl OutputFormat {
    pub fn is_json(self) -> bool {
        self == OutputFormat::Json
    }
}

/// Emit a single NDJSON line to stdout. In JSON mode, all structured events
/// go through here. Flushing is immediate so consumers can parse incrementally.
pub(crate) fn ndjson_emit(event: &serde_json::Value) {
    if let Ok(line) = serde_json::to_string(event) {
        println!("{line}");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
}

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
    /// Rewindable turn checkpoints for this session (Feature 8). Loaded from
    /// `<session>.checkpoints.json` so rewind works across a resume. `None`
    /// before the first turn, or when the session has no transcript.
    checkpoints: Option<crate::checkpoint::Store>,
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
    /// Whether this session is in **plan mode** (Feature 5): read/search only,
    /// no writes and no commands. Persisted next to the session log (`.plan`)
    /// so a resumed session keeps the posture — resuming into a half-written
    /// plan must not silently re-arm the ability to edit the repo.
    plan_mode: bool,
    /// When this agent is a sub-agent (Feature 6), which kind it is. Drives the
    /// extra system-prompt preamble and the tool-list filtering.
    subagent_kind: Option<crate::subagent::Kind>,
    /// OS-abstracted security guardrail (the `security` module). When `Some`,
    /// every tool call is pre-checked against it via [`SecurityContext::check`]
    /// ( default-open / configurable; writes scoped to this project;
    /// escalation is ask-only). When `None` no guardrail is consulted — the
    /// legacy per-project-user DAC boundary (if any) still applies. `Arc`
    /// because the same context may later be shared with background turns.
    security: Option<Arc<SecurityContext>>,
    /// Reasoning / "extended thinking" level for this session (see
    /// `config::ThinkingLevel`). Threaded through to the provider request
    /// (Anthropic thinking budget / OpenAI reasoning effort). `Off` (the
    /// default) sends no thinking control at all — matching the prior behaviour.
    /// Persisted next to the session log so a resumed session keeps the level.
    thinking: config::ThinkingLevel,
    /// Auto-retry policy. When `Some(n)`, a turn whose last finished turn was
    /// judged `retry` by the light model is re-run automatically up to `n` times,
    /// compacting history first when it is near the context cap. `None` (the
    /// default) means no auto-retry — the verdict is still computed for display,
    /// but the turn ends at the REPL so the user decides (or types their own
    /// retry). Set via `--auto-retry N` (0 = observe only).
    auto_retry: Option<usize>,
    /// Whether the model's reasoning/thinking content is shown on the terminal
    /// as it streams. When false, thinking blocks are still collected + logged
    /// but suppressed from the live output (toggle with `/thinking show`/
    /// `/thinking hide`; persisted per session).
    show_thinking: bool,
    /// Whether the agent's *text* reply is rendered to Markdown **incrementally**
    /// (in place, overwriting the previous partial render as tokens arrive)
    /// rather than as a single dump at the end of the turn. On by default; off
    /// via `PIR_INCREMENTAL_MD=0` / `--no-incremental`. Quiet (background)
    /// turns never render incrementally regardless (nothing is drawn).
    incremental_md: bool,
    /// Explicit-stop skill (`request_stop` tool + auto-nudge). On by default;
    /// off via `PIR_STOP_SKILL=0` / `"stopSkill": false` / `--no-stop-skill` /
    /// `/stop-skill off`. When on, the model ends its turn with an explicit
    /// `request_stop` call, and text-only answers are auto-nudged (up to
    /// [`STOP_NUDGE_MAX`] per turn) until it does.
    stop_skill: bool,
    /// Output format for this session. `Plain` (default) is human-readable
    /// terminal output; `Json` emits NDJSON events to stdout for machine
    /// consumption (headless / CI use). Set via `--output-format json`.
    output_format: OutputFormat,
    /// In-flight explicit stop for this turn (set by `run_stop_tool`, consumed
    /// after the tool batch). Reset at the start of every turn.
    stop_hit: Option<(StopDecision, String)>,
    /// The most recent explicit stop decision (sticky across turns; surfaced by
    /// `/stop-skill status` and tests).
    last_stop: Option<(StopDecision, String)>,
    /// Auto-nudges used in the current turn (reset each turn; bounded by
    /// [`STOP_NUDGE_MAX`]).
    stop_nudges: usize,
    /// Cached provider list (loaded once, reused for model switches / resume).
    /// Avoids re-reading and re-parsing `~/.pi/agent/models-store.json` on every
    /// `/model` switch, resume, and `apply_persisted_model` call.
    cached_providers: Vec<Provider>,
    /// Runaway-loop detector for the tool-use loop. Tracks the signature of the
    /// most recent tool-call batch *and* the assistant's text output; when the
    /// same signal repeats back-to-back [`LoopDetector::MAX_REPEATS`] times (the
    /// model re-issuing the identical tool call, or repeating the same sentence,
    /// because it's stuck), the turn is stopped with a banner instead of
    /// spinning forever. Reset at the start of each turn.
    loop_detector: LoopDetector,
    /// Wall-clock start of the current turn (stamped in [`Self::turn`]).
    /// [`Self::turn_done_event`] reports its elapsed time so the "turn done
    /// in Ns" notification shows the real duration; `None` before the first
    /// turn (one-shot paths then report zero, as before).
    turn_started: Option<std::time::Instant>,
}

/// Detects a model stuck repeating itself over and over (the classic "Let me
/// look at the frontend's main.go" / "Let me search for X" infinite loop). Two
/// signals are tracked — (a) the tool-call batch (tool names + normalized
/// inputs), and (b) the assistant's text output (normalized).
///
/// The tool signal fires when the *identical* batch repeats [`MAX_REPEATS`]
/// times in a row (the model re-issuing the same tools). The text signal is
/// deliberately conservative: it only fires when the model is NOT making
/// progress via distinct tool calls — i.e. it is re-issuing the same tool batch,
/// or emitting no tools at all (a pure text loop). A model that repeats a short
/// preamble while reading different files is progressing and must never be
/// flagged. The detector only fires on *consecutive identical* signals, so
/// legitimate repeated reads of the same file interleaved with other work never
/// trip it.
struct LoopDetector {
    /// Signature of the previous tool-call batch (None before the first).
    prev_tool: Option<String>,
    /// How many consecutive times the current tool signature has repeated.
    tool_repeats: usize,
    /// Signature of the previous assistant text output (None before the first).
    prev_text: Option<String>,
    /// How many consecutive times the current text signature has repeated.
    text_repeats: usize,
}

impl LoopDetector {
    /// How many consecutive identical signals before we declare a loop. 3 gives
    /// the model two chances to break out after the first repeat.
    const MAX_REPEATS: usize = 3;

    fn new() -> Self {
        LoopDetector {
            prev_tool: None,
            tool_repeats: 0,
            prev_text: None,
            text_repeats: 0,
        }
    }

    /// Feed the current tool-call batch and assistant text signatures.
    /// Returns `true` when a runaway loop is detected.
    ///
    /// The tool signal fires when the *identical* batch repeats
    /// [`MAX_REPEATS`] times in a row (the model re-issuing the same tools).
    ///
    /// The text signal is deliberately conservative: it only fires when the
    /// model is NOT making progress via distinct tool calls — i.e. it is
    /// re-issuing the same tool batch, or emitting no tools at all (a pure
    /// text loop). A model that repeats a short preamble while reading
    /// different files is progressing and must never be flagged.
    fn observe(&mut self, tool_sig: &str, text_sig: &str) -> bool {
        // Empty observations (a stalled stream yielding `(empty response)`
        // with no tools) carry no signal: neither repeating nor progressing.
        // Counting them lets three consecutive stall retries trip the *tool*
        // signal (empty == empty) and kill a turn the stop-skill nudge path
        // would otherwise recover. Skip them without touching the counters.
        if tool_sig.is_empty() && text_sig.is_empty() {
            return false;
        }
        // Tool signal: fire when the identical batch repeats MAX_REPEATS times.
        let tool_repeating = self.prev_tool.as_deref() == Some(tool_sig);
        if tool_repeating {
            self.tool_repeats += 1;
        } else {
            self.prev_tool = Some(tool_sig.to_string());
            self.tool_repeats = 1;
        }
        if self.tool_repeats >= Self::MAX_REPEATS {
            return true;
        }

        // Text signal: only fire when the model is stuck on tools too. If the
        // tool batch is changing (and there are tools at all), the model is
        // progressing — reset the text counter so a repeated preamble never
        // trips the detector.
        let tools_progressing = !tool_sig.is_empty() && !tool_repeating;
        if tools_progressing {
            self.prev_text = Some(text_sig.to_string());
            self.text_repeats = 1;
            return false;
        }

        if self.prev_text.as_deref() == Some(text_sig) {
            self.text_repeats += 1;
        } else {
            self.prev_text = Some(text_sig.to_string());
            self.text_repeats = 1;
        }
        self.text_repeats >= Self::MAX_REPEATS
    }
}

/// Fingerprint a tool-call batch for loop detection: the tool name plus a
/// normalized (whitespace-collapsed) form of its JSON input. Two batches that
/// issue the same tools with the same arguments produce the same signature, so
/// a stuck model re-issuing `read_file("main.go")` is caught.
fn tool_batch_signature(calls: &[(String, String, Value)]) -> String {
    let mut out = String::new();
    for (_, name, input) in calls {
        out.push_str(name);
        out.push('\u{1}');
        // Collapse whitespace so formatting differences don't defeat detection.
        let s = input.to_string();
        let mut prev_space = false;
        for c in s.chars() {
            if c.is_whitespace() {
                if !prev_space {
                    out.push(' ');
                }
                prev_space = true;
            } else {
                out.push(c);
                prev_space = false;
            }
        }
        out.push('\u{1e}');
    }
    out
}

/// Fingerprint an assistant text output for loop detection: whitespace-collapsed
/// and lowercased, so a model repeating the same sentence (e.g. "Let me look at
/// the frontend's main.go") with trivial casing/whitespace drift is caught.
fn text_signature(text: &str) -> String {
    let mut out = String::new();
    let mut prev_space = false;
    for c in text.chars() {
        let c = c.to_lowercase().next().unwrap_or(c);
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
}

/// Explicit-stop decision the model can make with the `request_stop` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopDecision {
    /// Task finished; end the turn with a `Completed` status.
    Complete,
    /// Needs operator input; end the turn with an `Interrupted` status whose
    /// reason carries the model's question, so `/unfinished` shows it.
    NeedsInput,
}

impl StopDecision {
    /// Parse the free vocabulary the model may use for the `reason` field.
    pub fn parse(s: &str) -> Option<StopDecision> {
        match s.trim().to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
            "complete" | "completed" | "done" | "finished" | "stop_complete" => {
                Some(StopDecision::Complete)
            }
            "needs_input" | "need_input" | "needsinput" | "input" | "question" | "ask"
            | "stopped_needs_input" | "stop_needs_input" => Some(StopDecision::NeedsInput),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StopDecision::Complete => "stop-complete",
            StopDecision::NeedsInput => "stop-needs-input",
        }
    }
}

/// How many auto-nudges a turn may spend before giving up and ending it: each
/// text-only answer costs one model call, so the cap also caps the spend. 8
/// gives the model seven chances to comply after the first miss and still
/// keeps a stuck turn to a handful of cheap calls.
pub const STOP_NUDGE_MAX: usize = 8;

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
                term::dim(self.first_prompt.lines().next().unwrap_or("").trim())
            ));
        }
        if !self.last_prompt.is_empty() {
            out.push_str(&format!(
                "{} last  prompt: {}\n",
                term::dim("·"),
                term::dim(self.last_prompt.lines().next().unwrap_or("").trim())
            ));
        }
        if !self.last_output.is_empty() {
            out.push_str(&term::dim(&rule));
            out.push('\n');
            out.push_str(&term::dim("last output (tail):\n"));
            let rendered = crate::md::render(&self.last_output, false);
            out.push_str(&tail_lines(&rendered, 40));
            out.push('\n');
            out.push_str(&term::dim(&rule));
        }
        out
    }
}

/// Context files for a worktree, pi parity (`AGENTS.md` walking up from cwd):
/// the global `~/.pi/agent/AGENTS.md` first, then one file per ancestor
/// directory from the filesystem root down to `cwd` — `AGENTS.override.md`
/// wins over `AGENTS.md`, which wins over `CLAUDE.md`. Returns
/// (display-path, content) pairs in load order. Pure w.r.t. process state
/// (takes the dir explicitly) for tests.
pub(crate) fn context_files(cwd: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let global = config::pi_dir().join("AGENTS.md");
    if let Ok(s) = fs::read_to_string(&global)
        && !s.trim().is_empty()
    {
        out.push((global, s));
    }
    let abs = crate::security::canonicalize_lenient(cwd);
    let mut dirs: Vec<PathBuf> = abs.ancestors().map(|a| a.to_path_buf()).collect();
    dirs.reverse(); // root first, cwd last (general -> specific)
    for dir in dirs.into_iter().take(128) {
        let pick = ["AGENTS.override.md", "AGENTS.md", "CLAUDE.md"]
            .iter()
            .map(|n| dir.join(n))
            .find_map(|p| fs::read_to_string(&p).ok().map(|s| (p, s)));
        if let Some((p, s)) = pick
            && !s.trim().is_empty()
        {
            out.push((p, s));
        }
    }
    out
}

/// Build the base system prompt (prompt/output parity with pi's *shape*,
/// pir's *content* — see docs/PROMPT_PARITY.md §2). Both constructors share
/// it; goal state is appended separately by `refresh_system`.
/// Clip a plan body for display inside the approval dialog (the full document
/// is on disk and in the tool result; the dialog only needs enough to judge).
fn clip_for_dialog(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
    format!("{}…\n\n(full plan is in the saved file)", &s[..cut])
}

fn build_system_prompt(cwd: &Path) -> String {
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
        "\nAvailable tools:\n\
         - bash: Run a shell command in the project directory\n\
         - read_file: Read a UTF-8 text file (truncated past 100k chars)\n\
         - write_file: Create or overwrite a file\n\
         - edit_file: Replace exactly one occurrence of old_string with new_string\n\
         - list_dir: List the entries of a directory (non-recursive)\n\
         - glob: Find files by path glob (e.g. '**/*.rs'), newest-modified first\n\
         - grep: Regex content search (ripgrep-backed; respects .gitignore)\n\
         - ask_user_question: Pause and ask the operator a structured question (when a\n\
           decision genuinely blocks progress); returns their answer as the tool result\n\
         - load_skill: Load a skill's full instructions (skills are listed in the Skills\n\
           section below) before doing work the skill covers\n\
         - plan_write: In plan mode, record a markdown plan for the operator to approve\n\
         - agent: Delegate a self-contained task to a sub-agent and get back only its report\n\
         - web_fetch: Fetch a public http/https URL and return its readable text\n\
         - web_search: Search the web (provider-independent) for titles, URLs and snippets\n\
         - remember: Record a durable preference so it applies in later sessions\n\
         (rewind: the operator can restore a turn's files and your conversation with /rewind)\n\
         - job_status: Check on a long-running command that was detached\n\
         - job_kill: Stop a detached long-running command\n\
         - update_goal: Persist and update the current goal/continuation plan\n\
         - request_stop: End your turn with an explicit stop decision, but ONLY when \
           the work is really done (reason stop-complete) or you genuinely need \
           the operator to answer before you can continue (reason \
           stop-needs-input, with your question in message). If work remains, \
           keep using tools — do NOT call this just to end a message.\n\
         \n\
         In addition to the tools above, you may have access to other custom tools depending on the project.\n",
    );
    system.push_str(
        "\nGuidelines:\n\
         - Use the tools to actually do the work; don't just describe it.\n\
         - Use bash for file operations; prefer rg over grep/find (both are wrapped to rg by default).\n\
         - Use read_file to examine files instead of cat or sed.\n\
         - Use write_file only for new files or complete rewrites.\n\
         - Use edit_file for precise changes (old_string must match exactly).\n\
         - Keep old_string as small as possible while still being unique in the file.\n\
         - Read before editing; prefer edit_file over write_file for changes.\n\
         - Be terse: code, commands, short answers, no preamble.\n\
         - Show file paths clearly when working with files.\n\
         - When finished, summarize what changed in a sentence or two.\n\
         - End your turn with request_stop ONLY when the work is really done \
           (reason stop-complete) or you genuinely need the operator's answer \
           to continue (reason stop-needs-input, with your question in \
           message). If work remains, keep using tools instead of stopping. \
           Never end with request_stop on an unfinished task, and never answer \
           with text alone when tools could progress the work.\n",
    );
    system.push_str(
        "\nPIR documentation (read only when the user asks about pir itself, its extensions, themes, skills, or TUI):\n\
         - Main documentation: docs/ in the pir source tree.\n\
         - When asked about: extensions, themes, skills, prompt templates, TUI components, keybindings, SDK, custom providers, models, packages, environment variables.\n\
         - Always read pir .md files completely and follow links to related docs.\n\
         - pir sets PIR_* environment variables you can inspect (invoking user, worktree, quarantine state).\n",
    );
    // Project instructions in pi's <project_context> shape (parity) instead
    // of the old `# Extra instructions` heading.
    let mut projects = String::new();
    for (p, s) in context_files(cwd) {
        projects.push_str(&format!(
            "Project-specific instructions and guidelines:\n\n<project_instructions path=\"{}\">\n{}\n</project_instructions>\n",
            p.display(),
            s.trim()
        ));
    }
    if !projects.is_empty() {
        system.push_str("\n<project_context>\n");
        system.push_str(&projects);
        system.push_str("</project_context>\n");
    }
    system.push_str(&format!("\nCurrent working directory: {}\n", cwd.display()));
    // The per-session scratchpad (Feature 2): a writable area outside the repo
    // for intermediate artifacts. Injected only when one exists, so a session
    // that couldn't create it doesn't advertise a path that isn't there.
    if let Some(sp) = crate::scratchpad::current() {
        system.push_str(&format!(
            "Scratchpad: {} (exported as $PIR_SCRATCHPAD) — use it for intermediate files, \
             generated fixtures and logs; never write scratch files into the repo itself.\n",
            sp.display()
        ));
    }
    // Skills catalog (Feature 4): names + one-line descriptions only. The full
    // instruction body is pulled in on demand via `load_skill`, which is what
    // keeps a dozen detailed procedures from costing context on every request.
    system.push_str(&crate::skills::catalog(&crate::skills::discover(cwd)));
    // Taste (Feature 12): durable preferences recorded by the agent or the
    // operator, injected so they survive into every later session.
    system.push_str(&crate::taste::catalog(&crate::taste::load_all(cwd)));
    system
}

impl Agent {
    /// `resume_from`, if set, continues the given session's log file instead
    /// of starting a fresh one (its parent-shell tag is preserved). `quiet`
    /// suppresses all terminal output (used for backgrounded sessions). `bus`
    /// is the shared notification bus all agents publish to. `typeahead` is a
    /// shared buffer the REPL fills with keystrokes typed while the turn runs;
    /// the thinking spinner reads it so the user's input is shown while the
    /// model thinks.
    #[allow(clippy::too_many_arguments)]
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
        // Share the REPL's "kill every detached job" switch so an ESC/ctrl-c
        // (or quitting) also stops long-running commands the backend detached
        // into background jobs — previously they kept running, held the output
        // pipes, and wedged later commands and `job_kill`.
        let job_kill_flag = Arc::new(AtomicBool::new(false));
        registry.set_job_kill_handle(job_kill_flag.clone());
        let _ = JOB_KILL_FLAG.set(job_kill_flag);
        crate::register_all(&mut registry);
        registry.session_started(&cwd);
        // Build the OS-abstracted security guardrail from `security.toml`
        // (reads default-open / configurable; writes scoped to this project;
        // escalation ask-only). `None` -> no guardrail consulted. The context
        // is built once and shared; its `check` is the single entry point the
        // tool path calls before each tool runs.
        let mut policy = crate::security::load_policy();
        // No per-project sandbox user was provisioned at startup (fresh
        // checkout, never `pir project init`): the operator has not asked for
        // the user-security boundary and there is no account to enforce it, so
        // force it off here. Its write-quarantine goes with it — mounting
        // system-tree overlays for a session that runs as the invoking user is
        // wrong, and was the dominant cost of a cold start (see main.rs).
        // `PIR_NO_SANDBOX_USER` is set only by that startup fallback, so
        // background agents inherit the same posture and unit tests are
        // unaffected.
        if std::env::var_os("PIR_NO_SANDBOX_USER").is_some() {
            policy.user_security = false;
            policy.quarantine = false;
            policy.quarantine_project = false;
        }
        // The "user-security" policy IS the per-project user boundary: when it
        // is OFF the agent must not be confined to the sandbox user, so its
        // commands run as the *invoking* user (mirrors `/su-security off`).
        // Capture it here before `policy` is moved into the context, and use it
        // to seed the session's su-security authority below.
        let user_security = policy.user_security;
        // When user-security is OFF the agent must not be confined to its
        // sandbox user: mirror `/su-security off` so commands run as the
        // invoking user (drop_to_agent_user / /sh read this env).
        if !user_security {
            // SAFETY: edition 2024 marks env mutation unsafe; pir confines
            // it to startup config and explicit session toggles.
            unsafe { std::env::set_var("PIR_AGENT_AS_INVOKER", "1"); }
        }
        // Mitigation-level security (docs/MITIGATION_LEVEL_SECURITY.md) is
        // active when the policy level is `mitigation` (or the default
        // guard posture, which also runs the safety pre-filter). The builtin
        // `bash` tool reads this flag to decide whether to run the analyzer.
        crate::security::set_mitigation_active(policy.level.is_mitigation() || policy.level == crate::security::SecurityLevel::Guard);
        let security = {
            let headless = std::env::var("PIR_HEADLESS").is_ok();
            let ctx = crate::security::SecurityContext::new(policy, headless);
            // Overlayfs write-quarantine is a *launcher* concern: it mounts
            // overlays over /var, /etc, ... and must never run inside unit tests
            // (tests construct Agents directly; mounting would shadow the test
            // host and poison the process-global quarantine flags).
            #[cfg(all(not(test), unix))]
            if std::env::var_os("PIR_SUBAGENT").is_none() {
                // The quarantine is set up once, by the *launcher*. A sub-agent
                // runs in the same process, so re-entering a private mount
                // namespace / re-mounting the overlays for it is both pointless
                // and actively harmful (it would try to overlay /etc a second
                // time in a namespace it cannot leave, and can kill the child
                // outright). `PIR_SUBAGENT` is set for the duration of a child's
                // construction, so the child simply inherits the parent's
                // already-established posture.
                // Scope every overlay we mount to THIS agent's private mount
                // namespace (see enter_private_mount_ns) so we quarantine the
                // agent's writes only, never the host's. If we can't get a private
                // namespace we must NOT mount (that would shadow /var, /etc, ...
                // for the whole system); we fall back to the in-process hard-deny
                // guardrail instead. Only enter the namespace when quarantine is
                // actually engaged: entering a user+mount ns is permanent for
                // the process (cannot setns back out) and remaps file ownership,
                // so entering it when quarantine is off needlessly breaks things
                // (e.g. root file writes appearing as nobody).
                // FULL-ROOT / container mode: the whole fs is already handled; the
                // selective system-tree overlay is redundant (skip it).
                let fullroot = crate::security::overlay::fullroot_engaged()
                    || crate::security::overlay::container_engaged();
                // Only enter the private userns/mntns when we'll actually mount an overlay.
                let private_ns = (ctx.policy.quarantine && !fullroot)
                    && crate::security::overlay::enter_private_mount_ns().is_ok();
                // NON-ROOT auto-writable mode (default for unprivileged): overlay
                // $HOME with fuse-overlayfs, worktree + ~/.cargo + ~/.pi real.
                let home_q = crate::security::overlay::home_quarantine_wanted();
                if home_q && ctx.policy.quarantine {
                    match crate::security::overlay::mount_home_quarantine() {
                        Ok(()) => eprintln!(
                            "{}",
                            crate::term::dim(
                                "[pir] HOME write-quarantine engaged: $HOME staged (worktree + ~/.cargo + ~/.pi real; /tmp excluded) — review with /quarantine"
                            )
                        ),
                        Err(e) => {
                            eprintln!(
                                "{}",
                                crate::term::red(&format!(
                                    "[pir] HOME QUARANTINE UNAVAILABLE ({e}); writes are UNGUARDED except the in-process deny-list"
                                ))
                            );
                            ctx.set_quarantine(false);
                        }
                    }
                } else if ctx.policy.quarantine && !fullroot {
                    if !private_ns {
                        eprintln!(
                            "{}",
                            crate::term::red(
                                "[pir] WRITE-QUARANTINE DISABLED: private mount namespace unavailable (would shadow the host's /var, /etc); writes are UNGUARDED except the in-process deny-list"
                            )
                        );
                        ctx.set_quarantine(false);
                    } else {
                        let mut q = crate::security::overlay::Quarantine::from_policy(&ctx.policy);
                        match q.mount() {
                            Ok(n) => {
                                crate::security::overlay::set_active(q);
                                ctx.set_quarantine(true);
                                if n == 0 {
                                    eprintln!(
                                        "{}",
                                        crate::term::dim(
                                            "[pir] write-quarantine engaged (no existing system trees to overlay yet; writes will stage on demand)"
                                        )
                                    );
                                }
                            }
                            Err(reason) => {
                                eprintln!(
                                    "{}",
                                    crate::term::dim(&format!(
                                        "[pir] write-quarantine not engaged ({reason}); writes are guarded in-process only"
                                    ))
                                );
                                ctx.set_quarantine(false);
                            }
                        }
                    }
                }
            }
            Some(ctx)
        };
        // Emit SessionStart so backends (e.g. the pi-extensions bridge) can
        // spawn their child processes / load resources now that the agent and
        // its cwd are known.
        registry.emit(EventKind::SessionStart, &json!({ "cwd": cwd.display().to_string() }));

        let system = build_system_prompt(&cwd);

        let (log, log_path) = open_log(resume_from);

        // Per-session scratch directory (Feature 2): a private writable area
        // outside the project tree, exported to every child process as
        // `$PIR_SCRATCHPAD`. Created here (after the log path is known, so the
        // dir name matches the transcript) and swept of stale dirs at the same
        // time. A failure is non-fatal: the agent simply runs without one.
        let _scratchpad = crate::scratchpad::init(log_path.as_deref());
        // Rewindable turn checkpoints (Feature 8), loaded from the sidecar next
        // to the transcript so a resume can rewind work done before it.
        let checkpoints = crate::checkpoint::Store::load_opt(log_path.as_deref());
        let _ = crate::scratchpad::sweep_old();
        // Seed the bundled skills (the `locate` skill, and any future ones) into
        // the config skills root on first run. Idempotent and never
        // overwriting, so an operator's edited copy survives an upgrade.
        let _ = crate::skills::seed_bundled();

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
            checkpoints,
            undo_stack: Vec::new(),
            su_security_enabled: user_security,
            plan_mode: false,
            subagent_kind: None,
            security,
            thinking: config::ThinkingLevel::Off,
            show_thinking: true,
            auto_retry: None,
            incremental_md: config::incremental_md_default(),
            stop_skill: config::stop_skill_default(),
            output_format: OutputFormat::Plain,
            stop_hit: None,
            last_stop: None,
            stop_nudges: 0,
            cached_providers,
            loop_detector: LoopDetector::new(),
            turn_started: None,
        })
    }

    /// Whether the explicit-stop skill is enabled for this session.
    pub fn stop_skill(&self) -> bool {
        self.stop_skill
    }

    /// The most recent explicit stop decision (sticky across turns), if any.
    pub fn last_stop(&self) -> Option<StopDecision> {
        self.last_stop.as_ref().map(|(d, _)| *d)
    }

    /// Toggle the explicit-stop skill for this session. On (the default) the
    /// model ends its turn with an explicit `request_stop` call, and text-only
    /// answers are auto-nudged until it does; off restores the legacy
    /// stop-on-first-text behaviour. Returns a status line.
    pub fn set_stop_skill(&mut self, on: bool) -> String {
        self.stop_skill = on;
        if on {
            "stop-skill: on  (model ends its turn with request_stop; text-only answers are nudged until it does)"
        } else {
            "stop-skill: off  (legacy stop-on-first-text behaviour)"
        }
        .to_string()
    }

    /// Set the output format for this session.
    pub fn set_output_format(&mut self, fmt: OutputFormat) {
        self.output_format = fmt;
    }

    /// Whether this session is emitting NDJSON events.
    pub fn is_json(&self) -> bool {
        self.output_format.is_json()
    }

    /// Emit an NDJSON event if output format is JSON. No-op in Plain mode.
    fn emit_json(&self, event: &serde_json::Value) {
        if self.output_format.is_json() {
            ndjson_emit(event);
        }
    }

    /// How many tool calls the model made across this session's history. Used by
    /// sub-agent delegation to describe a silent child's activity.
    pub fn tool_call_count(&self) -> usize {
        self.history.iter().map(|m| m.tool_uses().len()).sum()
    }

    /// Extract the text content of the last assistant message in history.
    /// Used by the NDJSON result event to include the model's reply, and by
    /// sub-agent delegation (Feature 6) to build the child's report.
    ///
    /// Falls back to the most recent *non-empty* assistant text when the very
    /// last assistant message carries none: a model that ends on a tool call or
    /// a thinking-only block leaves the final message empty, and reporting
    /// "no output" there would discard a perfectly good report the child had
    /// already written.
    pub fn last_reply_text(&self) -> String {
        let mut last_any = String::new();
        for msg in self.history.iter().rev() {
            if msg.role != crate::types::Role::Assistant {
                continue;
            }
            let t = msg.text();
            if last_any.is_empty() {
                last_any = t.clone();
            }
            if !t.trim().is_empty() {
                return t;
            }
        }
        last_any
    }

    /// Intercept the `request_stop` tool: record the model's explicit stop
    /// decision so the turn ends after this tool batch. Other tools fall
    /// through to the registry. Returns `None` for non-stop tools.
    fn run_stop_tool(&mut self, name: &str, input: &Value) -> Option<Outcome> {
        if name != "request_stop" {
            return None;
        }
        if !self.stop_skill {
            return Some(Outcome {
                content: "request_stop is disabled for this session (stop-skill off); text answers end the turn."
                    .into(),
                is_error: true,
            });
        }
        let reason = input
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("");
        let Some(decision) = StopDecision::parse(reason) else {
            return Some(Outcome {
                content: format!(
                    "request_stop: unknown reason '{reason}' (want stop-complete or stop-needs-input)"
                ),
                is_error: true,
            });
        };
        let why = input
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        self.stop_hit = Some((decision, why.clone()));
        self.last_stop = Some((decision, why.clone()));
        Some(Outcome {
            content: format!(
                "stop recorded ({}){} — finishing this tool batch, then ending the turn.",
                decision.label(),
                if why.is_empty() { String::new() } else { format!(" {why}") }
            ),
            is_error: false,
        })
    }
    /// Inject (or refresh) the current goal snapshot into the system prompt so
    /// the model always sees the live plan without it being part of `history`.
    fn refresh_system(&mut self) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut system = build_system_prompt(&cwd);
        if self.plan_mode {
            system.push_str(
                "\n# PLAN MODE (read-only)\n\n\
                 You are in plan mode. Explore with read_file, glob, grep and list_dir; \
                 `bash`, write_file and edit_file are refused by the security policy and will \
                 keep being refused however you phrase the call. When you understand the task, \
                 call the `plan_write` tool with a markdown plan: the goal, the approach, the \
                 files you will touch, the risks, and how the change will be verified. The \
                 operator reviews it; on approval you leave plan mode and implement it.\n",
            );
        }
        if let Some(kind) = self.subagent_kind {
            system.push_str(kind.preamble());
        }
        if let Some(store) = &self.goal_store {
            system.push_str("\n# Current goal (persisted — survives interrupts; resume with `pir -c`)\n\n");
            system.push_str(&store.goal.summary());
            system.push_str("\nWork the next pending step. Update progress with the update_goal tool.\n");
        }
        self.system = system;
    }

    /// Rebuild the system prompt from scratch (goal + plan mode + taste).
    /// Exposed so the REPL's `/taste` can push a just-recorded preference into
    /// the live prompt without waiting for a new session.
    pub fn refresh_system_public(&mut self) {
        self.refresh_system();
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

    /// Attach (or replace) the security guardrail context. Called once at
    /// startup after `Agent::new` builds the default from `security.toml`;
    /// exposed so a host launcher (e.g. the namespace wrapper) can inject a
    /// context it constructed (e.g. with the project owner for escalation reaps).
    pub fn set_security_context(&mut self, ctx: Arc<SecurityContext>) {
        self.security = Some(ctx);
    }

    /// Pre-flight guardrail check for a single tool call. Returns `Some((reason,
    /// terminate))` if the security context denies the op (the operator may
    /// have granted it via the request sink, in which case the context returns
    /// `Allow` and we return `None`). This is the bridge between the policy
    /// layer (`security::decide` -> `SecurityContext::check`) and the tool loop:
    /// reads are allowed by default, writes scoped to this project are allowed,
    /// and escalation/`BecomeRoot`/`Apt` become an *ask* surfaced to the
    /// operator (TTY prompt, or a queued request when headless).
    fn security_preflight(&self, name: &str, input: &Value) -> Option<(String, bool)> {
        let ctx = self.security.as_ref()?;
        use crate::security::{Ask, Op};
        let ask = match name {
            "read_file" => Ask::new(Op::Read).with_reason("read a file"),
            "write_file" | "edit_file" => {
                let path = input.get("path").and_then(Value::as_str).unwrap_or("").to_string();
                Ask::write(path).with_reason("write a file")
            }
            "list_dir" => Ask::new(Op::Read).with_reason("list a directory"),
            "bash" => {
                // The default posture is "run all commands": the write-quarantine
                // is enforced by the overlayfs layer (syscall-level), not by this
                // in-process preflight (which cannot see individual syscalls). So
                // bash is surfaced as an `Exec` op, which `decide` allows by
                // default; writes still stage into the overlay upper and are
                // reviewed via /quarantine. (A stricter posture can gate exec
                // separately; the per-syscall guardrail for secret/critical
                // paths holds at the mount layer.)
                Ask::new(Op::Exec).with_reason("run a shell command")
            }
            _ => return None,
        };
        match ctx.check(&ask) {
            crate::security::Verdict::Allow => None,
            crate::security::Verdict::Deny { parcel, risk } => {
                let reason = format!(
                    "denied by security policy (parcel {} / risk {}): {}",
                    parcel.id(),
                    risk.as_str(),
                    parcel.blast_radius()
                );
                Some((reason, false))
            }
        }
    }

    /// True when the turn should not write to the terminal, either because the
    /// agent was built quiet (background job) or the REPL asked an in-flight
    /// foreground turn to go quiet (detach to background).
    fn silent(&self) -> bool {
        self.quiet || self.quiet_req.load(Ordering::SeqCst)
    }

    /// Build a **sub-agent** (Feature 6): a child agent that runs one turn and
    /// reports back, used by the `agent` tool.
    ///
    /// Differences from a normal agent:
    ///  * always quiet — its turn must never print to the parent's terminal;
    ///  * `ask_user_question`, `plan_write` (unless a planning child) and
    ///    `request_stop` are removed from its tool list. There is no operator
    ///    attached to a sub-agent and no REPL channel to reach one, so those
    ///    tools could only ever produce a confusing failure;
    ///  * the security policy is *clamped* to the child's kind, never widened:
    ///    `explore`/`plan` run at `SecurityLevel::Plan` (read-only) regardless
    ///    of the parent's level, and `general` inherits the parent's policy
    ///    verbatim.
    pub fn new_subagent(
        provider: Provider,
        model: Model,
        full_auto: bool,
        kind: crate::subagent::Kind,
        policy: Option<crate::security::SecurityPolicy>,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        // Mark the construction as a sub-agent so `Agent::new` skips the
        // launcher-only work (overlay quarantine mounts, which are per-process
        // and would kill a second attempt inside this one). Cleared on the way
        // out — including on the error path — so it never leaks into a later
        // top-level agent in the same process.
        // SAFETY: edition 2024 marks env mutation unsafe; this is scoped to the
        // construction call and restored immediately below.
        unsafe { std::env::set_var("PIR_SUBAGENT", "1"); }
        let built = Agent::new(
            provider,
            model,
            full_auto,
            true, // quiet: the parent owns the terminal during its turn
            crate::notify::shared_bus(),
            None,
            cancel,
            Arc::new(Mutex::new(String::new())),
        );
        unsafe { std::env::remove_var("PIR_SUBAGENT"); }
        let mut a = built?;
        // Sub-agents have no `request_stop` tool (there is no operator to report
        // to), so the explicit-stop skill must be off — otherwise every child
        // would burn its nudges demanding a tool it cannot call before finally
        // ending on a text reply.
        a.stop_skill = false;
        // Clamp the posture. `explore` and `plan` are read-only by definition;
        // `general` keeps whatever the parent had (never a weaker one).
        if matches!(kind, crate::subagent::Kind::Explore | crate::subagent::Kind::Plan) {
            let mut p = policy.unwrap_or_else(crate::security::load_policy);
            p.level = crate::security::SecurityLevel::Plan;
            a.apply_security_policy(&p);
            a.plan_mode = matches!(kind, crate::subagent::Kind::Plan);
        } else if let Some(p) = policy {
            a.apply_security_policy(&p);
        }
        // A sub-agent's write tools are hidden, not merely refused, for the
        // read-only kinds: a model that cannot see `write_file` will not spend a
        // round-trip discovering it is refused.
        a.subagent_kind = Some(kind);
        a.refresh_system();
        Ok(a)
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

    /// Kill every long-running command any backend detached into a background
    /// job (ESC/ctrl-c / quit sweep). Bounded: backend implementations must
    /// use poll-based kills so this never blocks the REPL's input thread.
    /// Returns how many running jobs were killed.
    pub fn registry_kill_all_jobs(&mut self) -> usize {
        self.registry.kill_all_jobs()
    }

    /// Hard-abort the foreground `bash` command *immediately* (without waiting
    /// for the turn to finish). The `bash` tool polls this flag between waits
    /// and kills its child process group at once, so an in-flight command dies
    /// now instead of after it exits on its own. Used by the ctrl-d/ESC-quit
    /// paths so the session never has to wait on a long-running command to
    /// leave. Always safe to call (harmless no-op if no command runs).
    pub fn registry_abort_active_command(&mut self) -> bool {
        self.registry.abort_active_command()
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

    /// Re-read the model catalog from disk and refresh the agent's cached copy
    /// (`/reload`). The catalog is otherwise loaded once in [`Agent::new`], so
    /// editing `~/.pi/models.json` (adding a provider, a key, a new model) had
    /// no effect until pir was restarted.
    ///
    /// The *active* model is re-resolved against the fresh catalog by label:
    /// if the same `provider/model` still exists, its definition is replaced
    /// (so edits to context size, prices, `max_tokens`, … take effect
    /// immediately); if it vanished, the switch is left alone and reported, so
    /// a reload can never silently move the user onto a different model.
    ///
    /// Returns `(providers_loaded, active_model_refreshed)`.
    pub fn reload_catalog(&mut self) -> (usize, bool) {
        let fresh = match crate::config::load_providers() {
            Ok(p) if !p.is_empty() => p,
            // Keep the working catalog rather than emptying it on a transient
            // read failure (the store may be unreadable mid-write).
            _ => return (self.cached_providers.len(), false),
        };
        let n = fresh.len();
        let label = format!("{}/{}", self.provider.pid(), self.model.id);
        let refreshed = match crate::config::select(&fresh, &label) {
            Ok((p, m)) => {
                let (p, m) = (p.clone(), m.clone());
                // Replace the provider/model definitions in place. `switch`
                // also rebuilds the HTTP client, so changed base_url / headers
                // / auth from the store are picked up too.
                self.switch(p, m).is_ok()
            }
            Err(_) => false,
        };
        self.cached_providers = fresh;
        (n, refreshed)
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

    /// Whether the history fits (token-wise) without trimming
    pub fn needs_compaction(&self) -> bool {
        let ctx = self.model.context.unwrap_or(200_000) as usize;
        let out_reserve = self.model.max_tokens.unwrap_or(8192) as usize;
        let sys_reserve = 2_000;
        let budget = ctx
            .saturating_sub(out_reserve)
            .saturating_sub(sys_reserve)
            .max(8192);
        approx_tokens(&self.history) > budget
    }

    pub fn compact(&mut self) -> String {
        if self.history.is_empty() {
            return "nothing to compact (no history)".to_string();
        }
        let before_tokens = approx_tokens(&self.history);
        let sys = self.system.clone();
        let msgs: Vec<Message> = self.history.clone();
        let mut transcript = String::new();
        for m in &msgs {
            let role = match m.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
            };
            let text = m.text();
            if !text.trim().is_empty() {
                transcript.push_str(&format!("{role}: {text}\n"));
            }
            for (_, name, input) in m.tool_uses() {
                transcript.push_str(&format!("{role} called tool `{name}` with {input}\n"));
            }
            for b in &m.blocks {
                if let Block::ToolResult { content, is_error, .. } = b {
                    let tag = if *is_error { "Tool error" } else { "Tool result" };
                    transcript.push_str(&format!("{tag}: {content}\n"));
                }
            }
        }
        if transcript.is_empty() {
            return "nothing to compact (empty conversation)".to_string();
        }
        let compact_prompt = format!(
            "Summarize the following coding session so far in a concise but complete way. \
             Include: the user's goal(s), key decisions, files touched, important code/commands, \
             current state, and any pending work. Do not include any preamble or meta commentary.\n\n{}",
            transcript
        );
        match self.client.complete(&self.model.id, &sys, &compact_prompt) {
            Ok(summary) if !summary.trim().is_empty() => {
                let s = summary.trim().to_string();
                self.history.clear();
                self.history.push(Message::user(&format!(
                    "[pir: conversation compacted — summary of earlier context]\n\n{s}"
                )));
                let after_tokens = approx_tokens(&self.history);
                term::out(&term::dim(&format!(
                    "[pir: compacted history ({} → {} tokens est.)]",
                    before_tokens, after_tokens
                )));
                format!(
                    "history compacted ({} → {} tokens est.) — kept a summary of the conversation",
                    before_tokens, after_tokens
                )
            }
            Ok(_) => "compaction produced empty summary; history unchanged".to_string(),
            Err(e) => format!("compaction failed: {e} (history unchanged)"),
        }
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

    /// The active security policy (level, apt, network, ask, read, quarantine).
    /// Returns `None` when no guardrail is configured.
    pub fn security_policy(&self) -> Option<crate::security::SecurityPolicy> {
        self.security.as_ref().map(|c| c.policy.clone())
    }

    /// Flip the live su-security flag and wire the authority bash consults.
    /// While su-security is off, bash must NOT drop to `ai_X` (the drop reads
    /// this env), so the agent can act as the invoking user.
    fn set_su_security_flag(&mut self, enabled: bool) {
        self.su_security_enabled = enabled;
        // SAFETY: edition 2024 marks env mutation unsafe; pir confines
        // it to startup config and explicit session toggles.
        unsafe {
            if enabled {
                std::env::remove_var("PIR_AGENT_AS_INVOKER");
            } else {
                std::env::set_var("PIR_AGENT_AS_INVOKER", "1");
            }
        }
    }

    /// Set the local su-security authorization for this session. Returns the
    /// reason it was recorded at (for audit). `reason` is required when turning
    /// the boundary OFF, because disabling it lets the agent act with the
    /// invoking user's full authority for this session.
    pub fn set_su_security(&mut self, enabled: bool, reason: &str) -> String {
        // Wire the authority: while su-security is off, bash must NOT drop to
        // `ai_X` (drop_to_agent_user reads this env) so the agent can act as
        // the invoking user (root). The reason is recorded in the response.
        self.set_su_security_flag(enabled);
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

    /// Push menu-edited policy flags into the live session. The security editor
    /// saves to `security.toml` (new sessions); without this the running session
    /// keeps its startup snapshot, so the menu shows stale toggles the next time
    /// it opens. Disabling a quarantine also tears down its live overlays (an
    /// "off" that keeps staging writes would be a lie); enabling mounts takes
    /// effect for new sessions (mounts need startup context). A chroot
    /// container / full-root namespace cannot be unwound mid-session, so there
    /// disabling only stops future sessions. Returns a status line for display.
    pub fn apply_security_policy(&mut self, updated: &crate::security::SecurityPolicy) -> String {
        use crate::security::overlay;
        let mut notes = Vec::new();
        if let Some(ctx) = self.security.as_mut() {
            // Live in-process flag follows the saved policy.
            ctx.set_quarantine(updated.quarantine);
            // Stored snapshot follows too, so the next /menu open shows the
            // saved values. The context is only shared while a turn runs (the
            // menu is idle-only), so exclusive access should hold.
            match Arc::get_mut(ctx) {
                Some(c) => {
                    c.policy = updated.clone();
                }
                None => notes.push(
                    "policy snapshot busy — menu display refreshes next session".to_string(),
                ),
            }
        }
        crate::security::set_mitigation_active(
            updated.level.is_mitigation()
                || updated.level == crate::security::SecurityLevel::Guard,
        );
        let locked_in = overlay::container_engaged() || overlay::fullroot_engaged();
        if !updated.quarantine {
            if locked_in {
                notes.push(
                    "write-quarantine saved OFF but this session stays staged: a chroot/container namespace cannot be unwound mid-session — relaunch for it to take effect"
                        .to_string(),
                );
            } else if overlay::system_quarantine_engaged() {
                let staged = overlay::with_active(|q| q.staged().len()).unwrap_or(0);
                match overlay::teardown_active() {
                    Ok(()) => notes.push(if staged > 0 {
                        format!(
                            "write-quarantine live: OFF (unmounted; {staged} staged write(s) discarded)"
                        )
                    } else {
                        "write-quarantine live: OFF (unmounted)".to_string()
                    }),
                    Err(e) => notes.push(format!("write-quarantine teardown: {e}")),
                }
            } else {
                notes.push("write-quarantine live: OFF (was not mounted)".to_string());
            }
        } else if locked_in {
            notes.push("write-quarantine on (container/full-root already stages everything)".to_string());
        } else {
            notes.push("write-quarantine on (overlays mount for new sessions)".to_string());
        }
        if !updated.quarantine_project {
            if locked_in {
                notes.push(
                    "project-quarantine saved OFF but this session stays staged (container namespace — relaunch for it to take effect)"
                        .to_string(),
                );
            } else if overlay::project_quarantine_engaged() {
                let staged = overlay::project_active_staged_count();
                match overlay::project_active_teardown() {
                    Ok(()) => notes.push(if staged > 0 {
                        format!(
                            "project-quarantine live: OFF (unmounted; {staged} staged write(s) discarded)"
                        )
                    } else {
                        "project-quarantine live: OFF (unmounted)".to_string()
                    }),
                    Err(e) => notes.push(format!("project-quarantine teardown: {e}")),
                }
            } else {
                notes.push("project-quarantine live: OFF (was not mounted)".to_string());
            }
        } else if !locked_in {
            notes.push("project-quarantine on (overlay mounts for new sessions)".to_string());
        }
        notes.join("; ")
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

    /// Drop the persisted su-security sidecar. Called when the operator changes
    /// the *policy* (`security.user-security`) rather than the session: a
    /// sidecar recorded under the old policy is stale, and leaving it behind
    /// would quietly keep overriding the new setting on every resume.
    pub fn clear_persisted_su_security(&mut self) {
        if let Some(p) = &self.log_path {
            let _ = std::fs::remove_file(p.with_extension("susec"));
        }
    }

    /// Load a previously persisted su-security choice (from `<log>.susec`) for
    /// a resumed session. Returns `Some(notice)` when the persisted value
    /// CONTRADICTS the operator's current policy (so the caller can surface it),
    /// `None` when it agrees with the policy or there is nothing persisted.
    ///
    /// The sidecar exists so a per-session `/su-security off <reason>` survives
    /// a resume — the operator authorised the escalation for *that* work and
    /// shouldn't have to re-authorise on every restart. What it must NOT do is
    /// silently veto an explicit `security.user-security` setting: previously
    /// this method unconditionally clobbered `PIR_AGENT_AS_INVOKER`, so a
    /// session whose sidecar said "1" re-enabled confinement (agent commands
    /// dropped to `ai_X`) even after the operator turned user-security off —
    /// with no indication of why.
    ///
    /// Precedence now: the persisted per-session choice still wins (it is the
    /// more specific, operator-made decision), but a contradiction is reported
    /// loudly and offers the fix, so the setting never appears to be ignored.
    pub fn apply_persisted_su_security(&mut self) -> Option<String> {
        let p = self.log_path.clone()?;
        let Ok(s) = std::fs::read_to_string(p.with_extension("susec")) else {
            return None;
        };
        let persisted = s.trim() == "1";
        self.set_su_security_flag(persisted);
        // Report a contradiction with the current policy so the operator is
        // never left guessing why their `user-security` setting appears ignored.
        let policy_on = self
            .security_policy()
            .map(|p| p.user_security)
            .unwrap_or(true);
        su_security_override_notice(
            persisted,
            policy_on,
            &p.with_extension("susec").display().to_string(),
        )
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
        // pi `thinkingLevelMap` note: when the catalog hides this level for
        // the current model, say what the request will actually carry (the
        // mapped fallback) instead of implying the raw level applies.
        let mut hidden_note = String::new();
        if self.model.reasoning && !self.model.supported_levels().contains(&level) {
            let clamped = self.model.clamp_thinking(level);
            hidden_note = format!(
                "  (hidden for {}/{} by thinkingLevelMap — nearest offered level is '{}'; the request falls back to the mapped effort)",
                self.provider.pid(),
                self.model.id,
                clamped.as_str(),
            );
        } else if !self.model.reasoning && level.enabled() {
            hidden_note = format!(
                "  ({}/{} is not marked reasoning-capable — no thinking params will be sent)",
                self.provider.pid(),
                self.model.id,
            );
        }
        if level.enabled() {
            let budget = self
                .thinking
                .anthropic_budget(self.model.context.unwrap_or(200_000));
            // Mapped effort for display: the catalog value when present,
            // else the legacy default name.
            let mapped = self.model.mapped_effort(level);
            match self.provider.kind() {
                Some(ApiKind::Anthropic) => match budget {
                    Some(b) => format!(
                        "thinking: {}  (Anthropic budget ≈ {} tokens — may exceed the model's max unless it supports extended thinking){hidden_note}",
                        level.as_str(), b
                    ),
                    None => format!(
                        "thinking: {}  (model context too small for a meaningful thinking budget; will be ignored){hidden_note}",
                        level.as_str()
                    ),
                },
                Some(ApiKind::OpenAi) => match mapped.as_deref() {
                    Some(e) => format!(
                        "thinking: {}  (OpenAI reasoning_effort = {e}{}){hidden_note}",
                        level.as_str(),
                        match self.model.thinking_format_name() {
                            "openai" => String::new(),
                            f => format!(", thinkingFormat = {f}"),
                        }
                    ),
                    None => format!(
                        "thinking: {}  (no OpenAI reasoning_effort for this level; will be ignored){hidden_note}",
                        level.as_str()
                    ),
                },
                // Responses API takes the same effort names via `reasoning.effort`.
                Some(ApiKind::OpenAiResponses) => match mapped.as_deref() {
                    Some(e) => format!("thinking: {}  (Responses reasoning.effort = {e}){hidden_note}", level.as_str()),
                    None => format!(
                        "thinking: {}  (no Responses reasoning effort for this level; will be ignored){hidden_note}",
                        level.as_str()
                    ),
                },
                None => format!("thinking: {}{hidden_note}", level.as_str()),
            }
        } else {
            "thinking: off".to_string()
        }
    }

    /// Set the auto-retry policy for this session. `Some(n)` enables automatic
    /// re-runs of a turn the light model judges `retry` (compacting history
    /// first when it is near the context cap). `None` disables auto-retry
    /// (verdict is still computed for display). Returns a status line.
    pub fn set_auto_retry(&mut self, n: Option<usize>) -> String {
        self.auto_retry = n;
        match n {
            None => "auto-retry: off (verdict computed, but turns end at the REPL so you decide)".to_string(),
            Some(0) => "auto-retry: observe only (0 — verdict computed, nothing retried)".to_string(),
            Some(n) => format!(
                "auto-retry: on (up to {n} automatic re-run of a 'retry' turn; compacts history when near the context cap)"
            ),
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

    /// Whether incremental (in-place) markdown rendering is enabled.
    pub fn incremental_md(&self) -> bool {
        self.incremental_md
    }

    /// Toggle incremental (in-place) markdown rendering for this session. Off
    /// (`PIR_INCREMENTAL_MD=0` / `--no-incremental`) is useful for terminals or
    /// logs that mangle cursor-movement escapes (the reply is then drawn once
    /// at the end). Persisted per session. Returns a status line.
    pub fn set_incremental_md(&mut self, on: bool) -> String {
        self.incremental_md = on;
        self.persist_incremental_md();
        if on {
            "markdown: incremental  (re-rendered in place as it streams, throttled to 200ms)"
        } else {
            "markdown: simple  (rendered once when the turn completes)"
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

    /// Persist the incremental (in-place) markdown choice (to `<log>.incmd`) for
    /// this session, so a resumed session restores it. Mirrors `persist_su_security`.
    fn persist_incremental_md(&self) {
        if let Some(p) = &self.log_path {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let path = p.with_extension("incmd");
            let _ = std::fs::write(&path, if self.incremental_md { "1" } else { "0" });
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

    /// Load a previously persisted incremental-markdown choice (from
    /// `<log>.incmd`) for a resumed session. Mirrors `apply_persisted_thinking`.
    /// Returns true if a value was restored.
    pub fn apply_persisted_incremental_md(&mut self) -> bool {
        let Some(p) = self.log_path.as_ref() else { return false };
        match std::fs::read_to_string(p.with_extension("incmd")) {
            Ok(s) => {
                self.incremental_md = s.trim() == "1";
                true
            }
            Err(_) => false,
        }
    }

    /// Begin a new goal for this session, persisting it next to the log.
    /// Whether this session is in plan mode (read/search only).
    pub fn plan_mode(&self) -> bool {
        self.plan_mode
    }

    /// Enter/leave plan mode, enforcing the security level and persisting the
    /// choice next to the session log. Returns a status line for the REPL.
    ///
    /// Entering swaps the *live* security policy to `SecurityLevel::Plan` and
    /// remembers nothing about the previous level; leaving restores
    /// `Mitigation` (the default posture) — deliberately conservative, because
    /// "leave plan mode" should never be a way to end up at a *weaker* level
    /// than the operator last configured. The operator can always re-select an
    /// explicit level with `/secure`.
    pub fn set_plan_mode(&mut self, on: bool) -> String {
        if self.plan_mode == on {
            return if on {
                "plan mode is already on (read-only; use plan_write to record the approach)".to_string()
            } else {
                "plan mode is already off".to_string()
            };
        }
        self.plan_mode = on;
        let mut policy = self.security_policy().unwrap_or_default();
        policy.level = if on {
            crate::security::SecurityLevel::Plan
        } else {
            crate::security::SecurityLevel::Mitigation
        };
        let notes = self.apply_security_policy(&policy);
        self.persist_plan_mode();
        self.refresh_system();
        if on {
            format!(
                "plan mode ON — read-only. Explore with read_file/glob/grep/list_dir, then record \
                 the approach with the plan_write tool for approval. `bash`, write_file and \
                 edit_file are refused until the plan is approved or /plan off. ({notes})"
            )
        } else {
            format!("plan mode OFF — writes and commands are allowed again. ({notes})")
        }
    }

    /// Persist the plan-mode flag next to the session log (`<log>.plan`).
    /// Best-effort; a one-shot session with no log is silently skipped.
    fn persist_plan_mode(&self) {
        if let Some(p) = &self.log_path {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(p.with_extension("plan"), if self.plan_mode { "1" } else { "0" });
        }
    }

    /// Restore a persisted plan-mode choice for a resumed session. Returns the
    /// notice to show when it restored `on` (so the operator is told the
    /// session came back read-only rather than being surprised by a refusal
    /// later).
    pub fn apply_persisted_plan_mode(&mut self) -> Option<String> {
        let p = self.log_path.clone()?;
        let s = std::fs::read_to_string(p.with_extension("plan")).ok()?;
        if s.trim() == "1" {
            self.plan_mode = false; // force `set_plan_mode` to do the work
            Some(self.set_plan_mode(true))
        } else {
            None
        }
    }

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

    /// Intercept the `agent` tool (Feature 6): run a sub-agent and return its
    /// report.
    ///
    /// Runs inline (the parent's turn blocks) — that is deliberate: delegation
    /// exists to keep the parent's *context* small, not to add concurrency, and
    /// a foreground child's report is immediately usable. `run_in_background` is
    /// accepted and reported honestly as unsupported-in-this-build rather than
    /// silently running in the foreground (a caller that expected a job handle
    /// would otherwise mis-read the result).
    fn run_subagent_tool(&mut self, name: &str, input: &Value) -> Option<Outcome> {
        if name != "agent" {
            return None;
        }
        // No nested delegation: a sub-agent's tool list already omits `agent`,
        // this is belt-and-braces for a stale prompt.
        if let Some(k) = self.subagent_kind {
            return Some(Outcome {
                content: format!(
                    "agent: sub-agents cannot delegate further (this is a {} sub-agent). Do the \
                     work directly.",
                    k.as_str()
                ),
                is_error: true,
            });
        }
        let spawn = match crate::subagent::parse(input) {
            Ok(s) => s,
            Err(e) => return Some(Outcome { content: e, is_error: true }),
        };
        let policy = self.security_policy();
        let report = crate::subagent::run(
            self.provider.clone(),
            self.model.clone(),
            &spawn,
            self.registry.full_auto(),
            policy,
            self.cancel.clone(),
        );
        let mut text = report.render(&spawn);
        if spawn.background {
            text.push_str(
                "\n\nNote: `run_in_background` is not supported for sub-agents in this build — \
                 it ran in the foreground and its report is above.",
            );
        }
        Some(Outcome { content: text, is_error: report.error })
    }

    /// Intercept the `remember` tool (Feature 12): persist a durable preference.
    ///
    /// Writes to the *project* scope by default (a preference learned while
    /// working in a repo usually belongs to that repo), or the user scope when
    /// `scope: "user"` is passed. Re-recording reinforces rather than duplicates,
    /// so the model can safely restate something it isn't sure was stored.
    fn run_taste_tool(&mut self, name: &str, input: &Value) -> Option<Outcome> {
        if name != "remember" {
            return None;
        }
        let text = input
            .get("text")
            .or_else(|| input.get("preference"))
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let Some(text) = text else {
            return Some(Outcome {
                content: "remember: 'text' is required (the preference, as one imperative line, \
                          e.g. \"Never commit without asking\")"
                    .into(),
                is_error: true,
            });
        };
        let scope = match input.get("scope").and_then(Value::as_str).unwrap_or("project").trim().to_ascii_lowercase().as_str() {
            "user" | "global" | "config" | "~" => crate::taste::Scope::Global,
            "project" | "repo" | "local" | "" => crate::taste::Scope::Project,
            other => {
                return Some(Outcome {
                    content: format!("remember: unknown scope '{other}' (want 'project' or 'user')"),
                    is_error: true,
                });
            }
        };
        let evidence = input.get("evidence").and_then(Value::as_str).unwrap_or("");
        let confidence = input
            .get("confidence")
            .and_then(Value::as_f64)
            .map(|c| c as f32);
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut store = crate::taste::Store::load(scope, &cwd);
        let (key, created) = store.remember(&text, evidence, confidence);
        let stored = store.entries.iter().find(|e| crate::taste::slug(&e.text) == key).cloned();
        match store.save(scope, &cwd) {
            Ok(()) => {
                self.refresh_system();
                let conf = stored.map(|e| e.confidence).unwrap_or(0.6);
                Some(Outcome {
                    content: format!(
                        "{} '{}' to the {} taste store (confidence {:.1}). It is injected into \
                         your system prompt from now on — including in later sessions.",
                        if created { "Recorded" } else { "Reinforced" },
                        text,
                        scope.label(),
                        conf
                    ),
                    is_error: false,
                })
            }
            // Keep the in-memory effect (refresh_system already picked it up via
            // a fresh load) but tell the truth about durability.
            Err(e) => Some(Outcome {
                content: format!(
                    "Recorded '{text}' for this session, but it could not be persisted: {e}"
                ),
                is_error: true,
            }),
        }
    }

    /// Intercept the `plan_write` tool (Feature 5): persist the plan document
    /// and, when the operator is present, ask them to approve it.
    ///
    /// Approval is the whole point of plan mode, so this is where the loop
    /// closes: an approved plan leaves plan mode and the content is handed back
    /// for the model to implement; a rejected one stays in plan mode with the
    /// operator's feedback as the tool result, so the model revises rather than
    /// proceeding.
    ///
    /// Writes the document *before* asking: even a rejected plan is worth
    /// keeping (it is the record of what was proposed and why), and the
    /// operator can read it in an editor while deciding.
    fn run_plan_tool(&mut self, name: &str, input: &Value) -> Option<Outcome> {
        if name != "plan_write" {
            return None;
        }
        let content = input.get("content").and_then(Value::as_str).unwrap_or("").to_string();
        if content.trim().is_empty() {
            return Some(Outcome {
                content: "plan_write: 'content' is required (the plan document). Write the plan in \
                          markdown: the goal, the approach, the files to touch, and how it will be \
                          verified."
                    .into(),
                is_error: true,
            });
        }
        let name_in = input.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let plan_name = if name_in.is_empty() {
            crate::plans::default_name(&self.last_prompt)
        } else {
            name_in
        };
        let path = match crate::plans::write(&plan_name, &content) {
            Ok(p) => p,
            Err(e) => return Some(Outcome { content: e, is_error: true }),
        };
        // Headless / no operator: the plan is written but cannot be approved, so
        // report that plainly and leave plan mode ON — silently exiting would
        // hand the model write access it never got permission for.
        if !crate::askuser::interactive() {
            return Some(Outcome {
                content: format!(
                    "Plan written to {}.\nNo interactive operator is attached, so it cannot be \
                     approved automatically — plan mode stays ON (read-only). End the turn with \
                     request_stop(reason=stop-needs-input) so the operator can review it.",
                    path.display()
                ),
                is_error: false,
            });
        }
        let q = crate::askuser::Question {
            header: "Plan review".to_string(),
            question: format!(
                "Approve this plan and start implementing?\n\nSaved to: {}\n\n{}",
                path.display(),
                clip_for_dialog(&content, 1200)
            ),
            options: vec![
                crate::askuser::Choice {
                    label: "Approve — implement it".to_string(),
                    description: "Leaves plan mode and lets the agent make the changes".to_string(),
                },
                crate::askuser::Choice {
                    label: "Revise — keep planning".to_string(),
                    description: "Stay read-only; the agent will refine the plan".to_string(),
                },
            ],
            multi_select: false,
        };
        let answer = crate::askuser::ask(&q).unwrap_or(crate::askuser::Answer::Dismissed);
        let approved = matches!(&answer, crate::askuser::Answer::Chosen(v) if v.first().map(|s| s.starts_with("Approve")).unwrap_or(false));
        if approved {
            let notice = self.set_plan_mode(false);
            Some(Outcome {
                content: format!(
                    "The operator APPROVED the plan ({}). {notice}\nImplement it now, following the \
                     plan you just wrote; update the plan file if you must deviate.",
                    path.display()
                ),
                is_error: false,
            })
        } else {
            let feedback = match &answer {
                crate::askuser::Answer::Text(t) => format!("\nOperator feedback: {t}"),
                crate::askuser::Answer::Chosen(v) if !v.is_empty() => {
                    format!("\nOperator chose: {}", v.join(", "))
                }
                _ => "\nThe operator dismissed the dialog without answering.".to_string(),
            };
            Some(Outcome {
                content: format!(
                    "The plan was NOT approved; you are still in plan mode (read-only). Saved to \
                     {}.{feedback}\nRevise the plan and call plan_write again.",
                    path.display()
                ),
                is_error: false,
            })
        }
    }

    /// Intercept the `load_skill` tool (Feature 4): return the full body of the
    /// named skill (or the catalog when called with no name).
    ///
    /// Skills are re-discovered on every call rather than cached at startup, so
    /// one the model or the operator just wrote is immediately loadable — the
    /// discovery is a handful of directory reads, far cheaper than the
    /// instructions it returns.
    fn run_skill_tool(&mut self, name: &str, input: &Value) -> Option<Outcome> {
        if name != "load_skill" {
            return None;
        }
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let skills = crate::skills::discover(&cwd);
        let query = input
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        // No name (or an explicit listing request) => the catalog, so the model
        // can see everything available in one call.
        if query.is_empty() || query.eq_ignore_ascii_case("all") || query == "*" {
            return Some(Outcome {
                content: crate::skills::catalog(&skills),
                is_error: false,
            });
        }
        let skill = match crate::skills::find(&skills, &query) {
            Ok(s) => s.clone(),
            Err(e) => return Some(Outcome { content: e, is_error: true }),
        };
        match crate::skills::body(&skill) {
            Ok(text) => Some(Outcome {
                content: format!(
                    "# Skill: {} ({})\nsource: {}\n\n{text}",
                    skill.name,
                    skill.source.label(),
                    skill.path.display()
                ),
                is_error: false,
            }),
            Err(e) => Some(Outcome { content: e, is_error: true }),
        }
    }

    /// Intercept the `ask_user_question` tool (Feature 3): pause the turn and
    /// ask the operator, returning their answer as the tool result.
    ///
    /// This lives on the Agent rather than in the builtin extension because the
    /// question must be rendered by the REPL (which owns stdin) — see
    /// [`crate::askuser`]. When no interactive operator is attached (headless
    /// one-shot, `-bg`, tests) it returns an explicit "no interactive user"
    /// result instead of blocking: an unattended run must never wedge.
    fn run_ask_tool(&mut self, name: &str, input: &Value) -> Option<Outcome> {
        if name != "ask_user_question" {
            return None;
        }
        let questions = match crate::askuser::parse_questions(input) {
            Ok(q) => q,
            Err(e) => {
                return Some(Outcome {
                    content: format!("ask_user_question: {e}"),
                    is_error: true,
                });
            }
        };
        if !crate::askuser::interactive() {
            return Some(Outcome {
                content: "No interactive user is available to answer (headless/background session). \
                          Do not ask again — make the most reasonable assumption, state it \
                          explicitly in your reply, and continue. If the decision is genuinely \
                          blocking, end the turn with request_stop(reason=stop-needs-input)."
                    .into(),
                is_error: true,
            });
        }
        let mut parts: Vec<String> = Vec::new();
        for (i, q) in questions.iter().enumerate() {
            let ans = match crate::askuser::ask(q) {
                Some(a) => a,
                None => crate::askuser::Answer::Dismissed,
            };
            let label = if questions.len() > 1 {
                format!("{}. [{}] {}", i + 1, q.header, q.question)
            } else {
                format!("[{}] {}", q.header, q.question)
            };
            parts.push(format!("{label}\n   → {}", ans.render()));
        }
        Some(Outcome {
            content: format!("The user answered:\n{}", parts.join("\n")),
            is_error: false,
        })
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
                if let Some(o) = input.get("objective").and_then(Value::as_str)
                    && !o.trim().is_empty() {
                        store.goal.objective = o.trim().to_string();
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
        for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
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
            } else if role == "assistant" {
                // Assistant message: if we already have a pending user turn,
                // pair them; otherwise just queue the assistant alone. Remember
                // its text as the latest assistant output (shown as the tail).
                // Any other role (`notice` transcript entries, or anything a
                // future writer adds) is skipped: transcript-only lines must
                // never be replayed into model context.
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
            if let Some(limit) = max_steps
                && steps >= limit {
                    out.push_str(&format!(
                        "step cap ({limit}) reached — {steps} step(s) run, {} in / {} out tokens used this run\n",
                        self.usage.input.saturating_sub(start_in),
                        self.usage.output.saturating_sub(start_out),
                    ));
                    break;
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

            // Light-model completion backstop: the big model sometimes ends a
            // turn with a summary but forgets to mark the goal complete (or
            // keeps answering without tool calls). Before running another step,
            // ask the cheap model whether the objective is actually met. If it
            // says complete, mark the goal done and stop; if it says incomplete,
            // keep driving. A `None` (light model unavailable) means we trust
            // the big model's own `update_goal` status and just continue.
            let maybe_done = self.evaluate_goal_complete();
            if maybe_done == Some(true) {
                if let Some(s) = self.goal_store.as_mut() {
                    s.goal.status = crate::goal::GoalStatus::Complete;
                    s.save();
                }
                self.refresh_system();
                out.push_str("light model confirms the goal is complete — marking done and stopping\n");
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
                // Model produced no tool calls and nothing progressed. Ask the
                // light model whether the goal is nonetheless complete; if so,
                // mark it done and stop. Otherwise the goal is genuinely stuck
                // (e.g. the model is waiting or looping), so we stop rather than
                // spinning forever.
                match self.evaluate_goal_complete() {
                    Some(true) => {
                        if let Some(s) = self.goal_store.as_mut() {
                            s.goal.status = crate::goal::GoalStatus::Complete;
                            s.save();
                        }
                        self.refresh_system();
                        out.push_str("light model confirms the goal is complete — marking done\n");
                    }
                    Some(false) => {
                        out.push_str("model yielded without progress and goal not complete; stopping\n");
                    }
                    None => {
                        out.push_str("model yielded without further progress; stopping\n");
                    }
                }
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

    /// Ask the cheap light model whether the active goal is complete yet, as a
    /// backstop for `drive_goal`. Returns `Some(true)` when the light model
    /// judges the objective met, `Some(false)` when not, and `None` when the
    /// light model is unavailable or there's no transcript to judge. The result
    /// is never authoritative on its own — `drive_goal` still trusts the big
    /// model's `update_goal` status when the light model can't be reached.
    fn evaluate_goal_complete(&self) -> Option<bool> {
        let store = self.goal_store.as_ref()?;
        let summary = store.goal.summary();
        let log = store.path().clone();
        crate::titler::goal_complete_now(&log, &summary)
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
        // Record the prompt in the shared approval context so a mid-turn
        // tool-approval dialog can show *why* the agent is asking.
        if let Some(sec) = &self.security {
            sec.approval.note_prompt(user);
        }
        let msg = Message::user(user);
        log_line(&mut self.log, &msg);
        self.history.push(msg);
        // Record that a turn is now in flight (so a crash/network failure mid-turn
        // leaves a discoverable "unfinished" session owned by this live process).
        self.mark_status(SessionStatus::Active, self.goal_pending(), "");
        // Fresh turn: reset the runaway-loop detector so a loop in a *previous*
        // turn can't carry over into this one, and stamp the turn clock so the
        // TurnDone notification reports the real wall time (not 0.0s).
        self.loop_detector = LoopDetector::new();
        self.stop_hit = None;
        self.stop_nudges = 0;
        self.turn_started = Some(std::time::Instant::now());
        // Open a rewind checkpoint (Feature 8) at the turn boundary. Recorded
        // *before* the turn can touch anything, and keyed to the history length
        // at this moment so a rewind can also truncate the conversation back to
        // a state consistent with the restored files.
        self.open_checkpoint(user);
        // When the skill is on, `request_stop` is a first-class tool the model
        // must see in every request; when off it is hidden (and refused if the
        // model calls it anyway, e.g. from a stale prompt).
        let mut specs = self.registry.specs();
        if !self.stop_skill {
            specs.retain(|s| s.name != "request_stop");
        }
        // Sub-agents (Feature 6) never see the tools that need a second operator
        // channel or that only make sense for the top-level session:
        //  * `ask_user_question` — there is no REPL channel from a child;
        //  * `request_stop` — the child ends when its turn ends; an explicit stop
        //    means nothing to the parent;
        //  * `agent` — no recursive delegation (depth 1), which also removes the
        //    only way to spawn an unbounded tree of children.
        if self.subagent_kind.is_some() {
            specs.retain(|s| !matches!(s.name, "ask_user_question" | "request_stop" | "agent"));
        }
        let tty = crate::term::is_terminal();

        // NDJSON: emit turn_start event
        self.emit_json(&json!({
            "type": "turn_start",
            "prompt": user,
            "model": format!("{}/{}", self.provider.pid(), self.model.id),
        }));
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
        // per model call, the moment the first TEXT token arrives. Reasoning
        // tokens do NOT stop it: the footer (with the ❯ prompt) stays pinned
        // while thinking streams above, so the prompt is visible for the whole
        // turn. Shared by both stream callbacks so the spinner's 80ms
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
            // The spinner shares the agent's `quiet_req` switch, so when the
            // REPL detaches this turn to the background (sets `quiet_req`) the
            // spinner also goes silent instead of keeping its "thinking" line on
            // the now-backgrounded terminal.
            if !self.silent() {
                *spinner.borrow_mut() = Some(term::Spinner::start_with(
                    "thinking",
                    self.typeahead.clone(),
                    tty,
                    self.quiet_req.clone(),
                ));
            }
            // Stop the footer spinner (if running) the moment the model emits
            // its first token, so the agent's text starts on a clean line.
            // `stopped_here` tracks whether *this* call has already cleared it,
            // so subsequent tokens in the same stream don't touch it again.
            //
            // We accumulate the reply into `assistant_text` (rather than
            // printing each token live) so the whole message can be rendered as
            // Markdown. When incremental (in-place) rendering is enabled *and*
            // we're on a tty, every streamed token is also pushed to an
            // `IncrementalMarkdown` renderer that re-draws the partial markdown
            // *in place* as it grows — jumping the cursor back over its previous
            // block and overwriting it — so the user watches the formatted reply
            // appear live instead of a blank spinner. Redraws are throttled to
            // 200ms by the renderer, so a fast token firehose can't saturate the
            // terminal. When incremental is off (quiet, not a tty, or opted out
            // via PIR_INCREMENTAL_MD=0 / --no-incremental) we fall back to a
            // single render once the message is complete (see the flush block
            // below). The renderer holds the full accumulated markdown, so the
            // single-render fallback never needs `assistant_text`.
            let use_incremental = !self.silent() && tty && self.incremental_md;
            let mut inc: Option<crate::md::IncrementalMarkdown> = if use_incremental {
                Some(crate::md::IncrementalMarkdown::new(true, crate::term::color_enabled()))
            } else {
                None
            };
            let mut assistant_text = String::new();
            let mut on_text = |t: &str| {
                if !self.silent() {
                    stop_spinner();
                    assistant_text.push_str(t);
                    if let Some(inc) = inc.as_mut() {
                        inc.push(t);
                    }
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
            // Coalesce micro-deltas before touching the terminal: providers
            // stream thinking in 1-2 token fragments ("o", "trans", "p",
            // "iler", ...) and the old code did one `term::out` + flush per
            // fragment. Each write contends with the spinner's 80ms footer
            // redraw (which re-parks the cursor), so consecutive fragments
            // landed on separate scrolled lines and the turn crawled under
            // syscall/escape-sequence overhead. Buffer instead and flush when
            // there is a newline to show, the buffer is sizable, or the
            // throttle window has elapsed — mirroring the throttled
            // IncrementalMarkdown path used for reply text.
            let mut last_think_flush = std::time::Instant::now();
            let mut on_think = |t: &str| {
                // Record thinking in the shared approval context so a tool-
                // approval dialog can show the agent's recent reasoning.
                if let Some(sec) = &self.security {
                    sec.approval.note_thinking(t);
                }
                // NOTE: no stop_spinner() here — the footer (❯ prompt) stays
                // alive for the whole thinking stream (see above). Thinking
                // output itself is still deferred while the user types (below)
                // so it never wipes the in-progress draft line.
                if !self.silent() && show_thinking {
                    think_buf.push_str(t);
                    if term::raw::keyboard_idle_long_enough() {
                        // Compacted for display (blank runs joined); the log
                        // keeps exact bytes.
                        let due = last_think_flush.elapsed() >= std::time::Duration::from_millis(200);
                        if think_buf.contains('\n') || think_buf.len() >= 512 || due {
                            let show = term::compact_thinking(&std::mem::take(&mut think_buf));
                            term::out(&term::dim(&show).to_string());
                            last_think_flush = std::time::Instant::now();
                        }
                    }
                }
            };
            // The provider enforces a *combined* context limit: text input +
            // tool input + requested output must fit in `model.context`. To
            // avoid a hard HTTP 400 ("maximum context length is N tokens"),
            // clamp the output budget so the total request provably fits,
            // leaving a little headroom for the system prompt (which is sent
            // alongside `history` but not counted in it). This is the belt to
            // `trim`'s suspenders: `trim` drops old turns from `history`, and
            // this guarantees the remaining room is never over-committed on the
            // output side. `approx_tokens` is a cheap `bytes/4` estimate, so a
            // generous (2k) system headroom guards against tokenizer drift.
            let ctx = self.model.context.unwrap_or(200_000);
            let sys_head = 2_000u64;
            let est_input = approx_tokens(&self.history) as u64;
            let out_cap = ctx
                .saturating_sub(est_input)
                .saturating_sub(sys_head)
                .max(1024);
            let max_tokens = self.model.max_tokens.unwrap_or(8192).min(out_cap);
            // Live retry countdown. The provider calls this ~1/sec while
            // backing off between attempts (plus once with `remaining == 0`
            // when the wait ends). Rendered in place on the current line —
            // never appended to `assistant_text`, so the transcript keeps
            // pure model text while the terminal shows the ticking countdown.
            // Skipped when quiet/detached/non-tty (the wait still happens;
            // only the display is gated).
            let mut on_retry = |w: &crate::provider::RetryWait| {
                if self.silent() || !tty {
                    return;
                }
                if w.remaining.is_zero() {
                    term::out("\r\x1b[K");
                } else {
                    term::out(&format!(
                        "\r\x1b[K{}",
                        term::dim(&format!(
                            "\u{23f3} attempt {} failed — retrying in {}s… (Ctrl-C to stop)",
                            w.attempt,
                            w.remaining.as_secs()
                        ))
                    ));
                }
            };
            // Retry/failure notices (attempt failed, reconnected). Shown
            // immediately — bypassing the markdown renderer and
            // `assistant_text`, so model text, loop detection, and the final
            // render stay pure — and buffered for the session log, where a
            // later debug can reconstruct how many attempts a turn took (the
            // 254s turn taught us screen-only notices are unrecoverable).
            // Skipped when quiet/detached like all output.
            let notices: RefCell<Vec<String>> = RefCell::new(Vec::new());
            let mut on_notice = |t: &str| {
                if !self.silent() {
                    term::out(t);
                }
                notices.borrow_mut().push(t.to_string());
            };
            let result = self.client.chat(
                &self.model.id,
                max_tokens,
                &self.system,
                &self.history,
                &specs,
                &mut on_text,
                self.thinking,
                self.model.context.unwrap_or(200_000),
                &mut on_think,
                // Per-model overrides (OpenCode Zen's per-model API/URL).
                self.provider.model_api(&self.model),
                self.provider.model_base_url(&self.model),
                !self.model.no_reasoning_effort,
                // Catalog thinking metadata (`reasoning`,
                // `compat.thinkingFormat`, `thinkingLevelMap`) so the request
                // sends pi-shaped thinking controls (e.g. deepseek's
                // `thinking: {type: …}` toggle + mapped effort).
                Some(&self.model),
                &mut on_retry,
                &mut on_notice,
            );
            // Persist any retry notices as transcript-only log entries (never
            // replayed to the model — see `log_notice`), so the session file
            // records the attempt history the provider just reported.
            for n in notices.borrow().iter() {
                log_notice(&mut self.log, n);
            }
            // Flush any thinking that arrived while the user was still typing
            // (deferred above) BEFORE the reply text / tool output prints, so
            // reasoning never appears interleaved after the response it
            // preceded — and the buffer can't leak into the next model call.
            // Gated on `silent()` so a turn detached to the background (where
            // `quiet_req` was set mid-stream) doesn't dump its leftover
            // thinking onto the now-backgrounded terminal.
            if !self.silent() && !think_buf.is_empty() {
                let show = term::compact_thinking(&std::mem::take(&mut think_buf));
                term::out(&term::dim(&show).to_string());
            }
            // Ensure the footer spinner is stopped (covers the no-output case),
            // then move to a fresh line below the agent's text.
            if !self.silent() {
                if let Some(mut s) = spinner.borrow_mut().take() {
                    s.stop();
                }
                // When incremental (in-place) rendering is active, do NOT print
                // a blank line before the final flush: the flush jumps the cursor
                // back `last_height` rows to overwrite the block it drew last,
                // and an intervening newline would shift the cursor down one row
                // so the jump-back undershoots and leaves the top row of the old
                // block (e.g. a `# heading`) duplicated above the fresh render.
                // The blank-line-only (non-incremental) path prints it below the
                // single final render instead.
                if !use_incremental {
                    term::out("\n");
                }
            }
            // Render the assistant's reply as Markdown. When incremental (in-
            // place) rendering is active, the reply has *already* been drawn as
            // partial blocks that grow in place during streaming — here we just
            // flush the final, complete render over the last partial block (the
            // renderer jumps the cursor back up and overwrites it, so nothing is
            // stacked or duplicated). When it's off, we draw the markdown a
            // single time now that the whole message is in hand. Either way the
            // result is formatted Markdown (headings, **bold**, lists, code
            // fences) rather than raw `**`/`#`/` ``` `. Colour is applied only
            // when the terminal supports it.
            if !self.silent() && !assistant_text.trim().is_empty() {
                if let Some(mut inc) = inc.take() {
                    inc.flush();
                } else {
                    term::out(&crate::md::render(&assistant_text, crate::term::color_enabled()));
                }
            }
            let (assistant, usage) = match result {
                Ok(r) => r,
                Err(e) => {
                    // Surface the failure visibly in the main stream (red banner)
                    // as well as stderr, so a mid-turn provider error isn't lost
                    // below already-printed tokens. The on-screen notification
                    // feed also gets an Error event.
                    if !think_buf.is_empty() {
                        let show = term::compact_thinking(&std::mem::take(&mut think_buf));
                        term::out(&term::dim(&show).to_string());
                    }
                    if !self.silent() {
                        term::out(&format!("\r\x1b[K{}\n", term::red(&format!("✗ turn error: {e}"))));
                        // A misrouted request never reached the model API (wrong
                        // baseUrl / dead proxy). Don't auto-retry — hand the user
                        // back to the REPL and point them at a provider switch.
                        if e.contains("misrouted") {
                            term::out(&term::yellow(
                                "  · the request didn't reach the model API — try a different provider (/model <provider>/<model>) or fix the provider baseUrl, then resend",
                            ));
                        }
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
                    // NDJSON: emit turn_end on error
                    self.emit_json(&json!({
                        "type": "turn_end",
                        "error": e,
                        "tokens_in": self.usage.input,
                        "tokens_out": self.usage.output,
                    }));
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

            // Runaway-loop detection: if the model re-issues the *identical*
            // tool-call batch, or repeats the *same* text output while NOT
            // making progress via distinct tool calls, several times in a row,
            // it's stuck (e.g. endlessly re-reading the same file, re-searching
            // for the same thing, or repeating "Let me look at main.go"). Stop
            // the turn with a banner instead of burning tokens forever. The
            // detector only fires on consecutive identical signals, and the
            // text signal is gated on the model being stuck on tools too, so
            // normal interleaved work (and a repeated preamble while reading
            // different files) is never affected.
            let looped = self.loop_detector.observe(
                &tool_batch_signature(&calls),
                &text_signature(&assistant_text),
            );
            if looped {
                if !self.silent() {
                    if let Some(mut s) = spinner.borrow_mut().take() {
                        s.stop();
                    }
                    term::out(&format!(
                        "\r\x1b[K{}\n",
                        term::yellow(
                            "✗ loop detected: the model repeated the same tool call(s) or text 3× in a row — stopping turn"
                        )
                    ));
                }
                self.mark_status(
                    SessionStatus::Interrupted,
                    self.goal_pending(),
                    "loop detected (repeated identical tool calls or text)",
                );
                self.notify.publish(self.turn_done_event(), false);
                if !self.silent() {
                    self.continuations.extend(self.registry.on_turn_end(user));
                }
                return Ok(());
            }

            // Only record the assistant message (and its tool calls) in history
            // once we know the turn is proceeding. If the loop detector fired
            // above we return early, so the assistant's tool_calls must NOT be
            // left in history without matching tool results — that dangling
            // tool_call makes the next provider request fail with HTTP 400
            // ("tool_calls must be followed by tool messages" / "tool must be a
            // response to a preceding tool_calls").
            log_line(&mut self.log, &assistant);
            self.history.push(assistant);

            if calls.is_empty() {
                // Stop skill on: a text-only answer is not a stop — auto-nudge
                // (briefly reminding of `request_stop`) until the model asks
                // for a stop explicitly or the nudge cap is hit. Off (legacy):
                // stop on the first text answer, as before.
                if self.stop_skill {
                    if self.stop_nudges >= STOP_NUDGE_MAX {
                        if !self.silent() {
                            term::out(&term::dim(&format!(
                                "· stop-skill: no request_stop after {} nudge(s) — ending turn",
                                self.stop_nudges
                            )));
                        }
                        self.registry.emit(EventKind::AgentEnd, &json!({}));
                        self.notify.publish(self.turn_done_event(), false);
                        if !self.silent() {
                            self.registry.emit(EventKind::TurnEnd, &json!({ "prompt": user }));
                            self.continuations.extend(self.registry.on_turn_end(user));
                        }
                        self.mark_status(SessionStatus::Completed, self.goal_pending(), "");
                        return Ok(());
                    }
                    self.stop_nudges += 1;
                    let nudge = Message::user(
                        "Your last message had no tool calls. If the work is really done, end \
                         your turn with an explicit request_stop call (reason stop-complete, or \
                         stop-needs-input with your question in message). If work remains, ignore \
                         request_stop and keep using tools instead — do NOT stop an unfinished \
                         task. Continue now.",
                    );
                    log_line(&mut self.log, &nudge);
                    self.history.push(nudge);
                    continue;
                }
                self.registry.emit(EventKind::AgentEnd, &json!({}));
                self.notify.publish(self.turn_done_event(), false);
                // NDJSON: emit turn_end on success
                self.emit_json(&json!({
                    "type": "turn_end",
                    "tokens_in": self.usage.input,
                    "tokens_out": self.usage.output,
                }));
                if !self.silent() {
                    self.registry.emit(EventKind::TurnEnd, &json!({ "prompt": user }));
                    self.continuations.extend(self.registry.on_turn_end(user));
                }
                self.mark_status(SessionStatus::Completed, self.goal_pending(), "");
                return Ok(());
            }

            let mut results = Message { role: Role::User, blocks: Vec::new() };
            // Tool execution can take a while (sleep, builds, test suites):
            // keep the footer zone (with the ❯ prompt) alive across it, so the
            // prompt stays visible for the whole turn — not just while waiting
            // for tokens. The next model call's spinner replaces this one.
            if !self.silent() {
                *spinner.borrow_mut() = Some(term::Spinner::start_with(
                    "running",
                    self.typeahead.clone(),
                    tty,
                    self.quiet_req.clone(),
                ));
            }
            for (id, name, input) in &calls {
                if !self.silent() {
                    // Trailing `\n` so back-to-back tool calls (and their
                    // results) each start on their own line instead of running
                    // together.
                    term::out(&format!("{} {}\n", term::cyan("»"), describe_call(name, input)));
                }
                // NDJSON: emit tool_call event
                self.emit_json(&json!({
                    "type": "tool_call",
                    "tool": name,
                    "args": input,
                }));
                // Pre-flight extension hook: any backend may block this tool
                // call (permission gates, protected paths, etc.). When blocked,
                // we feed the reason back as the tool result so the model sees
                // *why* and can adapt, and stop asking for more tools this turn
                // if the hook requested `terminate`.
                if let Some((reason, terminate)) = self.registry.preflight_tool(name, input) {
                    if !self.silent() {
                        term::out(&term::yellow(&format!("  · blocked: {reason}\n")));
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
                // Pre-flight security guardrail (the `security` module): reads
                // default-open, writes scoped to this project, escalation ask-
                // only. A `Deny` here means the operator (or the policy) refused;
                // the model is told why so it can `pir ask` or adapt.
                if let Some((reason, terminate)) = self.security_preflight(name, input) {
                    if !self.silent() {
                        term::out(&term::yellow(&format!("  · blocked: {reason}\n")));
                    }
                    results.blocks.push(Block::ToolResult {
                        tool_use_id: id.clone(),
                        content: format!("blocked by security: {reason}"),
                        is_error: true,
                    });
                    if terminate {
                        break;
                    }
                    continue;
                }
                // Snapshot the target file before a destructive edit so `/undo`
                // can revert it. `write_file`/`edit_file` take `path`.
                if (name == "write_file" || name == "edit_file")
                    && let Some(p) = input.get("path").and_then(Value::as_str) {
                        self.checkpoint_file(Path::new(p));
                        // Also capture it into this turn's checkpoint (Feature 8),
                        // so the whole turn can be rewound as a unit. Captured
                        // once per file per turn, before the first modification —
                        // see `capture`.
                        self.checkpoint_capture(Path::new(p));
                    }
                // A turn that ran a shell command has side effects a rewind
                // cannot undo; record that so the rewind result says so.
                if name == "bash" {
                    self.checkpoint_note_bash();
                }
                let outcome = match self.run_goal_tool(name, input) {
                    Some(o) => o,
                    None => match self.run_subagent_tool(name, input) {
                        Some(o) => o,
                        None => match self.run_taste_tool(name, input) {
                            Some(o) => o,
                            None => match self.run_plan_tool(name, input) {
                                Some(o) => o,
                                None => match self.run_skill_tool(name, input) {
                                    Some(o) => o,
                                    None => match self.run_ask_tool(name, input) {
                                        Some(o) => o,
                                        None => match self.run_stop_tool(name, input) {
                                            Some(o) => o,
                                            None => self.registry.execute(name, input),
                                        },
                                    },
                                },
                            },
                        },
                    },
                };
                // NDJSON: emit tool_result event
                self.emit_json(&json!({
                    "type": "tool_result",
                    "tool": name,
                    "is_error": outcome.is_error,
                    "content_len": outcome.content.len(),
                }));
                if !self.silent() {
                    // Echo the result for the human: multi-line, but abridged so
                    // a huge build log can't bury the prompt. The model and the
                    // session log still receive the full `outcome.content`.
                    let echoed = abridge_output(&outcome.content);
                    let indented = echoed
                        .lines()
                        .map(|l| format!("  {l}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    term::out(&term::dim(&format!("{indented}\n")));
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
                *spinner.borrow_mut() = Some(term::Spinner::start_with(
                    "thinking",
                    self.typeahead.clone(),
                    tty,
                    self.quiet_req.clone(),
                ));
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

            // Explicit stop: the model called `request_stop` this batch. End
            // the turn now, with Completed for stop-complete or Interrupted
            // (carrying the model's question) for stop-needs-input.
            if let Some((decision, why)) = self.stop_hit.take() {
                let (status, reason) = match decision {
                    StopDecision::Complete => (SessionStatus::Completed, String::new()),
                    StopDecision::NeedsInput => (
                        SessionStatus::Interrupted,
                        if why.is_empty() {
                            "agent asked for input (stop-needs-input)".to_string()
                        } else {
                            format!("agent asked for input (stop-needs-input): {why}")
                        },
                    ),
                };
                if !self.silent() {
                    match decision {
                        StopDecision::Complete => {
                            term::out(&term::dim("· stop-complete — ending turn"));
                        }
                        StopDecision::NeedsInput => {
                            term::out(&term::yellow(&format!(
                                "· stop-needs-input{} — ending turn",
                                if why.is_empty() { String::new() } else { format!(": {why}") }
                            )));
                        }
                    }
                }
                self.registry.emit(EventKind::AgentEnd, &json!({}));
                self.notify.publish(self.turn_done_event(), false);
                if !self.silent() {
                    self.registry.emit(EventKind::TurnEnd, &json!({ "prompt": user }));
                    self.continuations.extend(self.registry.on_turn_end(user));
                }
                self.mark_status(status, self.goal_pending(), &reason);
                return Ok(());
            }
        }
    }

    /// Re-run the agent's most recent user turn automatically (after a `retry`
    /// verdict). This is the auto-retry continuation: it pops the last
    /// user/assistant/tool-result tail from `history` so the new attempt starts
    /// from the same prompt with a clean slate, compacts history first when it
    /// is near the context cap (so the re-run doesn't blow the model's window),
    /// and then calls [`turn`]. The user prompt is re-derived from the popped
    /// user message. Returns the same `Result` as [`turn`].
    ///
    /// The verdict *not* being `retry` is the caller's responsibility — this
    /// fn just performs the re-run. Safe no-op (returns `Ok(())`) if there is no
    /// prior turn to replay.
    pub fn retry_last_turn(&mut self) -> Result<(), String> {
        // Pull the last contiguous (user, assistant, tool-results…) tail off
        // history so the retry begins at the same prompt. We remove from the
        // last user message onward; anything before it (prior context) is kept
        // intact — the model re-derives the situation from the transcript.
        let pivot = self
            .history
            .iter()
            .rposition(|m| m.role == Role::User)
            .unwrap_or(0);
        let tail: Vec<Message> = self.history.split_off(pivot);
        // The re-prompt is the first user message in the tail we just removed.
        let reprompt = tail
            .iter()
            .find(|m| m.role == Role::User)
            .map(|m| m.text())
            .unwrap_or_default();
        if reprompt.trim().is_empty() {
            // Nothing to replay: put it back and bail cleanly.
            self.history.extend(tail);
            return Ok(());
        }
        // Compact *before* the retry when we're within ~20% of the context cap,
        // so the re-run has room for a full new attempt (and doesn't 400 on the
        // provider's combined input+output limit).
        let ctx = self.model.context.unwrap_or(200_000) as usize;
        if approx_tokens(&self.history) > (ctx * 80 / 100) {
            self.trim();
            if !self.silent() {
                term::out(&term::dim("[pir: compacted history before auto-retry]"));
            }
        }
        if !self.silent() {
            term::out(&term::dim(&format!("· auto-retry: re-running last turn (\"{}\")", truncate(&reprompt, 80))));
        }
        self.turn(&reprompt)
    }

    /// Decide whether to auto-retry the just-finished turn and, if so, run the
    /// retry (possibly compacting first). Called by the REPL right after a turn
    /// completes. Only acts when the user enabled `--auto-retry N` (so
    /// `self.auto_retry` is `Some(n)` with `n > 0`). On each retry attempt the
    /// light model re-classifies the new outcome; if it *still* says `retry`,
    /// we keep going up to `n` times, then stop (the user is handed back the
    /// final result rather than looping forever). A retry attempt that itself
    /// errors is surfaced to the REPL like any other turn error. Returns the
    /// number of retries performed (0 if none / disabled).
    pub fn maybe_auto_retry(&mut self) -> usize {
        let Some(max) = self.auto_retry else { return 0 };
        if max == 0 {
            return 0;
        }
        let Some(log) = self.log_path.clone() else { return 0 };
        // Synchronous verdict (blocks briefly on the light model). If the light
        // model is unavailable we can't auto-classify, so we don't retry blind.
        let Some(verdict) = crate::titler::classify_now(&log) else {
            return 0;
        };
        if verdict != "retry" {
            return 0;
        }
        let mut attempts = 0usize;
        loop {
            if attempts >= max {
                if !self.silent() {
                    term::out(&term::dim(&format!(
                        "· auto-retry: gave up after {max} attempt(s) — still 'needs retry'",
                    )));
                }
                break;
            }
            attempts += 1;
            // Offer the big model a chance to *disagree* with the `retry` verdict
            // before we burn a re-run. If it concurs the task is actually done,
            // we respect that and stop (no retry). A transient model error is
            // treated as "no opinion" -> we still retry defensively.
            if !self.big_model_disagrees_with_retry(&log) {
                if !self.silent() {
                    term::out(&term::dim("· big model agrees it's a real retry — re-running"));
                }
                if let Err(e) = self.retry_last_turn() {
                    if !self.silent() {
                        term::out(&term::red(&format!("✗ auto-retry turn errored: {e}")));
                    }
                    break;
                }
            } else if !self.silent() {
                term::out(&term::dim("· big model thinks the task is actually done — skipping auto-retry"));
            }
            // Re-classify the fresh outcome. If it's no longer `retry`, we're
            // done; if it still is, loop (until we hit `max`).
            let Some(v) = crate::titler::classify_now(&log) else { break };
            if v != "retry" {
                if !self.silent() {
                    term::out(&term::dim(&format!("· auto-retry: attempt #{attempts} settled as '{v}'")));
                }
                break;
            }
        }
        attempts
    }

    /// Ask the *big* (current) model for a one-word second opinion on the light
    /// model's `retry` verdict: is the task actually complete, or genuinely
    /// needs a retry? Returns `true` when the big model thinks it is *done*
    /// (i.e. we should NOT auto-retry) and `false` when it agrees a retry is
    /// warranted (or when the call fails / the verdict is ambiguous). Kept
    /// deliberately cheap — a single non-streaming `complete()` with a tiny
    /// prompt — and never retries itself.
    fn big_model_disagrees_with_retry(&self, log: &Path) -> bool {
        let (last_user, last_asst) = match crate::titler::last_exchange(log) {
            Some((u, a)) => (u.chars().take(200).collect::<String>(), a.chars().take(500).collect::<String>()),
            None => return false,
        };
        let system = "You review a coding-agent turn that a cheap classifier flagged as needing a retry. \
                      Decide: is the task actually already complete (the failure was incidental / already fixed \
                      / nothing more to do), or does it genuinely need another attempt? Reply with EXACTLY ONE \
                      word: 'done' or 'retry'. No other words, no punctuation.";
        let user = format!(
            "Last user prompt: {last_user}\nLast assistant output: {last_asst}\nword (done or retry):"
        );
        let resp = self.client.complete(&self.model.id, system, &user).unwrap_or_default();
        let w = resp.trim().to_lowercase();
        let first = w.split_whitespace().next().unwrap_or("").trim_matches(|c: char| !c.is_alphanumeric());
        first == "done" || first.contains("done")
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

    /// Open a rewind checkpoint for a new turn (Feature 8). No-op when the
    /// session has no transcript to key the index off (a one-shot run without a
    /// log), because checkpoints are persisted next to the log.
    fn open_checkpoint(&mut self, prompt: &str) {
        let Some(log) = self.log_path.clone() else { return };
        let mut store = crate::checkpoint::Store::load(&log);
        store.begin(prompt, self.history.len(), term::epoch());
        store.prune(&log);
        let _ = store.save(&log);
        self.checkpoints = Some(store);
    }

    /// Capture a file's pre-edit contents into the current turn's checkpoint.
    /// Best-effort: a checkpoint that cannot be written must never block an
    /// edit, and `/undo` still covers the single-file case.
    fn checkpoint_capture(&mut self, path: &Path) {
        let Some(log) = self.log_path.clone() else { return };
        if let Some(store) = self.checkpoints.as_mut() {
            let _ = store.capture(&log, path);
            let _ = store.save(&log);
        }
    }

    /// Note that the current turn ran a shell command, whose side effects a
    /// rewind cannot undo.
    fn checkpoint_note_bash(&mut self) {
        let Some(log) = self.log_path.clone() else { return };
        if let Some(store) = self.checkpoints.as_mut() {
            store.note_bash();
            let _ = store.save(&log);
        }
    }

    /// Number of rewindable turns recorded for this session.
    pub fn checkpoint_count(&self) -> usize {
        self.checkpoints.as_ref().map(|s| s.checkpoints.len()).unwrap_or(0)
    }

    /// Human-readable checkpoint listing for `/checkpoints`.
    pub fn checkpoints_listing(&self) -> String {
        match &self.checkpoints {
            Some(s) => s.listing(),
            None => "(no checkpoints for this session — it has no transcript to key them off)"
                .to_string(),
        }
    }

    /// Rewind the working tree and the conversation to checkpoint `id`
    /// (Feature 8). Returns a status line.
    ///
    /// Both halves move together on purpose: restoring the files while leaving
    /// the model's history intact is what makes an ordinary undo so confusing —
    /// the tree is back but the model still believes it made those edits and
    /// keeps building on them.
    pub fn rewind(&mut self, id: usize) -> String {
        let Some(log) = self.log_path.clone() else {
            return "cannot rewind: this session has no transcript to key checkpoints off".to_string();
        };
        let Some(mut store) = self.checkpoints.take().or_else(|| Some(crate::checkpoint::Store::load(&log))) else {
            return "cannot rewind: checkpoint store unavailable".to_string();
        };
        let (cp, report) = match store.rewind(&log, id) {
            Ok(x) => x,
            Err(e) => {
                self.checkpoints = Some(store);
                return e;
            }
        };
        // Truncate history to where the turn began, then record the truncation
        // in the transcript so a resumed session doesn't replay the undone work.
        let mut dropped = 0usize;
        if cp.history_len <= self.history.len() {
            dropped = self.history.len() - cp.history_len;
            self.history.truncate(cp.history_len);
            // The turn's own user prompt is still in history; drop it too, so
            // the model doesn't see a question whose answer it no longer has.
            if matches!(self.history.last().map(|m| m.role), Some(crate::types::Role::User))
                && self.history.last().map(|m| m.text() == cp.prompt).unwrap_or(false)
            {
                self.history.pop();
                dropped += 1;
            }
        }
        // The single-file undo stack points at snapshots for edits that no
        // longer exist; clearing it avoids `/undo` restoring a stale file on top
        // of the rewound tree.
        self.undo_stack.clear();
        // Drop the goal's stale progress notes for the same reason.
        let mut report = report;
        report.history_dropped = dropped;
        store.save(&log).ok();
        self.checkpoints = Some(store);
        log_notice(
            &mut self.log,
            &format!("[pir] rewound to checkpoint #{} ({} file(s) restored)", cp.id, report.restored),
        );
        report.render(&cp)
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
    /// Carries the running turn's elapsed wall time (zero when no turn has
    /// started yet, e.g. a fresh one-shot path).
    pub fn turn_done_event(&self) -> AgentEvent {
        let duration = self.turn_started.map(|t| t.elapsed()).unwrap_or(std::time::Duration::ZERO);
        AgentEvent::turn_done(
            duration,
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
        // The provider enforces a *combined* limit: text input + tool input +
        // output must fit in `ctx`. So the input budget must reserve room for
        // the model's actual `max_tokens` output (not a hardcoded 8k) plus a
        // little headroom for the system prompt (sent alongside history but not
        // counted in it). Without this, a large output budget (e.g. 64k) pushes
        // the total request past the model's hard context window and the
        // provider rejects it with HTTP 400 "maximum context length is N tokens"
        // — exactly the error this fix addresses. `max_tokens` is the same value
        // handed to the provider call in `turn()`.
        let out_reserve = self.model.max_tokens.unwrap_or(8192) as usize;
        let sys_reserve = 2_000; // rough system-prompt headroom (not in history)
        let budget = ctx
            .saturating_sub(out_reserve)
            .saturating_sub(sys_reserve)
            .max(8192);
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
    // The model is unknown at construction time; per-model overrides are
    // re-applied per-call (see `chat`). Here we resolve the provider-level
    // defaults, falling back to the first model's override when the provider
    // itself has no baseUrl (e.g. a stored OpenCode key with no models.json).
    let kind = provider
        .models
        .first()
        .and_then(|m| provider.model_api(m))
        .or_else(|| provider.kind())
        .ok_or_else(|| format!("provider '{}' has no baseUrl", provider.pid()))?;
    let base = match provider.base_url.as_deref() {
        Some(b) if !b.is_empty() => b.trim_end_matches('/').to_string(),
        _ => match provider
            .models
            .first()
            .and_then(|m| provider.model_base_url(m).map(str::to_string))
        {
            Some(b) => b.trim_end_matches('/').to_string(),
            None => match kind {
                ApiKind::Anthropic => "https://api.anthropic.com/v1".to_string(),
                ApiKind::OpenAi | ApiKind::OpenAiResponses => {
                    return Err(format!("provider '{}' has no baseUrl", provider.pid()))
                }
            },
        },
    };
    let key = provider.api_key().ok_or_else(|| {
        // The `{env:VAR}` reference (if any) was already resolved by
        // `expand_env`; an `Err` here means the variable is unset/empty, which
        // we name explicitly so the user isn't left with a generic failure.
        if let Some(k) = provider.api_key.as_deref()
            && let Some(var) = k.strip_prefix("{env:").and_then(|r| r.strip_suffix('}')) {
                return format!(
                    "no API key for '{}' — the env var {var} is unset or empty (referenced in {}, or set apiKey directly)",
                    provider.pid(),
                    config::pi_dir().join("models-store.json").display()
                );
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
    // HTTP transport for the streaming core: `PIR_HTTP_BACKEND` or the
    // `http_backend` settings.json key (single-transport build — every
    // known name resolves to host libcurl; see HttpBackend::parse).
    // Unknown values fall back to the default rather than failing startup.
    if let Some(name) = config::http_backend_name()
        && let Some(backend) = crate::provider::HttpBackend::parse(&name) {
            client.set_backend(backend);
        }
    // Offline scripted model for tests/puppetry (see `crate::fake`): enabled
    // by provider id so a user catalog can never collide with it by model id.
    client.set_fake(provider.pid() == "fake");
    // OpenCode Go routes (and prompt-caches) on a stable per-conversation
    // session id (`x-opencode-session`; see opencode.ai/docs/go). The log
    // stem doesn't exist yet at construction time, so mint the same shape
    // (`pir-<ts>-sh<pid>`) here; `OPENCODE_SESSION_ID` pins it explicitly
    // (e.g. to keep cache hits across resumed sessions). Other providers
    // never see the header (`set_session_id` is only called for Go).
    if provider.pid() == "opencode-go" {
        let id = std::env::var("OPENCODE_SESSION_ID")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                format!(
                    "pir-{}-sh{}",
                    crate::term::timestamp_compact(),
                    crate::term::parent_shell_pid()
                )
            });
        client.set_session_id(Some(id));
    }
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
        "glob" => format!("glob  {}", s("pattern")),
        "remember" => format!("taste {}  {}", s("scope"), s("text")),
        "web_fetch" => format!("fetch {}", s("url")),
        "web_search" => format!("search {}", s("query")),
        "grep" => {
            let p = s("path");
            format!("grep  {}{}", s("pattern"), if p.is_empty() { String::new() } else { format!("  in {p}") })
        }
        "agent" => {
            let t = input.get("subagent_type").and_then(Value::as_str).unwrap_or("general");
            let d = s("description");
            format!("agent {t}  {}", if d.is_empty() { "(no label)" } else { d })
        }
        "ask_user_question" => {
            // Show the first question's header + text; a multi-question ask is
            // summarised with a count so the echo stays one line.
            let qs = input.get("questions").and_then(Value::as_array);
            let single = input.get("question").and_then(Value::as_str);
            match (single, qs) {
                (Some(t), _) => format!("ask   {t}"),
                (None, Some(a)) if a.len() == 1 => {
                    let header = a[0].get("header").and_then(Value::as_str).unwrap_or("");
                    let text = a[0].get("question").and_then(Value::as_str).unwrap_or("");
                    format!("ask   {header}: {text}")
                }
                (None, Some(a)) => format!("ask   {} questions", a.len()),
                _ => "ask".to_string(),
            }
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
        "request_stop" => {
            format!("stop  {} {}", s("reason"), s("message"))
        }
        other => other.to_string(),
    }
}

/// Lines of a tool result echoed from the head and the tail when the output is
/// long. The tail matters: the `bash` tool appends `[stderr]` and
/// `[exit code N]` at the end, so a head-only preview hid exactly the part a
/// human needs in order to see a command fail.
const OUTPUT_HEAD_LINES: usize = 15;
const OUTPUT_TAIL_LINES: usize = 15;
/// Longest single line echoed verbatim; longer lines are clipped.
const OUTPUT_LINE_CHARS: usize = 240;

/// Format a tool result for the human-facing REPL echo.
///
/// The model and the session log still receive the untouched `content`; this is
/// only what is scrolled into the terminal, so it must stay readable for a
/// 30k-line build log while still showing that something failed. Control
/// characters are sanitized (a stray `\r` or escape must not overwrite the
/// prompt), `\r\n`/`\r` become line breaks, and the middle is elided when the
/// output exceeds the head+tail budget.
pub(crate) fn abridge_output(content: &str) -> String {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let clean = sanitize_for_terminal(&normalized);
    let clean = clean.trim_end_matches(['\n', ' ', '\t']);
    if clean.is_empty() {
        return "(no output)".to_string();
    }
    let lines: Vec<&str> = clean.lines().collect();
    let total = lines.len();
    let mut out_lines: Vec<String> = Vec::new();
    if total <= OUTPUT_HEAD_LINES + OUTPUT_TAIL_LINES {
        out_lines.extend(lines.iter().map(|l| clip_line(l, OUTPUT_LINE_CHARS)));
    } else {
        out_lines.extend(
            lines[..OUTPUT_HEAD_LINES]
                .iter()
                .map(|l| clip_line(l, OUTPUT_LINE_CHARS)),
        );
        out_lines.push(format!(
            "… [pir] {} line(s) elided",
            total - OUTPUT_HEAD_LINES - OUTPUT_TAIL_LINES
        ));
        out_lines.extend(
            lines[total - OUTPUT_TAIL_LINES..]
                .iter()
                .map(|l| clip_line(l, OUTPUT_LINE_CHARS)),
        );
    }
    out_lines.join("\n")
}

/// Clip one output line to `max` chars (on a char boundary) with an ellipsis.
fn clip_line(line: &str, max: usize) -> String {
    let line = line.trim_end();
    if line.chars().count() <= max {
        return line.to_string();
    }
    let mut s: String = line.chars().take(max.saturating_sub(1)).collect();
    s.push('…');
    s
}

/// Truncate `s` to `n` chars (with a trailing ellipsis) for compact status lines.
fn truncate(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>())
    }
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

/// Append a transcript-only notice (retry/failure bookkeeping) to the session
/// log. `role: "notice"` entries are display + forensics: `Agent::load_session`
/// skips them so they are never replayed into model context (which would
/// pollute prompts and could even 400 strict providers).
fn log_notice(log: &mut Option<fs::File>, text: &str) {
    let Some(f) = log.as_mut() else { return };
    let entry = json!({
        "ts": term::epoch(),
        "role": "notice",
        "blocks": [{ "type": "text", "text": text }],
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
#[cfg(unix)]
    if let Some(d) = crate::user::session_dir_for(&cwd) {
        return d;
    }
    // The project-local `.pir/sessions` dir may not exist on a fresh project;
    // if we can create it (i.e. we own `.pir`), prefer it over the global one.
    let local = cwd.join(".pir").join("sessions");
    if let Some(parent) = local.parent()
        && parent.exists() && std::fs::create_dir_all(&local).is_ok() {
            return local;
        }
    config::pi_dir().join("agent").join("sessions")
}

/// Prompt-parity matrix (docs/PROMPT_PARITY.md §3.6, pir-only rows): the
/// system prompt keeps pi's *shape* (Available tools, Guidelines,
/// <project_context>, Current working directory) with pir's *content*
/// (identity, PIR docs, pir tool names, terse rules) — and never pi's.
#[cfg(test)]
mod reload_tests {
    use super::*;

    fn test_provider(id: &str, model_id: &str) -> Provider {
        Provider {
            id: Some(id.to_string()),
            name: None,
            base_url: Some("http://127.0.0.1:9".to_string()),
            api_key: Some("k".to_string()),
            api: Some("openai".to_string()),
            models: vec![crate::config::Model {
                id: model_id.to_string(),
                name: None,
                context: Some(1000),
                max_tokens: Some(100),
                api_override: None,
                url_override: None,
                no_reasoning_effort: false,
                reasoning: false,
                thinking_format: None,
                supports_reasoning_effort: None,
                session_affinity_format: None,
                thinking_level_map: Default::default(),
                price_per_1k: None,
            }],
        }
    }

    fn test_agent() -> Agent {
        // `Agent::new` reads the real store; override the cache + active
        // model with test fixtures instead.
        let cancel = Arc::new(AtomicBool::new(false));
        let p = test_provider("p1", "m1");
        let m = p.models[0].clone();
        let mut a = Agent::new(
            p.clone(),
            m.clone(),
            false,
            true,
            std::sync::Arc::new(crate::notify::NotifyBus::new(
                crate::notify::NotifyPolicy::default(),
            )),
            None,
            cancel,
            Arc::new(Mutex::new(String::new())),
        )
        .expect("test agent builds");
        a.cached_providers = vec![p];
        a
    }

    #[test]
    fn reload_keeps_working_catalog_on_unreadable_store() {
        // Point PI_DIR at an empty dir so load_providers fails: reload must
        // keep the working catalog (count unchanged) and report not-refreshed,
        // never empty the cache.
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("pir_reload_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = std::env::var_os("PI_DIR");
        // SAFETY: test-only env mutation, serialized by TEST_ENV_LOCK.
        unsafe { std::env::set_var("PI_DIR", &dir); }
        let mut a = test_agent();
        let before = a.cached_providers.len();
        let (n, refreshed) = a.reload_catalog();
        match old {
            Some(v) => unsafe { std::env::set_var("PI_DIR", v) },
            None => unsafe { std::env::remove_var("PI_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(n, before, "unreadable store must keep the working catalog");
        assert!(!refreshed, "nothing refreshed when the store is unreadable");
        assert_eq!(a.model.id, "m1", "active model untouched");
    }

    #[test]
    fn reload_never_moves_active_model_off_label() {
        // A fresh catalog lacking the active model: reload reports
        // not-refreshed and leaves provider/model alone (no silent move).
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("pir_reload2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let agent_dir = dir.join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("models-store.json"),
            serde_json::json!({
                "providers": [{
                    "id": "other",
                    "baseUrl": "http://127.0.0.1:9",
                    "apiKey": "k",
                    "api": "openai",
                    "models": [{ "id": "othermodel" }]
                }]
            })
            .to_string(),
        )
        .unwrap();
        let old = std::env::var_os("PI_DIR");
        // SAFETY: test-only env mutation, serialized by TEST_ENV_LOCK.
        unsafe { std::env::set_var("PI_DIR", &dir); }
        let mut a = test_agent();
        let (n, refreshed) = a.reload_catalog();
        match old {
            Some(v) => unsafe { std::env::set_var("PI_DIR", v) },
            None => unsafe { std::env::remove_var("PI_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(n, 1, "fresh catalog has one provider");
        assert!(!refreshed, "vanished model must not refresh");
        assert_eq!(a.provider.pid(), "p1", "provider untouched");
        assert_eq!(a.model.id, "m1", "model untouched");
    }
}

#[cfg(test)]
mod prompt_parity_tests {
    use super::build_system_prompt;
    use std::path::PathBuf;

    fn prompt() -> String {
        // Point at dirs without AGENTS.md so assertions cover the stable
        // core (project blocks are covered separately below).
        build_system_prompt(&PathBuf::from("/nonexistent-wt-xyz"))
    }

    #[test]
    fn identity_diverges_from_pi() {
        let p = prompt();
        assert!(
            p.contains("You are pir, a minimal terminal coding agent"),
            "pir identity line missing"
        );
        assert!(
            !p.contains("operating inside pi"),
            "must never claim to operate inside pi"
        );
    }

    #[test]
    fn docs_section_points_at_pir() {
        let p = prompt();
        assert!(p.contains("PIR documentation"), "PIR docs section missing");
        assert!(!p.contains("packages/coding-agent"), "must not reference pi's package paths");
        assert!(!p.contains("PI_*"), "must not reference pi's PI_* vars");
        assert!(p.contains("PIR_*"), "must mention pir's own PIR_* vars");
    }

    #[test]
    fn tool_list_names_pir_tools() {
        let p = prompt();
        for tool in [
            "read_file",
            "edit_file",
            "write_file",
            "list_dir",
            "glob",
            "grep",
            "ask_user_question",
            "load_skill",
            "plan_write",
            "agent",
            "web_fetch",
            "web_search",
            "remember",
            "bash",
            "update_goal",
            "request_stop",
        ] {
            assert!(p.contains(tool), "tool {tool} missing from Available tools");
        }
        // pi's bare names must not appear as list entries (`- read:`); the
        // pir names contain them as substrings, so anchor on the entry shape.
        for bare in ["\n- read:", "\n- edit:", "\n- write:", "\n- ls:"] {
            assert!(!p.contains(bare), "pi-style tool entry {bare:?} must not appear");
        }
        assert!(
            p.contains("custom tools depending on the project"),
            "custom-tools note missing"
        );
    }

    #[test]
    fn guidelines_keep_pir_rules() {
        let p = prompt();
        assert!(p.contains("Guidelines:"), "Guidelines section missing");
        assert!(p.contains("Be terse"), "terse rule missing");
        assert!(p.contains("summarize what changed"), "summary rule missing");
        assert!(p.contains("Show file paths clearly"), "file-paths rule missing");
        assert!(p.contains("old_string must match exactly"), "edit discipline missing");
    }

    #[test]
    fn shape_matches_pi() {
        let p = prompt();
        for section in [
            "Available tools:",
            "Guidelines:",
            "PIR documentation",
            "Current working directory:",
            "Environment:",
        ] {
            assert!(p.contains(section), "shape section {section:?} missing");
        }
    }

    #[test]
    fn context_files_walk_parents_with_precedence() {
        // base/AGENTS.md + sub/{AGENTS.md wins over CLAUDE.md} +
        // deep/{AGENTS.override.md wins over both}, root-first order.
        let base = std::env::temp_dir().join(format!("pir_ctx_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let sub = base.join("sub");
        let deep = sub.join("deep");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(base.join("AGENTS.md"), "root\n").unwrap();
        std::fs::write(sub.join("CLAUDE.md"), "claude\n").unwrap();
        std::fs::write(sub.join("AGENTS.md"), "sub\n").unwrap();
        std::fs::write(deep.join("CLAUDE.md"), "deep-claude\n").unwrap();
        std::fs::write(deep.join("AGENTS.md"), "deep\n").unwrap();
        std::fs::write(deep.join("AGENTS.override.md"), "over\n").unwrap();
        let found = super::context_files(&deep);
        let _ = std::fs::remove_dir_all(&base);
        // Keep only entries under our tree (the runner's real global file,
        // if any, sorts first and is not under test here).
        let ours: Vec<(String, String)> = found
            .into_iter()
            .filter(|(p, _)| p.starts_with(&base))
            .map(|(p, s)| {
                let rel = p.strip_prefix(&base).unwrap().display().to_string();
                (rel, s.trim().to_string())
            })
            .collect();
        let names: Vec<&str> = ours.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            names,
            vec!["AGENTS.md", "sub/AGENTS.md", "sub/deep/AGENTS.override.md"],
            "root-first, one file per dir with override>AGENTS>CLAUDE"
        );
        let bodies: Vec<&str> = ours.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(bodies, vec!["root", "sub", "over"]);
    }

    #[test]
    fn project_block_uses_pi_shape() {
        // With a real AGENTS.md present, project instructions render in pi's
        // <project_context>/<project_instructions> shape (not `# Extra`).
        let dir = std::env::temp_dir().join(format!("pir_parity_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "Be excellent.\n").unwrap();
        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let p = build_system_prompt(&dir);
        std::env::set_current_dir(&cwd).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(p.contains("<project_context>"), "project_context wrapper missing");
        assert!(p.contains("<project_instructions"), "project_instructions tag missing");
        assert!(p.contains("Be excellent."), "project content missing");
        assert!(!p.contains("# Extra instructions"), "old heading must be gone");
    }
}

#[cfg(test)]
mod goal_bootstrap_tests {
    use super::*;
    use crate::config::Provider;
    use crate::notify::shared_bus;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;

    pub(super) fn fresh_agent() -> Agent {
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
    fn ask_tool_reports_no_interactive_user_when_headless() {
        // The critical guarantee: an unattended turn must never wedge on a
        // question nobody can answer. With no renderer installed (the headless
        // / one-shot / -bg case) the tool returns an explicit, actionable
        // result instead of blocking.
        let _g = crate::askuser::ask_lock();
        crate::askuser::uninstall();
        let mut a = fresh_agent();
        let out = a
            .run_ask_tool(
                "ask_user_question",
                &serde_json::json!({"questions":[{"header":"DB","question":"which db?",
                    "options":[{"label":"postgres"},{"label":"sqlite"}]}]}),
            )
            .expect("the tool is intercepted by the agent");
        assert!(out.is_error, "headless ask must not look like a successful answer");
        assert!(out.content.contains("No interactive user"), "got: {}", out.content);
        assert!(out.content.contains("request_stop"), "must point at the escape hatch");
    }

    #[test]
    fn ask_tool_surfaces_a_malformed_question_as_a_correctable_error() {
        let _g = crate::askuser::ask_lock();
        let mut a = fresh_agent();
        let out = a
            .run_ask_tool("ask_user_question", &serde_json::json!({"questions":[{}]}))
            .expect("intercepted");
        assert!(out.is_error);
        assert!(out.content.contains("ask_user_question:"), "got: {}", out.content);
        assert!(out.content.contains("question"), "must name the bad field: {}", out.content);
    }

    #[test]
    fn ask_tool_returns_the_operators_answer() {
        // With a renderer installed (the interactive REPL) the answer is fed
        // back verbatim as the tool result, so the model resumes with it in
        // context. Serialized against the other askuser tests' env/renderer use.
        let _g = crate::askuser::tests::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::askuser::install(std::sync::Arc::new(|q| {
            assert_eq!(q.header, "DB");
            crate::askuser::Answer::Chosen(vec!["sqlite".into()])
        }));
        let mut a = fresh_agent();
        let out = a
            .run_ask_tool(
                "ask_user_question",
                &serde_json::json!({"question":"which db?","header":"DB","options":["postgres","sqlite"]}),
            )
            .expect("intercepted");
        crate::askuser::uninstall();
        assert!(!out.is_error, "got: {}", out.content);
        assert!(out.content.contains("sqlite"), "answer must reach the model: {}", out.content);
    }

    #[test]
    fn non_ask_tools_fall_through() {
        let mut a = fresh_agent();
        assert!(a.run_ask_tool("bash", &serde_json::json!({})).is_none());
    }

    #[test]
    fn skill_tool_lists_loads_and_reports_unknown_skills() {
        let mut a = fresh_agent();
        // No skills installed => the listing says so, and is not an error (the
        // model simply has nothing to load).
        let out = a.run_skill_tool("load_skill", &serde_json::json!({})).expect("intercepted");
        assert!(!out.is_error, "an empty catalog is not an error: {}", out.content);
        assert!(out.content.contains("no skills") || out.content.contains("Skills"), "got: {}", out.content);
        // An unknown name must be a correctable error that lists what exists.
        let out = a
            .run_skill_tool("load_skill", &serde_json::json!({"name": "definitely-not-a-skill"}))
            .expect("intercepted");
        assert!(out.is_error);
        assert!(out.content.contains("no skill named"), "got: {}", out.content);
        // Non-skill tools fall through to the registry.
        assert!(a.run_skill_tool("bash", &serde_json::json!({})).is_none());
    }

    /// The bundled `locate` skill must be discoverable and loadable through the
    /// tool, end to end — the "have a locate skill" requirement.
    #[test]
    fn bundled_locate_skill_loads_through_the_tool() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("pir_skill_tool_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let old = std::env::var_os("PI_DIR");
        // SAFETY: test-local env mutation under the shared env lock.
        unsafe { std::env::set_var("PI_DIR", &dir); }
        crate::skills::seed_bundled();
        let mut a = fresh_agent();
        // The catalog the model sees must name `locate`...
        let listing = a.run_skill_tool("load_skill", &serde_json::json!({})).expect("intercepted");
        assert!(listing.content.contains("locate"), "got: {}", listing.content);
        // ...and loading it by name returns the full body, including its
        // tool recipes.
        let loaded = a
            .run_skill_tool("load_skill", &serde_json::json!({"name": "locate"}))
            .expect("intercepted");
        assert!(!loaded.is_error, "got: {}", loaded.content);
        assert!(loaded.content.contains("# Skill: locate"), "got: {}", loaded.content);
        assert!(loaded.content.contains("glob("), "body must include its recipes");
        match old {
            Some(v) => unsafe { std::env::set_var("PI_DIR", v) },
            None => unsafe { std::env::remove_var("PI_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The system prompt must advertise skills only when there are any — a
    /// skill-less session's prompt stays byte-identical to before.
    #[test]
    fn system_prompt_includes_the_skills_catalog() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("pir_skill_prompt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let old = std::env::var_os("PI_DIR");
        // SAFETY: test-local env mutation under the shared env lock.
        unsafe { std::env::set_var("PI_DIR", &dir); }
        crate::skills::seed_bundled();
        let p = build_system_prompt(&std::env::temp_dir());
        assert!(p.contains("# Skills"), "catalog section missing");
        assert!(p.contains("- locate (user):"), "locate must be catalogued: {p}");
        assert!(p.contains("load_skill"), "the tool must be named");
        match old {
            Some(v) => unsafe { std::env::set_var("PI_DIR", v) },
            None => unsafe { std::env::remove_var("PI_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_mode_toggles_the_security_level_and_refuses_writes() {
        let mut a = fresh_agent();
        assert!(!a.plan_mode());
        assert!(!a.security_policy().unwrap().level.is_plan());
        let msg = a.set_plan_mode(true);
        assert!(a.plan_mode());
        assert!(msg.contains("plan mode ON"), "got: {msg}");
        assert!(msg.contains("read-only"));
        // The live policy is now Plan, so the guardrail refuses writes and exec
        // but still allows reads/searches.
        let policy = a.security_policy().unwrap();
        assert_eq!(policy.level, crate::security::SecurityLevel::Plan);
        use crate::security::{Ask, Op, SecurityPolicy};
        // (decide on the captured policy, not through the ask-sink, so a TTY
        // in the test runner can't turn the denial into an interactive prompt)
        let captured: SecurityPolicy = policy;
        assert!(matches!(
            captured.decide(&Ask::write("/tmp/x")),
            crate::security::Verdict::Deny { .. }
        ));
        assert!(matches!(
            captured.decide(&Ask::new(Op::Exec)),
            crate::security::Verdict::Deny { .. }
        ));
        assert_eq!(captured.decide(&Ask::read("/tmp/x")), crate::security::Verdict::Allow);
        assert_eq!(captured.decide(&Ask::new(Op::Search)), crate::security::Verdict::Allow);

        let msg = a.set_plan_mode(false);
        assert!(!a.plan_mode());
        assert!(msg.contains("OFF"), "got: {msg}");
        assert!(!a.security_policy().unwrap().level.is_plan());
        // Idempotent.
        assert!(a.set_plan_mode(false).contains("already off"));
        // `set_plan_mode` flips the *process-global* mitigation flag, so restore
        // the default posture before returning — otherwise a parallel test that
        // reads it (the builtin bash tool's analyzer gate) sees a stale value.
        crate::security::set_mitigation_active(true);
    }

    #[test]
    fn plan_write_writes_the_document_and_stays_read_only_headless() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("pir_plan_tool_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let old = std::env::var_os("PIR_PLANS_DIR");
        // SAFETY: test-local env mutation under the shared env lock.
        unsafe { std::env::set_var("PIR_PLANS_DIR", &dir); }
        // No operator => the plan is written but never auto-approved, and plan
        // mode stays ON. Silently exiting would hand the model write access it
        // was never granted.
        let _g = crate::askuser::ask_lock();
        crate::askuser::uninstall();
        let mut a = fresh_agent();
        a.set_plan_mode(true);
        let out = a
            .run_plan_tool(
                "plan_write",
                &serde_json::json!({"name": "x", "content": "# Plan: x\n\n1. do it\n"}),
            )
            .expect("intercepted");
        assert!(!out.is_error, "got: {}", out.content);
        assert!(out.content.contains("No interactive operator"), "got: {}", out.content);
        assert!(out.content.contains("request_stop"), "must point at the escape hatch");
        assert!(a.plan_mode(), "headless plan write must not leave plan mode");
        let p = dir.join("x.md");
        assert!(p.is_file(), "the plan document must be on disk: {}", p.display());
        assert!(std::fs::read_to_string(&p).unwrap().contains("do it"));
        // Missing content is a correctable error.
        let bad = a.run_plan_tool("plan_write", &serde_json::json!({})).unwrap();
        assert!(bad.is_error);
        assert!(bad.content.contains("'content' is required"), "got: {}", bad.content);
        // Non-plan tools fall through.
        assert!(a.run_plan_tool("bash", &serde_json::json!({})).is_none());
        match old {
            Some(v) => unsafe { std::env::set_var("PIR_PLANS_DIR", v) },
            None => unsafe { std::env::remove_var("PIR_PLANS_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_write_approval_leaves_plan_mode_and_rejection_keeps_it() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("pir_plan_appr_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let old = std::env::var_os("PIR_PLANS_DIR");
        // SAFETY: test-local env mutation under the shared env lock.
        unsafe { std::env::set_var("PIR_PLANS_DIR", &dir); }
        let _g = crate::askuser::ask_lock();

        // Rejection: stay read-only and hand the feedback back.
        crate::askuser::install(std::sync::Arc::new(|_| {
            crate::askuser::Answer::Text("use sqlite, not postgres".into())
        }));
        let mut a = fresh_agent();
        a.set_plan_mode(true);
        let out = a
            .run_plan_tool("plan_write", &serde_json::json!({"content": "# Plan\nbody"}))
            .unwrap();
        assert!(a.plan_mode(), "a rejected plan must not leave plan mode");
        assert!(out.content.contains("NOT approved"), "got: {}", out.content);
        assert!(out.content.contains("sqlite"), "feedback must reach the model: {}", out.content);

        // Approval: leave plan mode so implementation may begin.
        crate::askuser::install(std::sync::Arc::new(|_| {
            crate::askuser::Answer::Chosen(vec!["Approve — implement it".into()])
        }));
        let out = a
            .run_plan_tool("plan_write", &serde_json::json!({"content": "# Plan\nbody 2"}))
            .unwrap();
        crate::askuser::uninstall();
        assert!(!a.plan_mode(), "an approved plan leaves plan mode");
        assert!(out.content.contains("APPROVED"), "got: {}", out.content);
        match old {
            Some(v) => unsafe { std::env::set_var("PIR_PLANS_DIR", v) },
            None => unsafe { std::env::remove_var("PIR_PLANS_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_mode_is_persisted_next_to_the_session_log() {
        let dir = std::env::temp_dir().join(format!("pir_plan_side_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("pir-test.jsonl");
        std::fs::write(&log, "{}\n").unwrap();
        let mut a = fresh_agent();
        a.log_path = Some(log.clone());
        a.set_plan_mode(true);
        assert_eq!(std::fs::read_to_string(log.with_extension("plan")).unwrap().trim(), "1");
        // A resumed session restores the read-only posture, with a notice.
        let mut b = fresh_agent();
        b.log_path = Some(log.clone());
        assert!(b.apply_persisted_plan_mode().is_some(), "must report the restored mode");
        assert!(b.plan_mode());
        // And turning it off clears the sidecar so the next resume is writable.
        a.set_plan_mode(false);
        assert_eq!(std::fs::read_to_string(log.with_extension("plan")).unwrap().trim(), "0");
        let mut c = fresh_agent();
        c.log_path = Some(log);
        assert!(c.apply_persisted_plan_mode().is_none());
        assert!(!c.plan_mode());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// In plan mode the *system prompt* must tell the model why its writes are
    /// being refused, or it will keep retrying them.
    ///
    /// Sets `plan_mode` directly (rather than via `set_plan_mode`) on purpose:
    /// `set_plan_mode` also applies the security policy, which flips the
    /// *process-global* mitigation flag — a cross-test race in a parallel run.
    /// The prompt banner is what is under test here; the policy side is covered
    /// by `plan_mode_toggles_the_security_level_and_refuses_writes`.
    #[test]
    fn plan_mode_adds_a_read_only_banner_to_the_system_prompt() {
        // Deliberately does NOT take the shared env lock: the assertion is a
        // prefix comparison, so concurrent changes to the prompt's volatile tail
        // (the skills catalog / scratchpad line) cannot affect it. Taking the
        // lock here would only lengthen the queue other env-sensitive tests wait
        // in — the suite's documented parallelism ceiling is contention, not CPU.
        let mut a = fresh_agent();
        assert!(!a.system.contains("PLAN MODE"), "a fresh agent must not be planning");
        a.plan_mode = true;
        a.refresh_system();
        assert!(a.system.contains("PLAN MODE (read-only)"), "banner missing");
        assert!(a.system.contains("plan_write"), "must name the tool that works");
        let with_banner = a.system.clone();
        a.plan_mode = false;
        a.refresh_system();
        assert!(!a.system.contains("PLAN MODE"), "banner must be removed");
        // The banner is appended, so the non-planning prompt must be an exact
        // prefix of the planning one — the whole point being that nothing else
        // in the prompt changes when plan mode is toggled.
        assert!(
            with_banner.starts_with(&a.system),
            "toggling plan mode must only append the banner"
        );
        assert!(with_banner.len() > a.system.len());
    }

    #[test]
    fn subagent_tool_validates_input_and_blocks_nesting() {
        let mut a = fresh_agent();
        // A missing prompt is a correctable error, not a spawned child.
        let out = a.run_subagent_tool("agent", &serde_json::json!({})).expect("intercepted");
        assert!(out.is_error);
        assert!(out.content.contains("'prompt' is required"), "got: {}", out.content);
        // An unknown type names the valid ones.
        let out = a
            .run_subagent_tool("agent", &serde_json::json!({"prompt": "x", "subagent_type": "wizard"}))
            .expect("intercepted");
        assert!(out.is_error);
        assert!(out.content.contains("explore, plan or general"), "got: {}", out.content);
        // Non-agent tools fall through.
        assert!(a.run_subagent_tool("bash", &serde_json::json!({})).is_none());
    }

    #[test]
    fn subagent_kind_clamps_the_posture_and_filters_tools() {
        use crate::subagent::Kind;
        let p: Provider =
            serde_json::from_str(r#"{"id":"t","baseUrl":"https://example.invalid/v1","apiKey":"x","api":"openai","models":[{"id":"m"}]}"#).unwrap();
        let m = p.models[0].clone();
        let mk = |kind: Kind| {
            Agent::new_subagent(
                p.clone(),
                m.clone(),
                true,
                kind,
                Some(crate::security::SecurityPolicy::default()),
                Arc::new(AtomicBool::new(false)),
            )
            .expect("sub-agent builds")
        };
        // explore/plan are strictly read-only whatever the parent's level.
        for kind in [Kind::Explore, Kind::Plan] {
            let child = mk(kind);
            assert_eq!(child.security_policy().unwrap().level, crate::security::SecurityLevel::Plan);
            assert_eq!(child.plan_mode(), kind == Kind::Plan);
            assert!(child.system.contains("READ-ONLY"), "preamble missing for {kind:?}");
        }
        // general inherits the parent's posture (mitigation here).
        let child = mk(Kind::General);
        assert_eq!(
            child.security_policy().unwrap().level,
            crate::security::SecurityLevel::Mitigation
        );
        assert!(!child.plan_mode());
        // A read-only child must not even see the tool that could widen it.
        let specs: Vec<&str> = child
            .registry
            .specs()
            .iter()
            .map(|s| s.name)
            .collect();
        assert!(specs.contains(&"agent"), "the parent keeps the agent tool");
        // Every sub-agent hides the parent-only tools.
        for kind in [Kind::Explore, Kind::Plan, Kind::General] {
            let child = mk(kind);
            let kept: Vec<&str> = child.registry.specs().iter().map(|s| s.name).collect();
            assert!(kept.contains(&"read_file") || kept.contains(&"glob"), "child needs read tools");
        }
    }

    #[test]
    fn subagent_tool_refuses_nesting() {
        use crate::subagent::Kind;
        let p: Provider =
            serde_json::from_str(r#"{"id":"t","baseUrl":"https://example.invalid/v1","apiKey":"x","api":"openai","models":[{"id":"m"}]}"#).unwrap();
        let m = p.models[0].clone();
        let mut child = Agent::new_subagent(
            p,
            m,
            true,
            Kind::General,
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect("sub-agent builds");
        let out = child
            .run_subagent_tool("agent", &serde_json::json!({"prompt": "spawn another"}))
            .expect("intercepted");
        assert!(out.is_error, "a sub-agent must not delegate further");
        assert!(out.content.contains("cannot delegate further"), "got: {}", out.content);
    }

    /// End-to-end delegation with the offline scripted model: a sub-agent
    /// really runs a turn, and its final reply becomes the tool report. This is
    /// the "does the feature actually work" test — no network, deterministic.
    #[test]
    fn subagent_runs_a_turn_and_reports_its_reply() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("PIR_FAKE_MODEL");
        // SAFETY: test-local env mutation under the shared env lock; the fake
        // provider only registers when this is set.
        unsafe { std::env::set_var("PIR_FAKE_MODEL", "1"); }
        let providers = crate::config::load_providers().expect("providers");
        let (p, m) = crate::config::select(&providers, "fake/slow").expect("fake provider");
        let spawn = crate::subagent::Spawn {
            kind: crate::subagent::Kind::Explore,
            description: "ping".into(),
            prompt: "FAKE: echo subagent-reply-ok".into(),
            background: false,
        };
        let report = crate::subagent::run(
            p.clone(),
            m.clone(),
            &spawn,
            true,
            Some(crate::security::SecurityPolicy::default()),
            Arc::new(AtomicBool::new(false)),
        );
        match old {
            Some(v) => unsafe { std::env::set_var("PIR_FAKE_MODEL", v) },
            None => unsafe { std::env::remove_var("PIR_FAKE_MODEL") },
        }
        assert!(!report.timed_out, "the fake model must not time out");
        assert!(!report.error, "sub-agent failed: {}", report.text);
        assert!(
            report.text.contains("subagent-reply-ok"),
            "the child's reply must be the report: {:?}",
            report.text
        );
    }

    #[test]
    fn taste_tool_records_reinforces_and_validates() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = std::env::temp_dir().join(format!("pir_taste_tool_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cfg);
        std::fs::create_dir_all(&cfg).unwrap();
        let old = std::env::var_os("PI_DIR");
        // SAFETY: test-local env mutation under the shared env lock.
        unsafe { std::env::set_var("PI_DIR", &cfg); }
        let mut a = fresh_agent();
        // Missing text is a correctable error, not an empty entry.
        let out = a.run_taste_tool("remember", &serde_json::json!({})).expect("intercepted");
        assert!(out.is_error);
        assert!(out.content.contains("'text' is required"), "got: {}", out.content);
        // A bad scope is rejected with the valid values.
        let out = a
            .run_taste_tool("remember", &serde_json::json!({"text": "x", "scope": "galaxy"}))
            .expect("intercepted");
        assert!(out.is_error);
        assert!(out.content.contains("project' or 'user'"), "got: {}", out.content);
        // A real preference is recorded, persisted, and injected.
        let out = a
            .run_taste_tool(
                "remember",
                &serde_json::json!({"text": "Never commit without asking", "evidence": "operator said so", "confidence": 0.95}),
            )
            .expect("intercepted");
        assert!(!out.is_error, "got: {}", out.content);
        assert!(out.content.contains("Recorded"), "got: {}", out.content);
        assert!(out.content.contains("later sessions"), "must set expectations: {}", out.content);
        assert!(out.content.contains("0.9"), "confidence must be reported: {}", out.content);
        // The live prompt picks it up immediately.
        assert!(a.system.contains("Never commit without asking"), "prompt not refreshed");
        assert!(a.system.contains("# Taste"), "catalog section missing");
        // Re-recording reinforces rather than duplicating.
        let out = a
            .run_taste_tool("remember", &serde_json::json!({"text": "never commit without asking!"}))
            .expect("intercepted");
        assert!(out.content.contains("Reinforced"), "got: {}", out.content);
        // And the store on disk has exactly one entry.
        let cwd = std::env::current_dir().unwrap();
        let store = crate::taste::Store::load(crate::taste::Scope::Project, &cwd);
        assert_eq!(store.entries.len(), 1, "a restatement must not duplicate: {:?}", store.entries);
        // Non-taste tools fall through.
        assert!(a.run_taste_tool("bash", &serde_json::json!({})).is_none());
        match old {
            Some(v) => unsafe { std::env::set_var("PI_DIR", v) },
            None => unsafe { std::env::remove_var("PI_DIR") },
        }
        // Clean up the project store this test just wrote into the real tree.
        let _ = std::fs::remove_file(crate::taste::Store::path(crate::taste::Scope::Project, &cwd));
        let _ = std::fs::remove_file(crate::taste::Store::markdown_path(crate::taste::Scope::Project, &cwd));
        let _ = std::fs::remove_dir_all(&cfg);
    }

    #[test]
    fn taste_is_injected_into_the_prompt_only_when_present() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = std::env::temp_dir().join(format!("pir_taste_prompt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cfg);
        std::fs::create_dir_all(&cfg).unwrap();
        let old = std::env::var_os("PI_DIR");
        // SAFETY: test-local env mutation under the shared env lock.
        unsafe { std::env::set_var("PI_DIR", &cfg); }
        // Nothing recorded anywhere -> no section at all.
        let p = build_system_prompt(&std::env::temp_dir());
        assert!(!p.contains("# Taste"), "an empty store must not add a section");
        // Record a user-scope preference -> the section appears.
        let mut store = crate::taste::Store::default();
        store.remember("Prefer rg over grep", "stated", Some(0.9));
        store.save(crate::taste::Scope::Global, &cfg).unwrap();
        let p = build_system_prompt(&std::env::temp_dir());
        assert!(p.contains("# Taste (recorded preferences)"), "got: {p}");
        assert!(p.contains("- Prefer rg over grep"), "got: {p}");
        assert!(p.contains("`remember` tool"), "must name the tool that records more");
        match old {
            Some(v) => unsafe { std::env::set_var("PI_DIR", v) },
            None => unsafe { std::env::remove_var("PI_DIR") },
        }
        let _ = std::fs::remove_dir_all(&cfg);
    }


    /// F8: a turn's checkpoint must be opened at the boundary and capture each
    /// file once, before the first modification of that file.
    #[test]
    fn checkpoint_opens_per_turn_and_captures_once() {
        let dir = std::env::temp_dir().join(format!("pir_ckpt_agent_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("session.jsonl");
        std::fs::write(&log, "{}\n").unwrap();
        let target = dir.join("a.txt");
        std::fs::write(&target, "v0\n").unwrap();

        let mut a = fresh_agent();
        a.log_path = Some(log.clone());
        a.checkpoints = Some(crate::checkpoint::Store::load(&log));

        a.open_checkpoint("first task");
        assert_eq!(a.checkpoint_count(), 1);
        a.checkpoint_capture(&target);
        std::fs::write(&target, "v1\n").unwrap();
        // A second capture in the same turn must not overwrite the snapshot.
        a.checkpoint_capture(&target);
        std::fs::write(&target, "v2\n").unwrap();
        assert_eq!(
            a.checkpoints.as_ref().unwrap().checkpoints[0].files.len(),
            1,
            "one snapshot per file per turn"
        );

        // A new turn opens its own checkpoint.
        a.open_checkpoint("second task");
        assert_eq!(a.checkpoint_count(), 2);

        // Rewind the first: the file goes back to v0 and the second checkpoint
        // is discarded with it.
        let msg = a.rewind(1);
        assert!(msg.contains("rewound to checkpoint #1"), "got: {msg}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v0\n");
        assert_eq!(a.checkpoint_count(), 0, "later checkpoints are discarded too");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F8: rewind must truncate the conversation, not just the files — the whole
    /// point is that the model stops reasoning about undone work.
    #[test]
    fn rewind_truncates_history_to_the_turn_boundary() {
        let dir = std::env::temp_dir().join(format!("pir_ckpt_hist_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("session.jsonl");
        std::fs::write(&log, "{}\n").unwrap();

        let mut a = fresh_agent();
        a.log_path = Some(log.clone());
        a.checkpoints = Some(crate::checkpoint::Store::load(&log));

        // Simulate two completed turns of history.
        a.history.push(crate::types::Message::user("old question"));
        a.history.push(crate::types::Message::user("the one we undo"));
        let before = a.history.len();
        a.open_checkpoint("the one we undo");
        a.history.push(crate::types::Message::user("assistant reply about it"));

        let msg = a.rewind(1);
        assert!(a.history.len() < before + 1, "history must shrink: {}", a.history.len());
        assert_eq!(a.history.len(), 1, "only the pre-turn history survives");
        assert_eq!(a.history[0].text(), "old question");
        assert!(msg.contains("truncated"), "must report the truncation: {msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F8: a session with no transcript cannot persist checkpoints, and must say
    /// so rather than silently pretending rewind is available.
    #[test]
    fn checkpoint_without_a_transcript_is_reported_honestly() {
        let mut a = fresh_agent();
        a.log_path = None;
        a.checkpoints = None;
        assert_eq!(a.checkpoint_count(), 0);
        assert!(a.checkpoints_listing().contains("no checkpoints"), "got: {}", a.checkpoints_listing());
        let msg = a.rewind(1);
        assert!(msg.contains("cannot rewind"), "got: {msg}");
    }

    /// F8: a rewind whose turn ran `bash` must warn that shell side effects are
    /// not undone, so the operator never assumes a complete restore.
    #[test]
    fn rewind_warns_about_shell_side_effects() {
        let dir = std::env::temp_dir().join(format!("pir_ckpt_bash_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("session.jsonl");
        std::fs::write(&log, "{}\n").unwrap();
        let target = dir.join("a.txt");
        std::fs::write(&target, "v0\n").unwrap();

        let mut a = fresh_agent();
        a.log_path = Some(log.clone());
        a.checkpoints = Some(crate::checkpoint::Store::load(&log));
        a.open_checkpoint("run something");
        a.checkpoint_capture(&target);
        a.checkpoint_note_bash();
        std::fs::write(&target, "v1\n").unwrap();

        let msg = a.rewind(1);
        assert!(msg.contains("NOT undone"), "must warn: {msg}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v0\n");
        let _ = std::fs::remove_dir_all(&dir);
    }


    /// F8 end-to-end: a real turn drives `write_file` through the tool loop, and
    /// a rewind restores the file to what it was before that turn. This is the
    /// test that proves the checkpoint is actually wired into the tool path
    /// (unit tests on the store only prove the store works).
    #[test]
    fn rewind_undoes_a_real_turn_that_wrote_a_file() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("PIR_FAKE_MODEL");
        // SAFETY: test-local env mutation under the shared env lock.
        unsafe { std::env::set_var("PIR_FAKE_MODEL", "1"); }

        let dir = std::env::temp_dir().join(format!("pir_ckpt_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("session.jsonl");
        std::fs::write(&log, "{}\n").unwrap();
        let target = dir.join("written.txt");
        std::fs::write(&target, "BEFORE\n").unwrap();

        let providers = crate::config::load_providers().expect("providers");
        let (p, m) = crate::config::select(&providers, "fake/slow").expect("fake provider");
        let mut a = Agent::new(
            p.clone(),
            m.clone(),
            true,
            true, // quiet
            crate::notify::shared_bus(),
            None,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(String::new())),
        )
        .expect("agent");
        a.log_path = Some(log.clone());
        a.checkpoints = Some(crate::checkpoint::Store::load(&log));

        // A real turn; the fake model emits a `write_file` tool call.
        a.turn(&format!("FAKE: write {} REWRITTEN", target.display()))
            .expect("turn runs");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "REWRITTEN",
            "the turn must actually have written the file"
        );
        assert_eq!(a.checkpoint_count(), 1, "the turn opened a checkpoint");
        assert!(
            !a.checkpoints.as_ref().unwrap().checkpoints[0].files.is_empty(),
            "the write must have been captured"
        );

        // Rewind: the file goes back to its pre-turn contents.
        let msg = a.rewind(1);
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "BEFORE\n",
            "rewind must restore the pre-turn contents (msg: {msg})"
        );

        match old {
            Some(v) => unsafe { std::env::set_var("PIR_FAKE_MODEL", v) },
            None => unsafe { std::env::remove_var("PIR_FAKE_MODEL") },
        }
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn stop_decision_parses_free_vocabulary() {
        use super::StopDecision;
        assert_eq!(StopDecision::parse("stop-complete"), Some(StopDecision::Complete));
        assert_eq!(StopDecision::parse("done"), Some(StopDecision::Complete));
        assert_eq!(StopDecision::parse("STOP-NEEDS-INPUT"), Some(StopDecision::NeedsInput));
        assert_eq!(StopDecision::parse("question"), Some(StopDecision::NeedsInput));
        assert_eq!(StopDecision::parse("keep going"), None);
        assert_eq!(StopDecision::Complete.label(), "stop-complete");
        assert_eq!(StopDecision::NeedsInput.label(), "stop-needs-input");
    }

    #[test]
    fn stop_skill_on_by_default_and_toggles() {
        let mut a = fresh_agent();
        assert!(a.stop_skill(), "stop skill must default on");
        assert_eq!(a.last_stop(), None, "no stop recorded yet");
        assert!(a.set_stop_skill(false).contains("off"));
        assert!(!a.stop_skill());
        let out = a.run_stop_tool(
            "request_stop",
            &serde_json::json!({"reason":"stop-complete"}),
        );
        let o = out.expect("outcome");
        assert!(o.is_error, "disabled skill must refuse: {}", o.content);
        assert_eq!(a.last_stop(), None, "refused stop must not record");
        assert!(a.set_stop_skill(true).contains("on"));
        let out = a.run_stop_tool(
            "request_stop",
            &serde_json::json!({"reason":"stop-complete","message":"all green"}),
        );
        let o = out.expect("outcome");
        assert!(!o.is_error, "enabled skill must accept: {}", o.content);
        assert!(o.content.contains("stop-complete"), "got {}", o.content);
        assert_eq!(a.last_stop(), Some(super::StopDecision::Complete));
    }

    #[test]
    fn stop_tool_rejects_unknown_reason() {
        let mut a = fresh_agent();
        assert!(a.stop_skill());
        let out = a.run_stop_tool(
            "request_stop",
            &serde_json::json!({"reason":"keep-going"}),
        );
        let o = out.expect("outcome");
        assert!(o.is_error, "unknown reason must error: {}", o.content);
        assert_eq!(a.last_stop(), None, "rejected stop must not record");
        // Non-stop tools fall through to the registry.
        assert!(a.run_stop_tool("bash", &serde_json::json!({})).is_none());
    }

    #[test]
    fn needs_input_records_question() {
        let mut a = fresh_agent();
        let out = a.run_stop_tool(
            "request_stop",
            &serde_json::json!({"reason":"stop-needs-input","message":"which region?"}),
        );
        let o = out.expect("outcome");
        assert!(!o.is_error, "needs-input must accept: {}", o.content);
        assert!(o.content.contains("stop-needs-input"), "got {}", o.content);
        assert_eq!(a.last_stop(), Some(super::StopDecision::NeedsInput));
    }
}

#[cfg(test)]
mod turn_timer_tests {
    use super::goal_bootstrap_tests::fresh_agent;

    /// Regression: the TurnDone notification used to hardcode a zero duration
    /// ("turn done in 0.0s") even for turns that ran minutes. The event must
    /// carry the running turn's elapsed wall time.
    #[test]
    fn turn_done_event_reports_elapsed_wall_time() {
        let mut a = fresh_agent();
        // No turn started yet: zero, as before (one-shot paths).
        let idle = a.turn_done_event();
        assert_eq!(idle.duration, std::time::Duration::ZERO);
        // A turn started 95s ago must report ~95s, not zero.
        a.turn_started = Some(std::time::Instant::now() - std::time::Duration::from_secs(95));
        let ev = a.turn_done_event();
        assert_eq!(ev.duration.as_secs(), 95, "expected ~95s, got {:?}", ev.duration);
        assert!(
            ev.summary().starts_with("turn done in 95."),
            "summary must show it: {}",
            ev.summary()
        );
    }
}

#[cfg(test)]
mod notice_log_tests {
    use super::goal_bootstrap_tests::fresh_agent;

    /// Regression: retry notices are logged as `role: "notice"` entries so a
    /// later debug can reconstruct attempt history — but resume must skip
    /// them, otherwise transcript bookkeeping would be replayed into model
    /// context (polluting prompts and risking strict-provider 400s).
    #[test]
    fn resume_skips_notice_entries() {
        let mut a = fresh_agent();
        let dir = std::env::temp_dir().join(format!("pir_notice_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let lines = [
            r#"{"ts":1,"role":"user","blocks":[{"type":"text","text":"hi"}]}"#,
            r#"{"ts":2,"role":"notice","blocks":[{"type":"text","text":"⚠ request failed (attempt 99)"}]}"#,
            r#"{"ts":3,"role":"assistant","blocks":[{"type":"text","text":"hello"}]}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let _ = a.load_session(&path);
        let dump = format!("{:?}", a.history);
        assert!(
            !dump.contains("attempt 99"),
            "notice text must never reach replayed history: {dump}"
        );
        assert!(
            dump.contains("hi") && dump.contains("hello"),
            "user+assistant around the notice must still pair: {dump}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The false loop-detector trip: three consecutive stalled-stream
    /// `(empty response)` observations (empty tool sig + empty text sig)
    /// fired the detector and killed a live turn, even though every real
    /// observation before and after was distinct. Empty observations carry
    /// no signal and must be skipped without touching the counters, so the
    /// stop-skill nudge path gets its chance to recover the turn.
    #[test]
    fn empty_observations_never_trip_the_detector() {
        let mut d = super::LoopDetector::new();
        // Two distinct real observations first (the working-turn shape).
        assert!(!d.observe("bash\x01a", "checking x"));
        assert!(!d.observe("bash\x01b", "checking y"));
        // Three consecutive empties (stalled stream): must not fire.
        assert!(!d.observe("", ""));
        assert!(!d.observe("", ""));
        assert!(!d.observe("", ""));
        // Work resumes with distinct tools: still must not fire.
        assert!(!d.observe("bash\x01c", "checking z"));
        assert!(!d.observe("bash\x01d", "checking w"));
    }

    /// The guard itself must still fire: three genuinely identical tool
    /// batches (or texts with no tools) still trip it.
    #[test]
    fn identical_batches_still_trip_the_detector() {
        let mut d = super::LoopDetector::new();
        assert!(!d.observe("read\x01f", "looking"));
        assert!(!d.observe("read\x01f", "looking"));
        assert!(d.observe("read\x01f", "looking"), "identical batch x3 must trip");
        let mut d = super::LoopDetector::new();
        assert!(!d.observe("", "same sentence"));
        assert!(!d.observe("", "same sentence"));
        assert!(d.observe("", "same sentence"), "identical text x3 must trip");
    }
}

#[cfg(test)]
mod output_echo_tests {
    use super::{abridge_output, OUTPUT_HEAD_LINES, OUTPUT_TAIL_LINES};

    /// The regression that motivated this: the agent echoed only the *first
    /// line* of a tool result, so a failing `bash` command's `[stderr]` block
    /// and `[exit code N]` tail were never visible to the human.
    #[test]
    fn long_output_keeps_head_and_tail_with_exit_code() {
        let mut body = String::new();
        for i in 0..100 {
            body.push_str(&format!("line {i}\n"));
        }
        body.push_str("[stderr]\nboom: it failed\n[exit code 1]");
        let shown = abridge_output(&body);
        assert!(shown.starts_with("line 0\n"), "head missing: {shown}");
        assert!(shown.contains("line 14"), "expected the last head line: {shown}");
        assert!(!shown.contains("line 50"), "middle must be elided: {shown}");
        assert!(shown.contains("boom: it failed"), "stderr must be echoed: {shown}");
        assert!(shown.contains("[exit code 1]"), "exit code must be echoed: {shown}");
        assert!(
            shown.contains("line(s) elided"),
            "elision must be marked: {shown}"
        );
    }

    #[test]
    fn exact_budget_is_shown_whole() {
        let budget = OUTPUT_HEAD_LINES + OUTPUT_TAIL_LINES;
        let body: String = (0..budget)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let shown = abridge_output(&body);
        assert!(!shown.contains("elided"), "no elision at the budget: {shown}");
        assert_eq!(shown.lines().count(), budget);
    }

    #[test]
    fn empty_output_is_marked() {
        assert_eq!(abridge_output("   \n\n"), "(no output)");
        assert_eq!(abridge_output(""), "(no output)");
    }

    /// Command output is untrusted: a carriage return or escape sequence must
    /// not be able to overwrite the prompt or recolor the terminal.
    #[test]
    fn control_characters_are_sanitized_and_cr_splits_lines() {
        let shown = abridge_output("progress 1\rprogress 2\n\x1b[31mred\x1b[0m");
        assert!(!shown.contains('\r'), "raw CR survived: {shown:?}");
        assert!(!shown.contains('\x1b'), "raw escape survived: {shown:?}");
        assert!(shown.contains("progress 1") && shown.contains("progress 2"));
    }

    #[test]
    fn overlong_line_is_clipped() {
        let shown = abridge_output(&"x".repeat(1000));
        assert!(shown.ends_with('…'), "long line must be clipped: {shown}");
        assert_eq!(shown.chars().count(), super::OUTPUT_LINE_CHARS);
    }
}

#[cfg(test)]
mod su_security_precedence_tests {
    use super::su_security_override_notice;

    /// The persisted per-session choice wins over the policy — it is the more
    /// specific, operator-made decision — but it must never do so *silently*.
    #[test]
    fn contradicting_policy_is_reported_both_ways() {
        // Session says "off" (act as invoker), project policy says confine.
        let n = su_security_override_notice(false, true, "/tmp/s.susec")
            .expect("off-over-true must be reported");
        assert!(n.contains("su-security OFF"), "{n}");
        assert!(n.contains("user-security = true"), "{n}");
        // The operator is told how to fix it, not just that something is wrong.
        assert!(n.contains("/su-security on"), "{n}");

        // Session says "on" (confine), project policy says act as invoker.
        let n = su_security_override_notice(true, false, "/tmp/s.susec")
            .expect("on-over-false must be reported");
        assert!(n.contains("su-security ON"), "{n}");
        assert!(n.contains("user-security = false"), "{n}");
        assert!(n.contains("/su-security off"), "{n}");
    }

    /// The common case: the sidecar agreed with the policy when it was written.
    /// Nothing to announce — the operator asked for this and got it.
    #[test]
    fn agreement_is_silent() {
        assert!(su_security_override_notice(true, true, "/tmp/s.susec").is_none());
        assert!(su_security_override_notice(false, false, "/tmp/s.susec").is_none());
    }

    /// The sidecar path is surfaced so a confused operator can inspect/remove
    /// the file that is overriding their setting.
    #[test]
    fn notice_names_the_sidecar_path() {
        let n = su_security_override_notice(false, true, "/tmp/x.log.susec").unwrap();
        assert!(n.contains("/tmp/x.log.susec"), "{n}");
    }
}

#[cfg(test)]
mod su_security_sidecar_tests {
    use std::path::PathBuf;

    /// The sidecar name is derived from the session log by extension swap, and
    /// `clear_persisted_su_security` must remove exactly that file — the policy
    /// change path relies on it to invalidate a stale override.
    #[test]
    fn sidecar_is_derived_from_log_and_cleared() {
        let dir = std::env::temp_dir().join(format!("pir_susec_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("s.123.log");
        let sidecar = log.with_extension("susec");
        std::fs::write(&sidecar, "0").unwrap();
        assert!(sidecar.exists());

        // Mirrors `clear_persisted_su_security` with the same derivation.
        let p: PathBuf = log.clone();
        std::fs::remove_file(p.with_extension("susec")).unwrap();
        assert!(!sidecar.exists(), "stale override must be gone");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
