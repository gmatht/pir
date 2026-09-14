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
use std::cell::{Cell, RefCell};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

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
         - job_status: Check on a long-running command that was detached\n\
         - job_kill: Stop a detached long-running command\n\
         - update_goal: Persist and update the current goal/continuation plan\n\
         \n\
         In addition to the tools above, you may have access to other custom tools depending on the project.\n",
    );
    system.push_str(
        "\nGuidelines:\n\
         - Use the tools to actually do the work; don't just describe it.\n\
         - Use bash for file operations like ls, rg, find.\n\
         - Use read_file to examine files instead of cat or sed.\n\
         - Use write_file only for new files or complete rewrites.\n\
         - Use edit_file for precise changes (old_string must match exactly).\n\
         - Keep old_string as small as possible while still being unique in the file.\n\
         - Read before editing; prefer edit_file over write_file for changes.\n\
         - Be terse: code, commands, short answers, no preamble.\n\
         - Show file paths clearly when working with files.\n\
         - When finished, summarize what changed in a sentence or two.\n",
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
        let policy = crate::security::load_policy();
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
            {
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
            su_security_enabled: user_security,
            security,
            thinking: config::ThinkingLevel::Off,
            show_thinking: true,
            auto_retry: None,
            incremental_md: config::incremental_md_default(),
            cached_providers,
            loop_detector: LoopDetector::new(),
            turn_started: None,
        })
    }

    /// Inject (or refresh) the current goal snapshot into the system prompt so
    /// the model always sees the live plan without it being part of `history`.
    fn refresh_system(&mut self) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut system = build_system_prompt(&cwd);
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

    /// The active security policy (level, apt, network, ask, read, quarantine).
    /// Returns `None` when no guardrail is configured.
    pub fn security_policy(&self) -> Option<crate::security::SecurityPolicy> {
        self.security.as_ref().map(|c| c.policy.clone())
    }

    /// Set the local su-security authorization for this session. Returns the
    /// reason it was recorded at (for audit). `reason` is required when turning
    /// the boundary OFF, because disabling it lets the agent act with the
    /// invoking user's full authority for this session.
    pub fn set_su_security(&mut self, enabled: bool, reason: &str) -> String {
        self.su_security_enabled = enabled;
        // Wire the authority: while su-security is off, bash must NOT drop to
        // `ai_X` (drop_to_agent_user reads this env) so the agent can act as
        // the invoking user (root). The reason is recorded in the response.
        if enabled {
            // SAFETY: edition 2024 marks env mutation unsafe; pir confines
            // it to startup config and explicit session toggles.
            unsafe { std::env::remove_var("PIR_AGENT_AS_INVOKER"); }
        } else {
            // SAFETY: edition 2024 marks env mutation unsafe; pir confines
            // it to startup config and explicit session toggles.
            unsafe { std::env::set_var("PIR_AGENT_AS_INVOKER", "1"); }
        }
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

    /// Load a previously persisted su-security choice (from `<log>.susec`) for
    /// a resumed session. Returns true if a value was restored.
    pub fn apply_persisted_su_security(&mut self) -> bool {
        let Some(p) = self.log_path.as_ref() else { return false };
        match std::fs::read_to_string(p.with_extension("susec")) {
            Ok(s) => {
                self.su_security_enabled = s.trim() == "1";
                if self.su_security_enabled {
                    // SAFETY: edition 2024 marks env mutation unsafe; pir confines
                    // it to startup config and explicit session toggles.
                    unsafe { std::env::remove_var("PIR_AGENT_AS_INVOKER"); }
                } else {
                    // SAFETY: edition 2024 marks env mutation unsafe; pir confines
                    // it to startup config and explicit session toggles.
                    unsafe { std::env::set_var("PIR_AGENT_AS_INVOKER", "1"); }
                }
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
        self.turn_started = Some(std::time::Instant::now());
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
                    }
                let outcome = match self.run_goal_tool(name, input) {
                    Some(o) => o,
                    None => self.registry.execute(name, input),
                };
                if !self.silent() {
                    term::out(&term::dim(&format!("  {}\n", first_line(&outcome.content))));
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
    // `http_backend` settings.json key (`"isahc"` default, `"ureq"`).
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
        for tool in ["read_file", "edit_file", "write_file", "list_dir", "bash", "update_goal"] {
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
}
