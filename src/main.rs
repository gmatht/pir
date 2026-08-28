mod agent;
mod config;
mod goal;
mod notify;
mod plugin;
mod project;
mod provider;
mod session;
mod term;
mod types;
mod user;
#[cfg(feature = "tui")]
mod tui;

// Statically linked extensions, emitted by build.rs (type "a").
include!(concat!(env!("OUT_DIR"), "/gen_registry.rs"));

use crate::agent::Agent;
use crate::config::Provider;
use crate::config::Model;
use crate::notify::SharedBus;
use std::io::BufRead;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

const HELP: &str = r#"pir — a featherweight pi-compatible coding agent

USAGE
  pir [options] [prompt]     prompt given => one-shot, else interactive REPL
  pir -r [token] [prompt]    resume a session (latest from this bash by default)
  pir -c [token] [prompt]    continue a goal: resume a session + drive its next step
  pir -bg <prompt>           run a prompt entirely in the background (notifies on done)

OPTIONS
  -m, --model <selector>     e.g. -m anthropic/claude-sonnet-4-5 (fuzzy match ok)
  -y, --full-auto            no confirmation for shell/write tools
  --confirm                  always prompt to confirm shell/write tools
  -n, --no-color             disable ANSI colors
  -r, --resume [token]       resume a session; token selects by index/time/preview
  -c, --continue [token]     resume a session and continue its goal (pir -c)
  -u, --as <user>            run project commands as this user (default ai_<project>)
  --tui                use the full-screen TUI REPL (requires the `tui` feature)
  --no-tui             use the plain streaming REPL (this is the default build)
  --no-raw             use line-buffered stdin (no raw mode) — for constrained
                       terminals / screen where raw input misbehaves
  --budget <tokens>    optional cumulative in+out token cap; turn stops (with a
                       banner) once exceeded. Off by default. Env: PIR_TOKEN_BUDGET

CONFIG (reused from pi, never modified)
  ~/.pi/models.json          providers, models, api keys ("{env:VAR}" supported)
  ~/.pi/agent/settings.json  optional default model ("defaultModel" / "defaultProvider")
  ~/.pi/AGENTS.md, ./AGENTS.md   appended to the system prompt
  ~/.pi/agent/sessions/      pir session transcripts + goal files (pir-*.jsonl/.goal.json)
  ~/.pi/agent/projects.json  project -> execution-user mappings (set by `pir project init`)

PER-PROJECT USERS
  `pir project init` creates a non-login user ai_<project> owning the cwd so
  all commands run as that user. Re-run as root, or `sudo -u ai_<project> pir`.

AGENT USERS RUN UNATTENDED
  When pir is running as an ai_* user (a per-project/agent sandbox), it
  defaults to full-auto and will NOT prompt to confirm each command — the
  sandbox boundary is the user account itself. Use `pir -c`/`--confirm` or set
  PIR_CONFIRM=1 to force prompts even as an ai_* user.

COMMANDS
  /help  /model <sel>  /models  /default-model <sel>  /sessions  /goal [objective]  /continue
  /bg <text>  /jobs  /fg <id>  /clear  /usage  /exit
  /undo [all]             revert the last file edit (or all) to its pre-edit state
  /project init            create the ai_<project> user and chown the cwd (root)
  /su-security <on|off|status>   enable/disable/inspect the su-based permission
                          model (sudoers.d/skynet-ai + wrappers); reversible (root)
  /fix                     make the .git setup sane for LLM use (install commit
                          guard hook + .gitattributes; jj-aware). Run it if you see
                          the "no commit guard hook" startup warning on an existing repo
  /rebuild                cargo build + exec the fresh binary (unix)
  /create [name]           scaffold a new project (seeds from clipboard .md spec)

  Lines ending in & run in the background: "fix the parser &"  => /bg fix the parser

  While a turn is running you can keep typing: Enter queues the line as the next
  prompt, /commands still work, and a line ending in & is fired off as a new
  background job while the current turn keeps streaming; ESC or ctrl-c cancels
  the running turn instantly (kills any in-flight command right away).
"#;

struct BgSession {
    id: usize,
    prompt: String,
    log: PathBuf,
    started: std::time::SystemTime,
    joined: bool,
    handle: Option<JoinHandle<()>>,
}

/// In-process background sessions. Each is a worker thread driving a `pir`
/// agent over its own session log; the foreground REPL stays free so the user
/// can start more work. Finished jobs keep their log so they can be
/// foregrounded (`/fg`) to reclaim the conversation.
struct BackgroundJobs {
    next_id: usize,
    jobs: Vec<BgSession>,
    /// Whether the interactive foreground turn is currently running. Set by the
    /// REPL so `/jobs` can show it alongside background jobs.
    fg_running: bool,
}

impl BackgroundJobs {
    fn new() -> Self {
        BackgroundJobs { next_id: 1, jobs: Vec::new(), fg_running: false }
    }

    /// Spawn a background task. `builder` constructs the agent (already
    /// configured for the chosen provider/model/flags); it is run on a thread
    /// with `set_quiet(true)` so nothing is printed to the terminal. On
    /// completion the agent's notify hub fires the usual `TurnDone`/`Error`
    /// events (gated by notify policy), so a backgrounded task pings the user
    /// exactly like the foreground one would — but only when they're not
    /// actively watching.
    fn spawn<F>(&mut self, prompt: String, log: PathBuf, builder: F)
    where
        F: FnOnce() -> Agent + Send + 'static,
    {
        let id = self.next_id;
        self.next_id += 1;
        let prompt_for_thread = prompt.clone();
        let handle = thread::spawn(move || {
            let mut agent = builder();
            match agent.turn(&prompt_for_thread) {
                Ok(()) => agent.notify_on_exit(agent.turn_done_event()),
                Err(e) => agent.notify_on_exit(agent.error_event(e)),
            }
        });
        self.jobs.push(BgSession {
            id,
            prompt,
            log: log.clone(),
            started: std::time::SystemTime::now(),
            joined: false,
            handle: Some(handle),
        });
        println!("{} backgrounded as job #{} (logs to {})", term::cyan("·"), id, log.display());
    }

    /// Adopt an *already-running* foreground turn as a background job: take over
    /// its worker handle + its session log so it shows up in `/jobs` and keeps
    /// running to completion (notifications still fire on the shared bus). The
    /// turn's agent must already have been told to go quiet (see
    /// `Agent::request_quiet`) so it stops writing to the terminal. Used by the
    /// `&`-to-background-the-current-turn path. The returned id is what `/fg`
    /// will later reattach to.
    fn attach_fg(&mut self, handle: JoinHandle<()>, log: PathBuf, prompt: String) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        self.jobs.push(BgSession {
            id,
            prompt,
            log,
            started: std::time::SystemTime::now(),
            joined: false,
            handle: Some(handle),
        });
        id
    }

    /// Spawn a background job from a prompt, using the current provider/model/
    /// full-auto captured in `ctx` (so it works even while the foreground turn
    /// owns the agent). A fresh session log is used so the background work
    /// doesn't disturb the interactive history; run with `set_quiet(true)` so
    /// the streaming output goes nowhere on the terminal (notifications fire on
    /// completion instead). This is what both the `&`-suffix path and `/bg`
    /// reach, whether typed at the idle prompt or *mid-turn*.
    fn spawn_prompt(&mut self, prompt: String, ctx: &Arc<Mutex<(Provider, Model, bool)>>, bus: SharedBus) {
        let (provider, model, full_auto) = {
            let g = ctx.lock().unwrap();
            (g.0.clone(), g.1.clone(), g.2)
        };
        let log = session_log_path();
        let bcancel = Arc::new(AtomicBool::new(false));
        self.spawn(prompt, log, move || {
            Agent::new(provider, model, full_auto, true, bus, None, bcancel, Arc::new(Mutex::new(String::new())))
                .expect("bg agent")
        });
    }

    /// Join any finished worker threads so their handles don't leak; returns
    /// the ids that completed during this call.
    fn reap(&mut self) -> Vec<usize> {
        let mut finished = Vec::new();
        for j in self.jobs.iter_mut() {
            if let Some(h) = j.handle.take() {
                if h.is_finished() {
                    let _ = h.join();
                    finished.push(j.id);
                } else {
                    j.handle = Some(h);
                }
            }
        }
        finished
    }

    fn list(&mut self) -> String {
        self.reap();
        if self.jobs.is_empty() {
            return term::dim("(no background jobs)\n").to_string();
        }
        let mut out = String::new();
        out.push_str(&term::bold("background jobs\n"));
        for j in &self.jobs {
            let running = j.handle.is_some();
            let state = if running && !j.joined {
                term::cyan("running").to_string()
            } else {
                term::dim("done").to_string()
            };
            out.push_str(&format!(
                "  #{:<3} [{}] {}\n       log: {}\n",
                j.id,
                state,
                truncate(&j.prompt, 70),
                j.log.display()
            ));
        }
        if self.fg_running {
            out.push_str(&term::bold("  foreground turn: running\n"));
            out.push_str(&term::dim("    (type /cancel or ctrl-c to stop)\n"));
        }
        out.push_str(&term::dim("foreground with: /fg <id>  (reloads that session)\n"));
        out
    }

    fn mark_joined(&mut self, id: usize) {
        if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
            j.joined = true;
        }
    }

    /// Mark whether the foreground interactive turn is currently running, so
    /// `list()` can surface it.
    fn set_fg_running(&mut self, running: bool) {
        self.fg_running = running;
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut model_sel: Option<String> = None;
    let mut full_auto = false;
    let mut force_confirm = false;
    let mut prompt: Vec<String> = Vec::new();
    let mut resume_token: Option<String> = None;
    let mut continue_token: Option<String> = None;
    let mut as_user: Option<String> = None;
    let mut project_name: Option<String> = None;
    let mut bg_prompt: Option<String> = None;
    // The full-screen ratatui REPL is used only when the `tui` feature is
    // compiled in AND `--tui` is passed; otherwise the streaming REPL runs.
    #[cfg(feature = "tui")]
    let mut use_tui = false;
    #[cfg(not(feature = "tui"))]
    let mut use_tui = false;
    let mut no_raw = false;
    let mut budget: Option<u64> = None;

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
            "-u" | "--as" => {
                i += 1;
                match args.get(i) {
                    Some(v) => as_user = Some(v.clone()),
                    None => die("--as needs a value"),
                }
            }
            "project" => {
                // `pir project init` — handle in a subcommand branch below.
                run_project_subcommand(&args[i + 1..]);
                return;
            }
            "-n" | "--no-color" => term::set_color(false),
            "-y" | "--full-auto" => full_auto = true,
            "--confirm" => {
                // Force confirmation prompts even when running as an agent
                // user (ai_*), overriding the full-auto default for them.
                force_confirm = true;
                full_auto = false;
            }
            "-r" | "--resume" => {
                // The next token is the resume token unless it's another flag.
                if let Some(next) = args.get(i + 1) {
                    if !next.starts_with('-') {
                        resume_token = Some(next.clone());
                        i += 1;
                    }
                }
            }
            "-c" | "--continue" => {
                if let Some(next) = args.get(i + 1) {
                    if !next.starts_with('-') {
                        continue_token = Some(next.clone());
                        i += 1;
                    }
                }
            }
            "-bg" | "--background" => {
                i += 1;
                match args.get(i) {
                    Some(v) => bg_prompt = Some(v.clone()),
                    None => die("-bg needs a prompt"),
                }
            }
            "--no-raw" => no_raw = true,
            "--budget" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u64>().ok()) {
                    Some(n) => budget = Some(n),
                    None => die("--budget needs a positive integer (tokens)"),
                }
            }
            "-h" | "--help" => {
                print!("{HELP}");
                return;
            }
            "-V" | "--version" => {
                println!("pir {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--no-tui" => use_tui = false,
            "--tui" => use_tui = true,
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
    term::set_model_providers(&providers);

    // Drop privileges to the per-project user *after* config/providers are
    // loaded but *before* the agent (and any tool) runs. On non-unix this is a
    // no-op. All `bash`/file tools then execute as that user automatically.
    //
    // IMPORTANT: `become_user` rewrites `HOME` to the target (sandbox) user's
    // real home, so `config::pi_dir()` now points at `~<user>/.pi`. We therefore
    // resolve the *default* model **after** this drop (see below): the startup
    // read and the `/default-model` write must consult the same settings file,
    // otherwise the choice is silently lost on restart — the early read used to
    // happen under the invoking user's HOME while the write happened under the
    // (dropped-to) sandbox user's HOME.
    #[cfg(unix)]
    let resolved_user: Option<String> = {
        let target = as_user.clone().unwrap_or_else(|| {
            crate::config::resolve_project_user(None, project_name.as_deref())
        });
        if let Err(e) = crate::user::become_user(&target) {
            die(&e);
        }
        Some(target)
    };
    #[cfg(not(unix))]
    let resolved_user: Option<String> = None;

    // Resolve the default model now that `HOME` reflects the effective
    // (possibly dropped) identity, so the read and the `/default-model` write
    // use the same `~/.pi/agent/settings.json`. Run as root under a per-project
    // user, this is the sandbox user's home; run plainly as a user, it's that
    // user's home.
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

    // Agent users (ai_*) run unattended: the sandbox boundary is the user
    // account itself, so we default to full-auto and suppress per-command
    // confirmation prompts. Honour explicit overrides:
    //   - `pir -y` / `PI_FULL_AUTO=1` forces full-auto,
    //   - `pir --confirm` / `PI_CONFIRM=1` forces confirmation regardless.
    let force_full_auto = full_auto || std::env::var("PI_FULL_AUTO").is_ok();
    let force_confirm_env = std::env::var("PI_CONFIRM").is_ok();
    let running_as_agent = resolved_user
        .as_deref()
        .map(|u| u.starts_with("ai_"))
        .unwrap_or(false);
    if force_confirm || force_confirm_env {
        full_auto = false;
    } else if running_as_agent || force_full_auto {
        full_auto = true;
    }

    // One shared notification bus for the whole process. The foreground agent
    // and every background session publish to it, so the active REPL screen
    // can show notifications from *all* agents (see the REPL loop below).
    let bus: SharedBus = crate::notify::shared_bus();

    // `-c`/`--continue` implies resuming a session and continuing its goal.
    let continue_mode = continue_token.is_some()
        || std::env::args().skip(1).any(|a| a == "-c" || a == "--continue");

    // If `-r`/`--resume` was given (with or without a token), resolve which
    // session to resume. With no token we default to the latest session that
    // came from the same shell (bash) that launched this pir.
    let resume = if resume_token.is_some() || continue_mode
        || std::env::args().skip(1).any(|a| a == "-r" || a == "--resume")
    {
        resolve_resume(resume_token.as_deref())
    } else {
        None
    };

    // Cooperative cancellation flag for the foreground turn. Set by the REPL
    // (ctrl-c) to ask a running turn to stop at the next safe boundary.
    let fg_cancel = Arc::new(AtomicBool::new(false));

    // Shared "go silent" switch for the *current* foreground turn: the REPL
    // flips it (via `Agent::request_quiet`) to *detach* a running turn into the
    // background. Once set, the worker stops streaming to stdout and the
    // terminal returns to the idle prompt while the turn keeps running.
    let fg_quiet: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    // Live "what the user is typing" buffer shown by the thinking spinner while
    // a turn runs (see `run_foreground_turn` / `term::Spinner`). The REPL thread
    // only writes to it; the spinner thread is the sole stdout writer during a
    // turn, so the user's keystrokes appear instead of being clobbered.
    let typeahead: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    let mut agent = match Agent::new(
        provider.clone(),
        model.clone(),
        full_auto,
        false,
        bus.clone(),
        resume.as_ref(),
        fg_cancel.clone(),
        typeahead.clone(),
    ) {
        Ok(a) => a,
        Err(e) => die(&e),
    };

 // Resume prior history if `-r`/`-c` was given.
    if let Some(session) = &resume {
        let (_, summary) = agent.load_session(session);
        if !summary.is_empty() {
            println!("{}", term::dim(&summary));
        }
        // Restore the model + su-security choice that were active when this
        // session last ran, so a resumed session doesn't silently drop back to
        // the global defaults.
        if agent.apply_persisted_model() {
            // (model restored silently; the startup banner below shows it)
        }
        agent.apply_persisted_su_security();
    }

    // Resolve the token budget (off by default). `--budget N` wins; else the
    // PIR_TOKEN_BUDGET env var; else None (unbounded).
    let budget = budget.or_else(|| {
        std::env::var("PIR_TOKEN_BUDGET")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
    });
    agent.set_token_budget(budget);
    term::raw::set_enabled(!no_raw);

    // Continuation mode: attach the goal that lives next to the resumed
    // session and drive it to the next step. The goal file is itself
    // persisted, so this is safe to re-run after an interrupt.
    if continue_mode {
        if let Some(session) = &resume {
            agent.attach_goal(session);
        }
        if agent.goal_snapshot().is_none() {
            eprintln!(
                "{} no goal file next to the resumed session — start one with /goal or run without -c",
                term::yellow("!")
            );
        } else {
            agent.continue_goal();
            return;
        }
    }

    // One-shot background mode: run a single prompt on a worker thread and
    // return immediately (notifications fire when it finishes). The closure
    // rebuilds the agent in the background thread so ownership stays simple.
    if let Some(prompt) = bg_prompt.clone() {
        let mut jobs = BackgroundJobs::new();
        let provider = provider.clone();
        let model = model.clone();
        let bcancel = Arc::new(AtomicBool::new(false));
        jobs.spawn(prompt, agent.log_path.clone().unwrap_or_default(), {
            let bus = bus.clone();
            move || {
                Agent::new(provider, model, full_auto, true, bus, None, bcancel, Arc::new(Mutex::new(String::new())))
                    .expect("agent build in background thread")
            }
        });
        agent.notify_on_exit(agent.idle_event());
        return;
    }
    if let Some(p) = &agent.log_path {
        let hist = p.with_extension("history");
        term::set_history_file(&hist);
    }

    if !prompt.is_empty() {
        match agent.turn(&prompt.join(" ")) {
            Ok(()) => agent.notify_on_exit(agent.turn_done_event()),
            Err(e) => agent.notify_on_exit(agent.error_event(e)),
        }
        return;
    }

    // Full-screen TUI REPL (only when the `tui` feature AND
    // `--tui` is passed; otherwise the streaming REPL above runs). It owns the terminal (alternate
    // screen + raw mode via crossterm) and renders a conversation pane + footer
    // pane (thinking + live draft prompt) with its own scrollback — no
    // hand-rolled ANSI block, so the stray-spinner class of bug can't recur.
    // The agent is switched to `quiet` mode first so its token streaming never
    // reaches the screen (ratatui owns every cursor); the TUI renders the
    // conversation by tailing the agent's session log instead.
    #[cfg(feature = "tui")]
    if use_tui {
        agent.set_quiet(true);
        let agent_slot: Arc<Mutex<Option<Agent>>> = Arc::new(Mutex::new(Some(agent)));
        let (done_tx, _done_rx) = smol::channel::bounded(1);
        match crate::tui::run(
            &agent_slot,
            &fg_cancel,
            &fg_quiet,
            &typeahead,
            &providers,
            &bus,
            &done_tx,
            full_auto,
            running_as_agent,
        ) {
            Ok(()) => return,
            Err(e) => {
                // Fall back to the streaming REPL if the TUI can't start
                // (e.g. not a tty). Take the agent back out of the slot and
                // restore its stdout streaming so the fallback works.
                eprintln!("pir: --tui failed: {e}; falling back to plain REPL");
                agent = agent_slot.lock().unwrap().take().expect("agent present");
                agent.set_quiet(false);
            }
        }
    }

    println!("{}", term::bold("pir"));
    println!(
        "{}",
        term::dim(&format!(
            "model {} · {} · config {}",
            agent.label(),
            if full_auto {
                if running_as_agent {
                    "full-auto (agent user)"
                } else {
                    "full-auto"
                }
            } else {
                "confirm-actions"
            },
            config::pi_dir().display()
        ))
    );
    // Surface extension startup banners (e.g. the worktree extension reporting
    // which worktree we launched in) before the first prompt.
    for line in agent.startup_reports() {
        println!("{}", term::dim(&line));
    }
    // Show the execution user when it isn't the invoking root (per-project
    // `ai_X` sandbox), so it's clear commands run as that identity.
    #[cfg(unix)]
    if let Some(u) = resolved_user.as_deref() {
        println!("{}", term::dim(&format!("running as user {u}")));
    }
    if let Some(p) = &agent.log_path {
        println!("{}", term::dim(&format!("session log: {}", p.display())));
    }
    println!("{}", term::dim("/help for commands · ctrl-d to quit · type while a turn runs; ESC/ctrl-c cancels it instantly"));

    // Warn on existing git projects that lack the LLM-safety guard hook, and
    // point at /fix. Skipped under jj (git hooks don't apply there).
    if crate::project::missing_git_guard(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))) {
        eprintln!(
            "{}",
            term::yellow(&format!(
                "[pir] this git repo has no commit guard hook — agents could commit large/binary files. Run /fix to make the .git setup sane for LLM use."
            ))
        );
    }

    let mut jobs = BackgroundJobs::new();

    // The interactive agent lives behind an Option so a running turn can *take*
    // ownership of it onto a worker thread (the lock is not held during the
    // turn — only the brief take/put around spawning/joining). This keeps the
    // REPL responsive: the model streams on a worker thread while the main
    // thread reads input, drains notifications, and reacts to ctrl-c.
    let agent_slot: Arc<Mutex<Option<Agent>>> = Arc::new(Mutex::new(Some(agent)));

    // Current provider/model/full-auto, mirrored out of the agent so a running
    // foreground turn (which *takes* the agent out of `agent_slot`) can still
    // spawn background jobs — `/bg` or a line ending in `&` typed mid-turn must
    // not panic on a missing agent. Updated on startup, on each `/model`
    // switch, and just before each foreground turn starts.
    let current_ctx: Arc<Mutex<(Provider, Model, bool)>> = {
        let g = agent_slot.lock().unwrap();
        let a = g.as_ref().expect("agent present before REPL");
        Arc::new(Mutex::new((a.provider(), a.model(), full_auto)))
    };

    // Running foreground turn state.
    let mut fg_handle: Option<JoinHandle<()>> = None;
    // Prompts queued by the user while a turn runs (submitted on Enter).
    let mut pending: Vec<String> = Vec::new();
    // Partial line buffer for the raw-mode input while a turn runs.
    let mut input_buf = String::new();
    // `typeahead` (the *same* Arc the agent + spinner thread were built with,
    // declared just before `Agent::new`) is reused here: the REPL thread only
    // ever *writes* to it and the spinner thread *reads* it, so the user's
    // keystrokes appear on the spinner line while the model thinks. Do NOT
    // create a second `typeahead` here — a separate Arc would mean the REPL
    // writes to a buffer the spinner never reads (the original bug).
    // Oneshot signal from the foreground worker: closed when the turn ends, so
    // the REPL's event-driven wait can wake without polling.
    let (done_tx, done_rx) = smol::channel::bounded(1);

    let mut line = String::new();
    loop {
        line.clear();

        // Surface notifications from ALL agents (foreground + background) on the
        // active screen before showing the prompt. Background sessions publish
        // to the shared bus while the main thread is pumping input, so this is
        // where their "done" notifications become visible.
        let feed = bus.drain_feed();
        let rendered = crate::notify::render_feed(&feed);
        if !rendered.is_empty() {
            term::out(&rendered);
        }
        // Reap any finished background jobs so their handles don't leak.
        jobs.reap();

        // If a foreground turn finished, join it and either start the next
        // queued prompt or return to the idle prompt.
        if let Some(h) = fg_handle.as_ref() {
            if h.is_finished() {
                let h = fg_handle.take().unwrap();
                let _ = h.join();
                term::raw::disable_raw();
                // Report token usage from the (now-returned) agent.
                if let Some(a) = agent_slot.lock().unwrap().as_ref() {
                    term::out(&term::dim(&format!(
                        "· {} in / {} out tokens",
                        fmt_tok(a.usage.input),
                        fmt_tok(a.usage.output)
                    )));
                }
                if let Some(next) = pending.drain(..).next() {
                    if let Ok(mut g) = typeahead.lock() { g.clear(); }
                    // A prompt the user typed *mid-turn* (raw mode) won't be in
                    // rustyline's history; record it so arrow-up recalls it later.
                    term::push_history(&next);
                    fg_handle = Some(run_foreground_turn(
                        &agent_slot,
                        &fg_cancel,
                        &fg_quiet,
                        next,
                        done_tx.clone(),
                    ));
                    term::raw::enable_raw();
                } else {
                    // No user-queued prompt: drain any follow-up prompts the
                    // extension backends queued during on_turn_end (e.g. the
                    // worktree extension asking the model to fix failing tests)
                    // and run them before returning to the idle prompt.
                    let follow = {
                        let mut g = agent_slot.lock().unwrap();
                        match g.as_mut() {
                            Some(a) => a.take_continuations(),
                            None => Vec::new(),
                        }
                    };
                    if let Some(next) = follow.into_iter().next() {
                        if let Ok(mut g) = typeahead.lock() { g.clear(); }
                        term::push_history(&next);
                        fg_handle = Some(run_foreground_turn(
                            &agent_slot,
                            &fg_cancel,
                            &fg_quiet,
                            next,
                            done_tx.clone(),
                        ));
                        term::raw::enable_raw();
                    }
                }
            }
        }

        if fg_handle.is_some() {
            // A turn is running on a worker thread: stay responsive. Block
            // (event-driven, ~0% CPU) until stdin is readable OR the worker
            // signals completion via `done_rx`; then drain any typed input.
            // Enter queues the next prompt, ctrl-c requests cancellation,
            // ctrl-d stops the session. The user's keystrokes are recorded into
            // `typeahead` (rendered by the thinking spinner) rather than echoed
            // here, so the two stdout writers never race.
            match term::raw::wait_input(&mut input_buf, &typeahead, &done_rx) {
                term::raw::RawInput::Line(s) => {
                    let s = s.trim();
                    // Clear the typeahead so the spinner line is blank before
                    // the next prompt / queued message is printed.
                    if let Ok(mut g) = typeahead.lock() { g.clear(); }
                    if s.is_empty() {
                        input_buf.clear();
                    } else if s.starts_with('/') {
                        // Slash commands are handled immediately, even mid-turn.
                        input_buf.clear();
                        handle_command(
                            &s[1..],
                            &agent_slot,
                            &providers,
                            &mut jobs,
                            full_auto,
                            &bus,
                            &fg_cancel,
                            true,
                            &current_ctx,
                        );
                    } else if s == "&" {
                        // A bare `&` typed *while a turn runs* detaches the
                        // running foreground turn into the background: flip the
                        // shared "go quiet" switch (the worker stops streaming
                        // to stdout) and adopt its worker handle as a background
                        // job, so the REPL returns to the idle prompt while the
                        // turn keeps running. The only sign of life is
                        // "#tasks running: N · Idle".
                        input_buf.clear();
                        let log = {
                            let g = agent_slot.lock().unwrap();
                            g.as_ref().and_then(|a| a.log_path().cloned()).unwrap_or_default()
                        };
                        let prompt = {
                            let g = agent_slot.lock().unwrap();
                            g.as_ref().map(|a| a.last_prompt.clone()).unwrap_or_default()
                        };
                        let h = fg_handle.take().expect("fg running");
                        let id = jobs.attach_fg(h, log, prompt);
                        // `h` is now owned by `jobs` (kept alive, never joined/dropped here).
                        fg_quiet.store(true, Ordering::SeqCst);
                        term::raw::disable_raw();
                        term::out(&term::dim(&format!(
                            "· detached running turn as job #{} — it keeps working in the background",
                            id
                        )));
                    } else if s.ends_with('&') && !s.trim_end_matches('&').trim().is_empty() {
                        // A prompt line ending in `&` typed *while a turn runs*
                        // starts a brand-new background job (the foreground keeps
                        // streaming). The context is read from `current_ctx`, so
                        // this never panics on the agent being owned by the turn.
                        input_buf.clear();
                        let prompt = s.trim_end_matches('&').trim().to_string();
                        jobs.spawn_prompt(prompt, &current_ctx, bus.clone());
                        term::out(&term::dim("· backgrounded; current turn continues"));
                    } else {
                        pending.push(s.to_string());
                        input_buf.clear();
                        term::out(&term::dim("· queued; will run when current turn ends"));
                    }
                }
                term::raw::RawInput::Interrupt | term::raw::RawInput::Cancel => {
                    if let Ok(mut g) = typeahead.lock() { g.clear(); }
                    fg_cancel.store(true, Ordering::SeqCst);
                    // The running turn aborts immediately: any in-flight bash
                    // command is killed right away, and the model loop stops at
                    // its next safe boundary. The worker then joins and the REPL
                    // returns to a clean idle prompt, ready for a new command.
                    term::out(&term::dim("· cancelling turn (ESC/ctrl-c)…"));
                }
                term::raw::RawInput::Eof => {
                    if let Ok(mut g) = typeahead.lock() { g.clear(); }
                    fg_cancel.store(true, Ordering::SeqCst);
                    // Let the running turn finish its current step, then exit.
                    let _ = fg_handle.take().unwrap().join();
                    term::raw::disable_raw();
                    return;
                }
                term::raw::RawInput::Suspend => {
                    // Pause the whole process (foreground turn + spinner thread
                    // all stop with it) and hand control back to the parent
                    // shell. Drop raw mode + the non-blocking flag first so the
                    // shell is usable while suspended, then suspend; re-enable
                    // raw mode on resume so the next `wait_input` works. The
                    // partial input line is left intact in `input_buf`/`buf`.
                    if let Ok(mut g) = typeahead.lock() { g.clear(); }
                    term::raw::disable_raw();
                    unsafe {
                        libc::raise(libc::SIGTSTP);
                    }
                    term::raw::enable_raw();
                }
                term::raw::RawInput::None => { /* turn finished / no input; re-check loop */ }
            }
            continue;
        }

        // Idle: full rustyline editing. Show a compact tasks indicator so a
        // backgrounded (detached) turn is visible at a glance — the only sign of
        // it is "#tasks running: N · Idle", with N > 0 while it works.
        let tasks_running = jobs.jobs.iter().filter(|j| j.handle.is_some()).count();
        if tasks_running > 0 {
            term::out(&term::dim(&format!("#tasks running: {} · Idle\n", tasks_running)));
        }
        match term::read_line(&format!("{} ", term::cyan("❯"))) {
            None => {
                println!();
                break;
            }
            Some(s) => line = s,
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        // A prompt ending in `&` at the idle prompt runs in its OWN session (a
        // fresh background job that keeps its own session log); the interactive
        // session is untouched. A bare `&` at idle does nothing (there is no
        // running turn to detach).
        let bg = input.ends_with('&') && !input.trim_end_matches('&').trim().is_empty();
        let input = input.trim_end_matches('&').trim();
        if input.is_empty() {
            continue;
        }
        if let Some(cmd) = input.strip_prefix('/') {
            handle_command(cmd, &agent_slot, &providers, &mut jobs, full_auto, &bus, &fg_cancel, false, &current_ctx);
        } else if bg {
            jobs.spawn_prompt(input.to_string(), &current_ctx, bus.clone());
        } else {
            if let Ok(mut g) = typeahead.lock() { g.clear(); }
            // A fresh foreground turn starts un-silenced; reset the detach
            // switch so a previously detached turn's quiet state can't leak.
            fg_quiet.store(false, Ordering::SeqCst);
            fg_handle = Some(run_foreground_turn(
                &agent_slot,
                &fg_cancel,
                &fg_quiet,
                input.to_string(),
                done_tx.clone(),
            ));
            term::raw::enable_raw();
        }
    }
}

/// Spawn the given prompt as a foreground turn on a worker thread, *moving* the
/// agent out of `agent_slot` for the duration (so the lock is never held during
/// the turn), then returning it when the turn completes. Resets the cancel flag
/// at the start and publishes an Idle/Error event when done. `done` is a oneshot
/// the REPL awaits, so it can wake the moment the turn ends instead of polling.
/// `typeahead` is not needed here: the agent already holds the shared buffer
/// (the same Arc the REPL thread writes to), and the thinking spinner reads it
/// directly from the agent (see [`crate::term::Spinner`]). `quiet_handle` is the
/// REPL's shared "go silent" switch: the REPL flips it (via `Agent::request_quiet`)
/// to *detach* a running turn into the background — once set, the worker stops
/// streaming to stdout and the terminal returns to the idle prompt while the
/// turn keeps running.
pub(crate) fn run_foreground_turn(
    agent_slot: &Arc<Mutex<Option<Agent>>>,
    cancel: &Arc<AtomicBool>,
    quiet_handle: &Arc<AtomicBool>,
    prompt: String,
    done: smol::channel::Sender<()>,
) -> JoinHandle<()> {
    let slot = agent_slot.clone();
    let cancel = cancel.clone();
    let quiet_handle = quiet_handle.clone();
    thread::spawn(move || {
        cancel.store(false, Ordering::SeqCst);
        let mut a = slot.lock().unwrap().take().expect("agent present");
        // Hand the worker the shared quiet switch so the REPL can detach this
        // turn to the background mid-flight without owning the agent.
        a.set_quiet_handle(quiet_handle);
        let ev = match a.turn(&prompt) {
            Ok(()) => a.idle_event(),
            Err(e) => a.error_event(e),
        };
        a.notify_on_exit(ev);
        *slot.lock().unwrap() = Some(a);
        // Wake the REPL's event-driven wait so it joins this handle immediately.
        let _ = done.try_send(());
    })
}

/// A fresh session log path for a background job (tagged so it never collides
/// with the foreground session or another job).
fn session_log_path() -> PathBuf {
    let dir = config::pi_dir().join("agent").join("sessions");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("pir-{}-sh{}-bg{}.jsonl", term::timestamp_compact(), term::parent_shell_pid(), std::process::id()))
}

type AgentSlot = Arc<Mutex<Option<Agent>>>;

/// Handle a slash command. `agent_slot` holds the interactive agent behind an
/// `Option` so a running foreground turn can own it on its worker thread;
/// commands that need the agent take it only briefly. `jobs` surfaces
/// background work and shows whether the foreground turn is running. `fg_running`
/// is the truthful state (the REPL owns it via `fg_handle.is_some()`), so
/// mid-turn `/cancel`/`/jobs` work even though the agent is *taken* out of the
/// slot by the running worker.
fn handle_command(
    cmd: &str,
    agent_slot: &AgentSlot,
    providers: &[Provider],
    jobs: &mut BackgroundJobs,
    full_auto: bool,
    bus: &SharedBus,
    cancel: &Arc<AtomicBool>,
    fg_running: bool,
    current_ctx: &Arc<Mutex<(Provider, Model, bool)>>,
) {
    let mut parts = cmd.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    match cmd {
        "cancel" | "c" => {
            // Advisory stop: set the same cooperative cancel flag ctrl-c sets
            // (the REPL owns it). The running worker turn observes it at its
            // next safe boundary and stops after the current step completes.
            // `fg_running` (passed by the REPL) is the source of truth — the
            // worker *takes* the agent out of `agent_slot` while running, so
            // inspecting the slot would always report "no turn".
            if !fg_running {
                eprintln!("pir: no turn running to cancel (idle)");
            } else {
                cancel.store(true, Ordering::SeqCst);
                println!("{} requesting cancel (turn stops now)", term::dim("·"));
            }
        }
        "h" | "help" => print!("{HELP}"),
        "m" | "model" => {
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                eprintln!("pir: agent busy (turn running) — try again when idle");
                return;
            };
            if rest.is_empty() {
                // No argument: show current model and explain how to make a
                // choice the *default* for future sessions (the first time a
                // user picks a model, nothing is persisted globally).
                let label = agent.label();
                println!("current model: {}", label);
                println!(
                    "{}",
                    term::dim(&format!(
                        "to use this by default in new sessions, add to {}:\n  {}",
                        config::pi_dir().join("agent").join("settings.json").display(),
                        format!("{{ \"defaultModel\": \"{}\" }}", label)
                    ))
                );
                println!(
                    "{}",
                    term::dim("(or just run `/model <sel>` again later — it's saved per-session and restored on resume)")
                );
            } else {
                match config::select(providers, &rest.join(" ")) {
                    Ok((p, m)) => match agent.switch(p.clone(), m.clone()) {
                        Ok(()) => {
                            // Keep the shared background-job context in sync so
                            // any `/bg` or `&` fired afterwards uses the new model.
                            if let Ok(mut ctx) = current_ctx.lock() {
                                ctx.0 = p.clone();
                                ctx.1 = m.clone();
                            }
                            println!("→ {}", agent.label());
                            println!("{} (saved for this session; restored on resume)", term::dim("·"));
                            println!(
                                "{}",
                                term::dim(&format!(
                                    "to use this by default in new sessions, add to {}:\n  {}",
                                    config::pi_dir().join("agent").join("settings.json").display(),
                                    format!("{{ \"defaultModel\": \"{}\" }}", agent.label())
                                ))
                            );
                        }
                        Err(e) => eprintln!("{e}"),
                    },
                    Err(e) => eprintln!("{e}"),
                }
            }
        }
        "models" => print!("{}", list_models(providers)),
        "dm" | "default-model" | "model-default" => {
            // Set the default model for *new* sessions by persisting it to
            // ~/.pi/agent/settings.json. With no argument, just show the
            // current default (if any).
            if rest.is_empty() {
                match config::default_model_setting() {
                    Some(d) => println!("default model (new sessions): {}", d),
                    None => println!("{} no default model set", term::dim("·")),
                }
                return;
            }
            match config::select(providers, &rest.join(" ")) {
                Ok((p, m)) => match config::set_default_model(&p.pid(), &m.id) {
                    Ok(path) => println!("→ default model set to {} (saved in {})", p.label(m), path.display()),
                    Err(e) => eprintln!("pir: {e}"),
                },
                Err(e) => eprintln!("{e}"),
            }
        }
        "sessions" => print!("{}", list_sessions()),
        "bg" => {
            let prompt: String = rest.join(" ");
            if prompt.trim().is_empty() {
                eprintln!("usage: /bg <prompt>  (or end a line with &)");
            } else {
                jobs.spawn_prompt(prompt, &current_ctx, bus.clone());
            }
        }
        "jobs" | "background" | "running" => {
            // `fg_running` is the real state: the worker *takes* the agent out of
            // the slot while a turn runs, so the slot alone can't tell us.
            jobs.set_fg_running(fg_running);
            print!("{}", jobs.list());
        }
        "fg" | "foreground" => {
            let id = match rest.first().and_then(|s| s.parse::<usize>().ok()) {
                Some(id) => id,
                None => {
                    eprintln!("usage: /fg <job-id>  (see /jobs)");
                    return;
                }
            };
            let Some(log) = jobs.jobs.iter().find(|j| j.id == id).map(|j| j.log.clone()) else {
                eprintln!("pir: no background job #{id}");
                return;
            };
            // Foreground = reload that job's session into this agent and hand
            // control back to the user. The background thread has its own copy
            // of the history; reloading the transcript reconciles them.
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                eprintln!("pir: agent busy (turn running) — cancel it first");
                return;
            };
            agent.clear();
            agent.load_session(&log);
            agent.apply_persisted_su_security();
            jobs.mark_joined(id);
            println!("{} foregrounded job #{} from {}", term::bold("·"), id, log.display());
        }
        "unfin" | "unfinished" => {
            // Show sessions that were interrupted, crashed (process gone but
            // turn never finished), or still have a goal in progress — and which
            // no live process is currently driving.
            print!("{}", session::list_unfinished());
        }
        "resume" | "res" => {
            // Resume an unfinished session (from `/unfinished`, index 0 = newest)
            // into the *current* interactive agent and, if a goal is attached,
            // drive it to the next pending step. Nothing is actively mutating the
            // chosen session because `scan_unfinished` only returns sessions with
            // no live client. The current session's history is replaced.
            let token: String = rest.join(" ");
            if token.trim().is_empty() {
                eprintln!("usage: /resume <index|path-fragment>   (see /unfinished)");
                return;
            }
            let Some(path) = session::resolve_unfinished(&token) else {
                eprintln!("pir: no unfinished session matches '{token}'");
                return;
            };
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                eprintln!("pir: agent busy (turn running) — cancel it first");
                return;
            };
            agent.clear();
            let (_, summary) = agent.load_session(&path);
            if !summary.is_empty() {
                println!("{}", term::dim(&summary));
            }
            agent.apply_persisted_model();
            agent.apply_persisted_su_security();
            if agent.goal_snapshot().is_some() {
                agent.attach_goal(&path);
                let out = agent.continue_goal();
                term::out(&out);
            }
        }
        "project" => {
            if rest.is_empty() || rest[0] == "init" {
                run_project_subcommand(&rest.iter().map(|s| s.to_string()).collect::<Vec<_>>());
            } else {
                eprintln!("unknown /project subcommand '{}' — try /project init", rest[0]);
            }
        }
        "su-security" | "susec" => {
            // Local, per-session authority toggle. When enabled (default) the
            // agent stays confined to its sandbox identity; when disabled the
            // agent is authorized to act with the *invoking user's full
            // authority* for THIS session only. This is a self-imposed, in-
            // session authorization flag — it never edits any system file
            // (no sudoers/wrappers are touched) and has no effect on anything
            // other than this agent's own authorization. Persisted per-session
            // so a resumed session keeps its choice.
            let arg = rest.first().copied().unwrap_or("status");
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                eprintln!("pir: agent busy (turn running) — try again when idle");
                return;
            };
            match arg {
                "on" | "enable" | "1" => {
                    println!("{}", agent.set_su_security(true, ""));
                }
                "off" | "disable" | "0" => {
                    // Disabling widens this session's authority to the invoking
                    // user's full authority, so a reason is required. `status`
                    // (and the lack of an explicit `off`) is non-destructive.
                    let reason = rest.get(1..).map(|r| r.join(" ")).unwrap_or_default();
                    if reason.trim().is_empty() {
                        eprintln!(
                            "usage: /su-security off <reason>  — disabling requires a reason\n\
                             \x20\x20e.g. /su-security off need to install system packages"
                        );
                        return;
                    }
                    println!("{}", agent.set_su_security(false, &reason));
                }
                "status" | "state" => {
                    if agent.su_security_enabled() {
                        println!("{}", term::bold("su-based security: ENABLED (agent confined to its sandbox identity)"));
                    } else {
                        println!(
                            "{}",
                            term::bold(" security: DISABLED (agent authorized with the invoking user's full authority, this session only)")
                        );
                    }
                    println!("{}", term::dim("(local per-session flag; no system-wide configuration changed)"));
                }
                other => eprintln!(
                    "usage: /su-security <on|off|status>   (off requires a reason; local to this session)"
                ),
            }
        }
        "create" => {
            let name: String = rest.join(" ");
            create_project(&name);
        }
        "goal" => {
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                eprintln!("pir: agent busy (turn running) — try again when idle");
                return;
            };
            let obj: String = rest.join(" ");
            if obj.trim().is_empty() {
                agent.show_goal();
            } else {
                agent.start_goal(&obj);
                println!("goal started: {}", obj);
            }
        }
        "continue" | "cont" => {
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                eprintln!("pir: agent busy (turn running) — try again when idle");
                return;
            };
            let lp = agent.log_path.clone();
            if let Some(p) = lp {
                agent.attach_goal(&p);
            }
            agent.continue_goal();
        }
        "clear" => {
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                eprintln!("pir: agent busy (turn running) — try again when idle");
                return;
            };
            agent.clear();
            println!("history cleared");
        }
        "fix" => {
            let repo = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            if !crate::project::is_git_repo(&repo) && crate::project::detect_vcs(&repo) != crate::project::Vcs::Jj {
                eprintln!("pir: not inside a git/jj repo");
                return;
            }
            println!("{}", crate::project::fix_git_setup(&repo));
        }
        "rebuild" => {
            // Recompile from source and, on success, replace this process with the
            // freshly built binary (so you pick up the new code without leaving a
            // stale agent running). Build failures print the tail and leave the
            // current session intact.
            rebuild_and_exec();
        }
        "usage" => {
            let g = agent_slot.lock().unwrap();
            match g.as_ref() {
                Some(agent) => println!(
                    "{} in / {} out tokens this session",
                    fmt_tok(agent.usage.input),
                    fmt_tok(agent.usage.output)
                ),
                None => eprintln!("pir: agent busy (turn running) — try again when idle"),
            }
        }
        "undo" => {
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                eprintln!("pir: agent busy (turn running) — try again when idle");
                return;
            };
            let all = rest.first().map(|s| *s == "all").unwrap_or(false);
            println!("{}", agent.undo(all));
        }
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

fn list_sessions() -> String {
    let my_pid = term::parent_shell_pid();
    let mut sessions = match scan_sessions() {
        Some(s) => s,
        None => return term::dim("(no session log directory found)\n").to_string(),
    };
    if sessions.is_empty() {
        return term::dim("(no sessions yet)\n").to_string();
    }
    let mut out = String::new();
    out.push_str(&format!(
        "{} (highlighted = from this shell, sh{})\n",
        term::bold("sessions"),
        my_pid
    ));
    // Newest first.
    sessions.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    for (idx, s) in sessions.iter().enumerate() {
        let from_here = s.shell_pid == my_pid;
        let marker = if from_here { term::cyan("▸") } else { " ".to_string() };
        let name = s.name.replace("pir-", "").replace(".jsonl", "");
        let tag = format!("[{}]", s.shell_pid);
        let tag_s = if from_here { term::cyan(&tag) } else { term::dim(&tag).to_string() };
        let line = format!(
            "  {} {:>2}  {}  {}  {}",
            marker,
            idx,
            term::dim(&name),
            tag_s,
            term::dim(&truncate(&s.preview, 60)),
        );
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(&term::dim(
        "resume with: pir -r <idx|time|preview>  (omit token => latest from this shell)\n",
    ));
    out
}

/// Find the session file for a `-r` token: an index from `/sessions`, a
/// fragment of the timestamp, or a fragment of the first-line preview.
/// With no token, default to the latest session from this shell (bash).
fn resolve_resume(token: Option<&str>) -> Option<PathBuf> {
    let mut sessions = scan_sessions()?;
    if sessions.is_empty() {
        eprintln!("pir: no sessions to resume");
        return None;
    }
    sessions.sort_by(|a, b| b.mtime.cmp(&a.mtime));

    let chosen = match token {
        None => sessions.iter().find(|s| s.shell_pid == term::parent_shell_pid()),
        Some(t) => {
            // Index (most recent = 0)?
            if let Ok(n) = t.parse::<usize>() {
                sessions.get(n)
            } else {
                let tl = t.to_lowercase();
                sessions
                    .iter()
                    .find(|s| s.name.to_lowercase().contains(&tl) || s.preview.to_lowercase().contains(&tl))
            }
        }
    };

    match chosen {
        Some(s) => Some(s.path.clone()),
        None => {
            if token.is_none() {
                eprintln!("pir: no session from this shell yet; use `pir -r <idx>` — see `pir -r` list");
                eprintln!("{}", list_sessions());
            } else {
                eprintln!("pir: no session matches '{token:?}'");
            }
            None
        }
    }
}

struct Session {
    path: PathBuf,
    name: String,
    shell_pid: u32,
    mtime: std::time::SystemTime,
    preview: String,
}

fn scan_sessions() -> Option<Vec<Session>> {
    use std::fs;

    let dir = config::pi_dir().join("agent").join("sessions");
    let entries = fs::read_dir(&dir).ok()?;
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        // pir-<timestamp>-sh<pid>.jsonl
        let shell_pid = name
            .rsplit("sh")
            .next()
            .and_then(|s| s.trim_end_matches(".jsonl").trim().parse::<u32>().ok())
            .unwrap_or(0);
        let mtime = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
        let preview = first_user_line(&path);
        out.push(Session { path, name, shell_pid, mtime, preview });
    }
    Some(out)
}

fn first_user_line(path: &PathBuf) -> String {
    if let Ok(f) = std::fs::File::open(path) {
        for line in std::io::BufReader::new(f).lines().flatten() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                if v.get("role").and_then(|r| r.as_str()) == Some("user") {
                    if let Some(txt) = v
                        .get("blocks")
                        .and_then(|b| b.as_array())
                        .and_then(|a| a.iter().find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text")))
                        .and_then(|b| b.get("text").and_then(|t| t.as_str()))
                    {
                        return truncate(txt.lines().next().unwrap_or("").trim(), 80);
                    }
                }
            }
        }
    }
    String::new()
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>())
    }
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

/// `pir project init [--name X] [--path P]` — create the per-project execution
/// user and chown the project directory (must run as root).
#[cfg(unix)]
fn run_project_subcommand(rest: &[String]) {
        let mut name: Option<String> = None;
    let mut path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--name" => name = it.next().cloned(),
            "--path" => {
                if let Some(p) = it.next() {
                    path = PathBuf::from(p);
                }
            }
            other if other.starts_with("--name=") => name = Some(other["--name=".len()..].to_string()),
            other if other.starts_with("--path=") => path = PathBuf::from(&other["--path=".len()..]),
            "init" => {}
            other => eprintln!("pir project: ignoring unknown arg '{other}'"),
        }
    }
    let cwd_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".into());
    let project = name.clone().unwrap_or(cwd_name);
    let user = format!("ai_{project}");
    match crate::user::provision(&project, &user, &path) {
        Ok(msg) => {
            println!("{msg}");
            println!("now run: sudo -u {user} pir …");
        }
        Err(e) => die(&e),
    }
}

#[cfg(not(unix))]
fn run_project_subcommand(_rest: &[String]) {
    die("per-project users are only supported on unix");
}

/// `/rebuild` — `cargo build` (debug, honoring the lockfile) and, if it
/// succeeds, replace this process with the freshly built binary via `exec`. On
/// a build failure we print the tail of the output and stay in the running
/// session. Unix-only: `exec` replaces the process image in place, so the new
/// `pir` inherits the same stdio/terminal and keeps the user's place.
fn rebuild_and_exec() {
    eprintln!("{} rebuilding…", term::dim("·"));
    let output = std::process::Command::new(env!("CARGO"))
        .args(["build"])
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("./target/debug/pir"));
            eprintln!("\x1b[32m✓\x1b[0m built {}; restarting…", bin.display());
            // Re-exec the new binary with the same args the user originally gave.
            // `exec` does not return on success.
            let err = std::process::Command::new(&bin).args(std::env::args().skip(1)).exec();
            // Only reached if exec fails.
            die(&format!("rebuild: failed to restart {}: {}", bin.display(), err));
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            let tail: String = stderr.lines().rev().take(25).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
            eprintln!("{} build failed:\n{}", term::red("error:"), tail);
        }
        Err(e) => {
            eprintln!("{} could not run cargo build: {}", term::red("error:"), e);
        }
    }
}

#[cfg(not(unix))]
fn rebuild_and_exec() {
    eprintln!("pir: /rebuild (exec) is only supported on unix");
}

/// `/create [name]` — scaffold a new project directory under `PIR_PROJECTS_DIR`
/// (default `~/.pi/projects`). If the system clipboard holds a project markdown
/// spec (the `unmd2.sh` format of `### path` headers + ``` code blocks), offer
/// to extract it into the new project.
fn create_project(name: &str) -> Option<std::path::PathBuf> {
    
    let name = if name.trim().is_empty() {
        let suggested = std::env::current_dir()
            .ok()
            .and_then(|c| c.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "new-project".into());
        let ans = term::read_answer(&format!("project name [{}]:", suggested));
        let trimmed = ans.trim();
        if trimmed.is_empty() { suggested } else { trimmed.to_string() }
    } else {
        name.trim().to_string()
    };

    let base = config::projects_dir();
    let dir = base.join(&name);
    if dir.exists() {
        eprintln!("pir: {} already exists", dir.display());
        return None;
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("pir: cannot create {}: {e}", dir.display());
        return None;
    }
    println!("created project dir {}", dir.display());

    // Offer to seed the project from clipboard markdown (unmd2.sh format).
    if let Some(text) = crate::project::read_clipboard() {
        if crate::project::looks_like_project_md(&text) {
            let n = crate::project::count_md_files(&text);
            let ans = term::read_answer(&format!(
                "clipboard looks like a {}‑file project spec — extract it here? [y]es / [n]o",
                n
            ));
            if ans == "y" || ans == "yes" || ans.is_empty() {
                match crate::project::scaffold_from_md(&dir, &text) {
                    Ok(written) => println!("extracted {} file(s) into {}", written, dir.display()),
                    Err(e) => eprintln!("pir: scaffold failed: {e}"),
                }
            }
        }
    }

    println!("open it with:  cd {}", dir.display());
    Some(dir)
}
