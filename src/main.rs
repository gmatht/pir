mod agent;
mod config;
mod goal;
mod notify;
mod picker;
mod plugin;
mod project;
mod provider;
mod session;
mod term;
mod types;
mod user;
#[cfg(feature = "tui")]
mod tui;
#[cfg(feature = "gui")]
mod gui;

// Statically linked extensions, emitted by build.rs (type "a").
include!(concat!(env!("OUT_DIR"), "/gen_registry.rs"));

use crate::agent::Agent;
use crate::config::Provider;
use crate::config::Model;
use crate::notify::SharedBus;
use std::io::BufRead;
use std::io::IsTerminal;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Death-provenance + child reaping (items 4, 7).
//
// Two historical "all pir processes died at once" incidents were caused by an
// agent running a scoped `pkill`/`kill` (handled now by the bash-tool guard),
// but there was *no log of what sent the kill*. We install a signal handler
// that, on SIGTERUP/SIGINT, appends a tiny provenance note (signal
// number + parent pid + build stamp) to this session's `.status.json` sidecar
// so the cause is reconstructable from the logs later. We also set
// PR_SET_PDEATHSIG so a dead parent (e.g. a closed tmux pane) delivers SIGHUP
// to us, and we reap any spawned command process groups on exit (item 4: a
// `pir` that dies no longer leaves its `bash` children orphaned to init).
// ---------------------------------------------------------------------------

/// Path to the active session's `.status.json`, set once `agent` is built.
/// The signal handler reads it (under the lock) to know where to log.
static ACTIVE_STATUS: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);

/// Pid of the most recently spawned command's process group, so we can reap it
/// on exit. Set by the bash tool via [`set_active_child_pgid`]; read on exit.
static ACTIVE_CHILD_PGID: Mutex<Option<i32>> = Mutex::new(None);

#[cfg(unix)]
pub fn set_active_child_pgid(pgid: i32) {
    *ACTIVE_CHILD_PGID.lock().unwrap() = Some(pgid);
}

/// Install the death-provenance signal handler and PR_SET_PDEATHSIG. Best-
/// effort: any failure is silently ignored (we must never refuse to start).
fn install_death_tracking() {
    #[cfg(unix)]
    {
        // If our parent (the tmux pane / shell) dies, ask the kernel to send us
        // SIGHUP so we tear down cleanly instead of lingering.
        unsafe {
            let _ = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGHUP);
        }

        extern "C" fn handler(signo: libc::c_int) {
            // Build a one-line provenance note and append it to the status file.
            let ppid = unsafe { libc::getppid() };
            let self_pid = std::process::id();
            let note = format!(
                "[death] received signal {signo} (SIGHUP={}) at {} from ppid={}; pir pid={}\n",
                libc::SIGHUP,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                ppid,
                self_pid,
            );
            // Reap any spawned command group so our children don't outlive us.
            if let Some(pgid) = *ACTIVE_CHILD_PGID.lock().unwrap() {
                unsafe { let _ = libc::kill(-pgid, libc::SIGKILL); }
            }
            // Append to the active status sidecar if there is one. We never
            // create or truncate it — only append — so a real status write
            // (which happens via session::write_status) is preserved.
            if let Some(path) = ACTIVE_STATUS.lock().unwrap().clone() {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                    let _ = f.write_all(note.as_bytes());
                }
            }
            // Default disposition: re-raise so the process actually terminates.
            unsafe {
                libc::signal(signo, libc::SIG_DFL);
                libc::raise(signo);
            }
        }

        for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
            unsafe {
                if libc::signal(sig, handler as libc::sighandler_t) == libc::SIG_ERR {
                    // ignore — handler install failed for this signal
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ACTIVE_STATUS; // keep the static referenced on non-unix
    }
}

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
  --gui                use the graphical GTK REPL (requires the `gui` feature)
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
  When the sandbox user can't reach the working directory (e.g. a parent dir
  is another user's 0700 home), pir offers a wizard: move/clone the project
  into the user's home, or skip the privilege drop entirely (no sandbox).

AGENT USERS RUN UNATTENDED
  When pir is running as an ai_* user (a per-project/agent sandbox), it
  defaults to full-auto and will NOT prompt to confirm each command — the
  sandbox boundary is the user account itself. Use `pir -c`/`--confirm` or set
  PIR_CONFIRM=1 to force prompts even as an ai_* user.

COMMANDS
  /help  /model <sel>  /models  /default-model <sel>  /sessions  /goal [objective]  /continue
  /thinking [<level>] [show|hide]   set the model's thinking level
                          (off|minimal|low|medium|high|xhigh|max) and/or toggle
                          whether streamed reasoning is displayed; no arg = status
  /model* <sel>  /model-all <sel>   broadcast a model switch to ALL your open pir terminals (also sets the new default)
  /bg <text>  /jobs  /fg <id>  /clear  /usage  /exit
  /undo [all]             revert the last file edit (or all) to its pre-edit state
  /sh [cmd args]         drop to a shell, or run a command via $SHELL (sh -c)
  /project init            create the ai_<project> user and chown the cwd (root)
  /su-security <on|off|status>   enable/disable/inspect the su-based permission
                          model (sudoers.d/skynet-ai + wrappers); reversible (root)
  /fix                     make the .git setup sane for LLM use (install commit
                          guard hook + .gitattributes; jj-aware). Run it if you see
                          the "no commit guard hook" startup warning on an existing repo
  /rebuild                cargo build + exec the fresh binary (unix)
  /create [name]           scaffold a new project (seeds from clipboard .md spec)
  /login [provider]        store an API key for a provider in ~/.pi/agent/auth.json
  /logout [provider]       remove a stored provider credential from auth.json

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
    /// True only for the job adopted via `attach_fg`: a turn the user
    /// backgrounded (bare `&`) *while it was running*. Such a job still owns the
    /// single interactive `Agent` (it's parked in `None` in `agent_slot` until
    /// the job finishes and returns it), so a new foreground turn must join this
    /// job before it can take the agent. Plain `/bg` jobs spin up their own
    /// agent and never touch the main slot.
    owns_main_agent: bool,
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
            owns_main_agent: false,
        });
        println!("{} backgrounded as job #{} (logs to {})", term::cyan("·"), id, log.display());
    }

    /// Adopt an *already-running* foreground turn as a background job: take over
    /// its worker handle + its session log so it shows up in `/jobs` and keeps
    /// running to completion (notifications still fire on the shared bus). The
    /// turn's agent must already have been told to go quiet (see
    /// `Agent::request_quiet`) so it stops writing to the terminal. Used by the
    /// `&`-to-background-the-current-turn path. The returned id is what `/fg`
    /// will later reattach to. This is the *only* kind of job that owns the
    /// single interactive `Agent` — so a subsequent foreground turn must join
    /// it (see [`BackgroundJobs::reclaim_main_agent`]) before taking the agent.
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
            owns_main_agent: true,
        });
        id
    }

    /// If any job still owns the main interactive `Agent` (a turn the user
    /// detached with a bare `&` and which hasn't finished yet), join it so its
    /// worker returns the agent into `agent_slot` before a new foreground turn
    /// tries to take it. Without this, starting a fresh prompt after
    /// backgrounding a running turn hit `.take().expect("agent present")` on an
    /// empty slot and panicked. Safe to call when idle (no-op if nothing holds
    /// the agent); returns once the agent is back in its slot.
    fn reclaim_main_agent(&mut self, agent_slot: &Arc<Mutex<Option<Agent>>>) {
        // Find the (at most one) job that owns the main agent and is still
        // running. Finished attach_fg jobs have already returned the agent to
        // the slot inside `run_foreground_turn`, so only a live one is blocking.
        let live = self
            .jobs
            .iter()
            .position(|j| j.owns_main_agent && j.handle.is_some());
        if let Some(pos) = live {
            let h = self.jobs[pos].handle.take().expect("live job has a handle");
            let _ = h.join();
            // The worker put the agent back into the slot; nothing else to do.
            // Defensively ensure the slot isn't still empty (it shouldn't be).
            let _ = agent_slot;
            self.jobs.remove(pos);
        }
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
    // The graphical GTK REPL is used only when the `gui` feature is compiled in
    // AND `--gui` is passed.
    #[cfg(feature = "gui")]
    let mut use_gui = false;
    #[cfg(not(feature = "gui"))]
    let mut use_gui = false;
    let mut no_raw = false;
    let mut budget: Option<u64> = None;

    // Capture the invoking user's default-model selector BEFORE the privilege
    // drop (while HOME still points at the real user's ~/.pi). After the drop,
    // settings.json would come from the sandbox user, whose catalog is a
    // different (smaller) store — using it to resolve against the invoking
    // user's catalog produced "no model matches" fallbacks.
    let pre_drop_selector = std::env::var("PI_MODEL").ok().or_else(config::default_model_setting);

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
            "--abi" => {
                print_abi();
                return;
            }
            "-V" | "--version" => {
                println!("pir {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--no-tui" => use_tui = false,
            "--tui" => use_tui = true,
            "--gui" => use_gui = true,
            x if x.starts_with('-') => die(&format!("unknown flag {x} — try --help")),
            x => prompt.push(x.to_string()),
        }
        i += 1;
    }

    // Load providers as the INVOKING user (root), BEFORE the privilege drop —
    // the 313KB model catalog lives in the real user's ~/.pi and sandbox users
    // can't read it. Settings.json (the *selector*) is resolved after the drop
    // from the sandbox identity. To keep the selector resolvable against the
    // catalog, `config::select` is never handed a selector that only exists in
    // the sandbox's own settings: we resolve with the invoking-user catalog and
    // only fall back to the sandbox default when the user gave no selector at
    // all (see below — sandbox settings are consulted but matched leniently).
    // Install death-provenance signal tracking + PR_SET_PDEATHSIG early, so we
    // can record *what* terminates us (item: SIGTERM/SIGHUP provenance in the
    // status sidecar) and reap spawned children if the parent pane dies.
    install_death_tracking();

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
        // Before dropping, check the sandbox user can actually reach the cwd.
        // If not, offer a wizard to relocate/clone the project (or skip the
        // drop entirely) so the agent doesn't later silently fail to read files
        // because a parent dir (e.g. another user's 0700 home) is unreadable.
        let wizard = crate::user::cwd_accessibility_wizard(&target);
        let mut skip_drop = false;
        match wizard {
            Ok(crate::user::AccessibilityAction::Relocated(dest)) => {
                // Re-root the process at the relocated/clone copy before the
                // drop, so the agent's cwd is one the sandbox user owns.
                if let Err(e) = std::env::set_current_dir(&dest) {
                    eprintln!("pir: could not chdir to {} ({}); dropping anyway", dest.display(), e);
                }
            }
            Ok(crate::user::AccessibilityAction::SkipDrop) => {
                skip_drop = true;
            }
            Ok(crate::user::AccessibilityAction::Proceed) => {}
            Err(e) => {
                eprintln!("pir: accessibility check skipped ({e}); dropping anyway");
            }
        }
        if skip_drop {
            // Honour the user's choice not to sandbox: run as the invoking user.
            None
        } else {
            if let Err(e) = crate::user::become_user(&target) {
                die(&e);
            }
            Some(target)
        }
    };
    #[cfg(not(unix))]
    let resolved_user: Option<String> = None;

    // Resolve the model. Priority: explicit -m/PI_MODEL on the INVOKING
    // user's command line, then the invoking user's settings.json (captured in
    // `pre_drop_selector` before HOME changed), then the first catalog model.
    // The sandbox user's own settings.json is NOT used to pick a model that
    // must resolve against the invoking user's catalog: its catalog (e.g.
    // ai_pir's tiny local/fake store) is a subset, and a sandbox-only selector
    // could never match here. `default_model_setting()` is re-read after the
    // drop only for the /default-model WRITE path below.
    let explicit = model_sel.is_some();
    let selector = model_sel
        .or(pre_drop_selector)
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

    // Point the line editor's history file at the session's `.history` so the
    // per-session prompt history (and the prompts we seed below when resuming)
    // is persisted and recalled with arrow-up. Must run before the resume block
    // so `push_history` during `-r`/`/fg` actually lands in this file.
    if let Some(p) = &agent.log_path {
        let hist = p.with_extension("history");
        term::set_history_file(&hist);
    }

 // Resume prior history if `-r`/`-c` was given.
    if let Some(session) = &resume {
        let resumed = agent.load_session(session);
        if resumed.turns > 0 {
            println!("{}", resumed.banner(session));
            // Seed arrow-up history with the session's prior prompts so the user
            // can scroll back through them at the idle prompt.
            for p in &resumed.prompts {
                term::push_history(p);
            }
        } else if !resumed.summary.is_empty() {
            println!("{}", term::dim(&resumed.summary));
        }
        // Restore the model + su-security + thinking choice that were active
        // when this session last ran, so a resumed session doesn't silently
        // drop back to the global defaults.
        if agent.apply_persisted_model() {
            // (model restored silently; the startup banner below shows it)
        }
        agent.apply_persisted_su_security();
        agent.apply_persisted_thinking();
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

    // Graphical GTK REPL (only when the `gui` feature AND `--gui` is passed).
    // The agent is switched to `quiet` mode (it streams only to its session log)
    // and the GUI renders the conversation by draining that log — same
    // separation the TUI uses. Falls back to the streaming REPL if the GTK
    // backend can't initialise.
    #[cfg(feature = "gui")]
    if use_gui {
        agent.set_quiet(true);
        let agent_slot: Arc<Mutex<Option<Agent>>> = Arc::new(Mutex::new(Some(agent)));
        match crate::gui::run(
            &agent_slot,
            &fg_cancel,
            &fg_quiet,
            &providers,
            &bus,
            full_auto,
        ) {
            Ok(()) => return,
            Err(e) => {
                eprintln!("pir: --gui failed: {e}; falling back to plain REPL");
                agent = agent_slot.lock().unwrap().take().expect("agent present");
                agent.set_quiet(false);
            }
        }
    }
    #[cfg(not(feature = "gui"))]
    if use_gui {
        eprintln!("pir: --gui requires the `gui` feature (build with --features gui)");
        // fall through to the streaming REPL below
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

    // Cross-instance model broadcast: `/model*` writes a small file
    // (~/.pi/agent/model-broadcast.json) that every running pir of this user
    // polls. We remember the latest generation we've *already* applied so the
    // watcher doesn't re-apply an old broadcast on startup, and a separate
    // thread queues newly-seen labels into `pending_model`; the REPL applies
    // them when idle (or right after a turn ends / errors). `self_pid` is used
    // so each instance ignores its own broadcast echo.
    let broadcast_seen = config::read_model_broadcast().map(|b| b.generation).unwrap_or(0);
    let pending_model: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let _watcher = spawn_model_broadcast_watcher(
        broadcast_seen,
        std::process::id(),
        pending_model.clone(),
    );

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
                // Bounded join: if the worker already finished (is_finished()
                // was true) the join returns immediately, so this is normally
                // free. The bound is a safety net so a pathological worker can
                // never pin the REPL in raw mode waiting on a turn — we must
                // never block the input thread for long.
                let _ = join_with_timeout(h, Duration::from_millis(500));
                term::raw::disable_raw();
                // Report token usage from the (now-returned) agent.
                if let Some(a) = agent_slot.lock().unwrap().as_ref() {
                    term::out(&term::dim(&format!(
                        "· {} in / {} out tokens",
                        fmt_tok(a.usage.input),
                        fmt_tok(a.usage.output)
                    )));
                }
                // Apply any cross-instance model switch queued while this turn
                // was running (a `/model*` from another terminal). We apply it
                // the moment the agent is back in the slot (turn done or errored
                // — both land here), so the next step uses the new model.
                if let Some(label) = pending_model.lock().unwrap().take() {
                    match apply_broadcast_model(&agent_slot, &current_ctx, &providers, &label) {
                        Ok(new_label) => term::out(&term::dim(&format!(
                            "· model switched to {new_label} (via /model* from another terminal)\n"
                        ))),
                        Err(e) => term::out(&term::dim(&format!(
                            "· ignored model broadcast '{label}': {e}\n"
                        ))),
                    }
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
                    // ESC/ctrl-c = stop EVERYTHING, now. The cooperative cancel
                    // only ends the turn after the current step; hard-abort
                    // kills the in-flight foreground command — and the sweep
                    // below also kills every *detached* background job (they
                    // were otherwise untouchable from the REPL: the agent that
                    // owns them sits on the turn's worker thread). Two paths:
                    //  - agent in slot (idle): kill synchronously;
                    //  - turn running: flip the shared job-kill flag, which the
                    //    agent's own bash wait loop consumes within 250ms and
                    //    sweeps on our behalf.
                    let mut killed = {
                        let mut g = agent_slot.lock().unwrap();
                        match g.as_mut() {
                            Some(a) => a.registry_kill_all_jobs(),
                            None => 0,
                        }
                    };
                    if let Some(f) = crate::agent::job_kill_flag() {
                        f.store(true, Ordering::SeqCst);
                    }
                    let _ = killed;
                    term::out(&term::dim("· cancelling turn (ESC/ctrl-c)…"));
                }
                term::raw::RawInput::Eof => {
                    if let Ok(mut g) = typeahead.lock() { g.clear(); }
                    fg_cancel.store(true, Ordering::SeqCst);
                    // Quitting (ctrl-d) also sweeps detached jobs so pir's
                    // children don't linger holding output pipes after we exit.
                    {
                        let mut g = agent_slot.lock().unwrap();
                        if let Some(a) = g.as_mut() {
                            let _ = a.registry_kill_all_jobs();
                        }
                    }
                    if let Some(f) = crate::agent::job_kill_flag() {
                        f.store(true, Ordering::SeqCst);
                    }
                    // Hard-abort the in-flight foreground command NOW (don't
                    // wait for it to finish — the whole point of ctrl-d is to
                    // stop the session *promptly*). The shared registry abort
                    // flag kills the bash child's process group on its next
                    // wait-loop tick; once it's parked in a blocking model read
                    // the cooperative cancel above already makes the stream
                    // parser bail within tens of milliseconds.
                    {
                        let mut g = agent_slot.lock().unwrap();
                        if let Some(a) = g.as_mut() {
                            a.registry_abort_active_command();
                        }
                    }
                    // The turn will end shortly of its own accord (the cancel
                    // flag is observed at the next safe boundary in the tool
                    // loop, or the in-flight stream aborts immediately). We
                    // must NOT block here waiting for it — a blocking `join()`
                    // would let ctrl-d hang for the whole (possibly multi-minute)
                    // turn, exactly the "ctrl-d won't stop pir" bug. So we detach
                    // the worker: drop the handle so it can't be joined, leave
                    // raw mode, and exit at once. The worker's own exit path
                    // (returns the agent to its slot + fires the oneshot) still
                    // runs free of us; PR_SET_PDEATHSIG + the death handler reap
                    // any stragglers if the process truly goes away.
                    let _ = fg_handle.take();
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
        // Status line under the prompt: current workspace + model in use.
        let workspace = workspace_label();
        let model = {
            let g = agent_slot.lock().unwrap();
            g.as_ref().map(|a| a.label()).unwrap_or_default()
        };
        term::out(&format!("{}\n", term::status_line(&workspace, &model)));
        // Apply any cross-instance model switch queued by the broadcast watcher
        // (from a `/model*` in another terminal). We only do this while idle, so
        // a mid-turn instance defers until it returns here — including after an
        // error, which lands back at this prompt too.
        if let Some(label) = pending_model.lock().unwrap().take() {
            match apply_broadcast_model(&agent_slot, &current_ctx, &providers, &label) {
                Ok(new_label) => term::out(&term::dim(&format!(
                    "· model switched to {new_label} (via /model* from another terminal)\n"
                ))),
                Err(e) => term::out(&term::dim(&format!(
                    "· ignored model broadcast '{label}': {e}\n"
                ))),
            }
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
            // If the user previously backgrounded a *running* turn (bare `&`),
            // that detached job still owns the interactive Agent (it's parked in
            // `None` in the slot until the job finishes and returns it). Join it
            // first so the agent is back before we take it for this new turn.
            jobs.reclaim_main_agent(&agent_slot);
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

/// How often the cross-instance model-broadcast watcher polls
/// `~/.pi/agent/model-broadcast.json` (a deliberately cheap, cooperative
/// poll — there is no daemon, so this is as close to "live" as we get across
/// independent `pir` processes without fs events).
const BROADCAST_POLL: Duration = Duration::from_secs(2);

/// Apply a `provider/model` label that arrived via the cross-instance model
/// broadcast (`/model*`), switching the idle agent and syncing the shared
/// background-job context. Returns the new label on success (for a status
/// line), or an error string. Must be called only when the agent is idle
/// (the slot is free), which the watcher/REPL guarantee before calling.
fn apply_broadcast_model(
    agent_slot: &AgentSlot,
    current_ctx: &Arc<Mutex<(Provider, Model, bool)>>,
    providers: &[Provider],
    label: &str,
) -> Result<String, String> {
    // Re-resolve against *this* instance's providers, since another terminal
    // may have a different model store. Drop the label if it resolves nowhere.
    let (p, m) = config::select(providers, label)?;
    let mut g = agent_slot.lock().unwrap();
    let agent = g.as_mut().ok_or_else(|| "agent busy (turn running)".to_string())?;
    agent.switch(p.clone(), m.clone())?;
    if let Ok(mut ctx) = current_ctx.lock() {
        ctx.0 = p.clone();
        ctx.1 = m.clone();
    }
    Ok(format!("{}/{}", p.pid(), m.id))
}

/// Spawn the cross-instance model-broadcast watcher. It polls
/// `~/.pi/agent/model-broadcast.json` every [`BROADCAST_POLL`]; when a newer
/// generation (than `last_seen`) appears that *this* process didn't originate,
/// it records the label in `pending_model` so the REPL applies it as soon as it
/// is idle (or right after a running turn ends / errors). `self_pid` is used to
/// ignore our own broadcast. Best-effort: any read/parse error is silently
/// skipped.
fn spawn_model_broadcast_watcher(
    last_seen: u64,
    self_pid: u32,
    pending_model: Arc<Mutex<Option<String>>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut seen = last_seen;
        loop {
            thread::sleep(BROADCAST_POLL);
            let Some(b) = config::read_model_broadcast() else { continue };
            if b.generation <= seen || b.generation == 0 {
                continue;
            }
            // Ignore broadcasts we ourselves published.
            if b.by_pid == self_pid as u64 {
                seen = b.generation;
                continue;
            }
            if b.label.is_empty() {
                seen = b.generation;
                continue;
            }
            seen = b.generation;
            // Queue it; the REPL applies it when safe (idle / after turn ends).
            if let Ok(mut pending) = pending_model.lock() {
                *pending = Some(b.label);
            }
        }
    })
}

type AgentSlot = Arc<Mutex<Option<Agent>>>;

/// A fresh session log path for a background job (tagged so it never collides
/// with the foreground session or another job).
fn session_log_path() -> PathBuf {
    let dir = config::pi_dir().join("agent").join("sessions");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("pir-{}-sh{}-bg{}.jsonl", term::timestamp_compact(), term::parent_shell_pid(), std::process::id()))
}

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
        "model*" | "model-all" => {
            // Broadcast a model switch to *every* running pir instance for this
            // user (each polls ~/.pi/agent/model-broadcast.json). On a match the
            // other instances apply it the next time they're idle (or as soon as
            // a running turn ends / errors). Also persists it as the new default
            // for *future* sessions and switches this instance immediately.
            if rest.is_empty() {
                println!("usage: /model* <selector>   (switch the model in all open pir terminals)");
                println!("{}", term::dim("also sets the new default for future sessions"));
                return;
            }
            match config::select(providers, &rest.join(" ")) {
                Ok((p, m)) => {
                    // Switch this instance right now (we already own the agent
                    // at the idle prompt; mid-turn we'd be in the fg_running arm).
                    {
                        let mut g = agent_slot.lock().unwrap();
                        let Some(agent) = g.as_mut() else {
                            eprintln!("pir: agent busy (turn running) — try again when idle");
                            return;
                        };
                        match agent.switch(p.clone(), m.clone()) {
                            Ok(()) => {
                                if let Ok(mut ctx) = current_ctx.lock() {
                                    ctx.0 = p.clone();
                                    ctx.1 = m.clone();
                                }
                                println!("→ {} (this instance)", agent.label());
                            }
                            Err(e) => eprintln!("{e}"),
                        }
                    }
                    // Persist as the default for new sessions too.
                    let _ = config::set_default_model(&p.pid(), &m.id);
                    // Broadcast to other running instances of this user.
                    match config::publish_model_broadcast(&format!("{}/{}", p.pid(), m.id)) {
                        Some(gen) => println!(
                            "{} broadcasting to all your open terminals (generation {gen})",
                            term::dim("·")
                        ),
                        None => eprintln!("pir: could not write broadcast file"),
                    }
                }
                Err(e) => eprintln!("{e}"),
            }
        }
        "models" => print!("{}", list_models(providers)),
        "login" => {
            // Store an API key for a provider in ~/.pi/agent/auth.json (pi's
            // `/login`, minus the OAuth/subscription flows). With an argument we
            // use it as the provider id; with none we list the known providers
            // (from the catalog) plus any already-stored ones and let the user
            // pick one. On success the key is saved and — if that provider is
            // already in the live model catalog — the agent switches to its
            // first model so the credential is live immediately. Environment
            // variables (`{env:VAR}` in models.json) take precedence over stored
            // keys, exactly as at startup.
            if fg_running {
                eprintln!("pir: a turn is running — finish or /cancel it first, then /login");
                return;
            }
            let provider_id = match rest.first().copied() {
                Some(id) => id.trim().to_string(),
                None => {
                    if providers.is_empty() {
                        term::read_answer("provider id: ")
                    } else {
                        println!("{}", term::bold("providers (from your model catalog):"));
                        for p in providers {
                            println!("  - {}", p.pid());
                        }
                        for id in config::stored_auth_providers() {
                            if !providers.iter().any(|p| p.pid().eq_ignore_ascii_case(&id)) {
                                println!("  - {}  {}", id, term::dim("(stored, not in catalog)"));
                            }
                        }
                        term::read_answer("provider id: ")
                    }
                }
            };
            let provider_id = provider_id.trim().to_string();
            if provider_id.is_empty() {
                return;
            }
            let key = term::read_secret(&format!("API key for {provider_id}: "));
            if key.is_empty() {
                eprintln!("pir: empty key — nothing saved");
                return;
            }
            match config::set_auth_key(&provider_id, &key) {
                Ok(path) => {
                    println!(
                        "{} saved API key for '{}' in {}",
                        term::green("✓"),
                        provider_id,
                        path.display()
                    );
                }
                Err(e) => {
                    eprintln!("pir: {e}");
                    return;
                }
            }
            // If the provider is in the live catalog, switch to its first model
            // so the credential is usable immediately. Re-resolve the provider
            // list (it now includes the freshly stored key) so a provider that
            // was only present via a stored key works without a restart.
            match config::load_providers() {
                Ok(fresh) => {
                    if let Some(p) = fresh.iter().find(|p| p.pid().eq_ignore_ascii_case(&provider_id)) {
                        if !p.models.is_empty() {
                            let mut g = agent_slot.lock().unwrap();
                            let Some(agent) = g.as_mut() else {
                                println!("{} key saved; it will be available after reload", term::dim("·"));
                                return;
                            };
                            if agent.switch(p.clone(), p.models[0].clone()).is_ok() {
                                if let Ok(mut ctx) = current_ctx.lock() {
                                    ctx.0 = p.clone();
                                    ctx.1 = p.models[0].clone();
                                }
                                println!("→ switched to {}", agent.label());
                            }
                        }
                    }
                }
                Err(_) => {}
            }
        }
        "logout" => {
            // Remove a stored credential from ~/.pi/agent/auth.json (pi's
            // `/logout`, which only touches the auth file — environment-variable
            // and models.json config are untouched). With no argument we list
            // what's stored; with one we remove that provider's entry.
            let stored = config::stored_auth_providers();
            let provider_id = match rest.first().copied() {
                Some(id) => id.trim().to_string(),
                None => {
                    if stored.is_empty() {
                        println!("{} no stored credentials in {}", term::dim("·"), config::auth_path().display());
                        return;
                    }
                    println!("{}", term::bold("stored credentials:"));
                    for id in &stored {
                        println!("  - {id}");
                    }
                    term::read_answer("provider id to log out: ")
                }
            };
            let provider_id = provider_id.trim().to_string();
            if provider_id.is_empty() {
                return;
            }
            match config::remove_auth_key(&provider_id) {
                Ok(true) => println!("{} removed stored credential for '{}'", term::green("✓"), provider_id),
                Ok(false) => eprintln!("pir: no stored credential for '{}'", provider_id),
                Err(e) => eprintln!("pir: {e}"),
            }
        }
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
            let resumed = agent.load_session(&log);
            agent.apply_persisted_su_security();
            agent.apply_persisted_thinking();
            jobs.mark_joined(id);
            println!("{} foregrounded job #{} from {}", term::bold("·"), id, log.display());
            if resumed.turns > 0 {
                println!("{}", resumed.banner(&log));
                for p in &resumed.prompts {
                    term::push_history(p);
                }
            }
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
            let resumed = agent.load_session(&path);
            if resumed.turns > 0 {
                println!("{}", resumed.banner(&path));
                for p in &resumed.prompts {
                    term::push_history(p);
                }
            } else if !resumed.summary.is_empty() {
                println!("{}", term::dim(&resumed.summary));
            }
            agent.apply_persisted_model();
            agent.apply_persisted_su_security();
            agent.apply_persisted_thinking();
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
        "thinking" => {
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                eprintln!("pir: agent busy (turn running) — try again when idle");
                return;
            };
            let arg = rest.join(" ");
            if arg.trim().is_empty() {
                // No argument: report the current level.
                println!("thinking level: {}", agent.thinking_level().as_str());
                println!(
                    "{}",
                    term::dim("set it with /thinking <off|minimal|low|medium|high|xhigh|max>; add 'show'/'hide' to toggle display")
                );
                return;
            }
            // Parse an optional trailing show/hide flag.
            let mut show: Option<bool> = None;
            let mut words: Vec<&str> = arg.split_whitespace().collect();
            if let Some(last) = words.last().copied() {
                match last {
                    "show" => {
                        show = Some(true);
                        words.pop();
                    }
                    "hide" => {
                        show = Some(false);
                        words.pop();
                    }
                    _ => {}
                }
            }
            let level_arg = words.join(" ");
            if !level_arg.is_empty() {
                match crate::config::ThinkingLevel::parse(&level_arg) {
                    Some(lvl) => println!("{}", agent.set_thinking(lvl)),
                    None => {
                        eprintln!(
                            "usage: /thinking [<off|minimal|low|medium|high|xhigh|max>] [show|hide]  (got '{level_arg}')"
                        );
                        return;
                    }
                }
            }
            if let Some(on) = show {
                println!("{}", agent.set_show_thinking(on));
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
        "sh" | "shell" => {
            // Drop down to an interactive shell (`/sh`), or run a single command
            // and return (`/sh COMMAND ARG1 ARG2 …`). The shell inherits the
            // agent's (possibly dropped) identity, cwd, env and stdio, so it runs
            // exactly as the current `pir` would — just with a human at the keys.
            // We restore raw mode only if the REPL had it active (mid-turn) so a
            // child shell isn't left fighting the REPL's terminal attributes; at
            // the idle prompt raw is already off, so nothing to do.
            if fg_running {
                eprintln!("pir: a turn is running — finish or /cancel it first, then /sh");
                return;
            }
            let args: Vec<&str> = rest;
            let was_raw = term::raw::is_active();
            if was_raw {
                term::raw::disable_raw();
            }
            let status = run_shell(args);
            if was_raw {
                term::raw::enable_raw();
            }
            if let Some(code) = status {
                if code != 0 {
                    eprintln!("pir: shell exited with status {}", code);
                }
            } else {
                eprintln!("pir: could not start shell");
            }
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
        "ext" => {
            // Diagnostic: list tools + slash commands currently provided by
            // extensions (e.g. the pi-extensions bridge), including ones the
            // model may call. Quiet when there are none.
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                eprintln!("pir: agent busy (turn running) — try again when idle");
                return;
            };
            let tools = agent.registry_spec_names();
            let cmds = agent.registry_command_names();
            println!("{} extension tools ({}):", term::bold("ext"), tools.len());
            for t in &tools {
                println!("  - {t}");
            }
            println!("{} extension commands ({}):", term::bold("ext"), cmds.len());
            for (n, d) in &cmds {
                println!("  - /{n}  {d}");
            }
        }
        "q" | "quit" | "exit" => std::process::exit(0),
        other => {
            // Unknown to the built-in set. Try extension-registered slash
            // commands (e.g. from the `pi-extensions` bridge). The agent owns
            // the registry; we take it briefly, run the command, and report the
            // result. Returns None when no extension claimed the name.
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                eprintln!("pir: agent busy (turn running) — try again when idle");
                return;
            };
            match agent.run_registered_command(other, rest.join(" ").trim()) {
                Some(outcome) => {
                    if outcome.is_error {
                        eprintln!("{}", outcome.content);
                    } else {
                        println!("{}", outcome.content);
                    }
                }
                None => eprintln!("unknown command /{other} — try /help"),
            }
        }
    }
}

/// Print the machine-readable extension ABI surface this `pir` build supports
/// (used by `pir --abi` and by the pre-install analyzer in the `pi-extensions`
/// host). Keep this in sync with `extensions/pi-extensions/ABI.md`.
fn print_abi() {
    println!("pir extension ABI (pi-extensions bridge surface)");
    println!();
    println!("events:");
    for e in [
        "session_start",
        "turn_start",
        "turn_end",
        "agent_start",
        "agent_end",
    ] {
        println!("  - {e}");
    }
    println!("pi.* API:");
    for a in [
        "pi.on(event, handler)",
        "pi.registerTool({name,label,description,parameters,execute})",
        "pi.registerCommand(name, {description, handler})",
    ] {
        println!("  - {a}");
    }
    println!("ctx.ui.*:");
    for a in ["ctx.ui.notify(text, level)", "ctx.ui.confirm(title, body) -> bool"] {
        println!("  - {a}");
    }
    println!("ctx.sessionManager.*:");
    println!("  - ctx.sessionManager.getSessionFile() -> string");
    println!("NOT supported (extension may break or be auto-stubbed):");
    for a in [
        "ctx.ui.custom() / setStatus / setWidget / input (TUI widgets)",
        "before_provider_request / before_provider_headers (provider interception)",
        "ctx.ui.confirm is auto-approved (no blocking UI on the host yet)",
    ] {
        println!("  - {a}");
    }
}

fn list_models(providers: &[Provider]) -> String {
    let mut out = String::new();
    let mut idx = 0usize;
    for p in providers {
        out.push_str(&format!("{}\n", term::bold(&p.pid())));
        for m in &p.models {
            let ctx = m.context.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
            out.push_str(&format!(
                "  {:>3}  {:<41} ctx {:>7}  {}\n",
                format!(":{idx}"),
                m.id,
                ctx,
                m.name.as_deref().unwrap_or("")
            ));
            idx += 1;
        }
    }
    out.push_str(&term::dim(
        "pick with /model <partial|provider/model|:N>  (indices are the :N shown above)\n",
    ));
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

/// Find the session file for a `-r` token: an index from the listing, a
/// fragment of the timestamp, or a fragment of the first-line preview.
/// With no token, default to the latest session from this shell (bash).
///
/// When there is no session from this shell (and no explicit token), instead of
/// the old line-prompt we launch the interactive arrow-key picker (`picker::
/// pick_session`): a full-screen two-pane UI that lists every session for this
/// project (newest first) and shows a live preview of the highlighted session's
/// first/last prompts and the tail of its last thinking + response. Enter /
/// Right resumes the highlighted one; `y` resumes the newest; `n`/Esc/ctrl-c/
/// ctrl-d/`q` start a fresh session. The picker only runs when stdin is a tty —
/// with piped/scripted stdin we fall back to a simple `read_answer` so we never
/// block forever.
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
            if token.is_some() {
                eprintln!("pir: no session matches '{token:?}'");
                return None;
            }
            // No session from this shell. Don't dead-end: offer the interactive
            // picker over every session in this project (they may be from a
            // different shell/terminal), with a live preview. The picker drives
            // stdin in raw mode and restores it on exit.
            eprintln!("pir: no session from this shell yet — pick one to resume:");
            if std::io::stdin().is_terminal() {
                let my_pid = term::parent_shell_pid();
                let items = build_pick_items(&sessions, my_pid);
                match crate::picker::pick_session(&items) {
                    crate::picker::PickResult::Resume(idx) => {
                        if let Some(s) = sessions.get(idx) {
                            return Some(s.path.clone());
                        }
                    }
                    crate::picker::PickResult::Cancel => {
                        eprintln!("pir: ok — not resuming (start fresh, or `pir -r <idx>` next time)");
                        return None;
                    }
                }
            } else {
                // Non-interactive stdin: fall back to the plain line prompt so a
                // scripted `pir -r` doesn't block forever.
                eprintln!("{}", list_sessions());
                let ans = term::read_answer("resume one? [idx | y=latest | n]");
                let ans = ans.as_str();
                let pick = if ans.is_empty() || ans == "n" || ans == "no" {
                    None
                } else if ans == "y" || ans == "yes" {
                    Some(0usize)
                } else {
                    ans.parse::<usize>().ok()
                };
                match pick.and_then(|n| sessions.get(n)) {
                    Some(s) => return Some(s.path.clone()),
                    None => eprintln!("pir: ok — not resuming (start fresh, or `pir -r <idx>` next time)"),
                }
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

/// Build the candidate list for the interactive `pir -r` picker from a scanned
/// session set (already sorted newest-first). Carries the `from_here` flag so
/// the picker can highlight sessions that came from this shell.
fn build_pick_items(sessions: &[Session], my_pid: u32) -> Vec<crate::picker::PickItem> {
    sessions
        .iter()
        .enumerate()
        .map(|(idx, s)| crate::picker::PickItem {
            index: idx,
            name: s.name.clone(),
            shell_pid: s.shell_pid,
            from_here: s.shell_pid == my_pid,
            mtime: s.mtime,
            path: s.path.clone(),
            preview_line: s.preview.clone(),
        })
        .collect()
}

/// Join a worker thread, but never block the caller longer than `budget`.
/// The worker runs the agent's `turn`, whose own cancel/abort paths end it
/// promptly; this bound is purely a safety net so a stuck worker can't pin the
/// REPL (in raw mode) forever. If the budget elapses the thread is detached
/// (it dies with the process), exactly like the ctrl-d/quit detach path.
fn join_with_timeout(h: JoinHandle<()>, budget: Duration) -> bool {
    let start = Instant::now();
    loop {
        if h.is_finished() {
            let _ = h.join();
            return true;
        }
        if start.elapsed() >= budget {
            // Give up waiting; detach (never block the REPL input thread).
            drop(h);
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
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

/// Collapse `$HOME` to `~` in a path for a compact display, leaving other
/// absolute paths intact. Used by the REPL status line's "Workspace" field.
fn home_collapsed(path: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = std::path::Path::new(&home);
        if let Ok(suffix) = path.strip_prefix(home) {
            return format!("~{}", suffix.to_string_lossy());
        }
    }
    path.to_string_lossy().to_string()
}

/// The "Workspace" shown in the status line: the current working directory with
/// `$HOME` collapsed to `~`. If `pir` was launched inside a git work tree we
/// show the repo root instead of the cwd so the workspace reads the same from
/// any subdirectory.
pub(crate) fn workspace_label() -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    if crate::project::is_git_repo(&cwd) {
        if let Some(root) = crate::project::repo_root_opt(&cwd) {
            return home_collapsed(&root);
        }
    }
    home_collapsed(&cwd)
}

/// `/sh [cmd args]` — drop into an interactive shell, or run a command via the
/// shell and return. With no args it execs the user's login shell (`$SHELL`,
/// else `/bin/sh`) so they get a familiar prompt. With args it runs
/// `cmd arg1 arg2 …` *through* the shell (`sh -c`), so pipes / globs / redirects
/// / env-expansion behave exactly as at a normal prompt, and reports the exit
/// status. The child inherits pir's stdio, identity (the possibly-dropped
/// `ai_*` user), cwd and environment, so it behaves identically to the
/// surrounding session. Returns the child's exit code, or `None` if the shell
/// could not be spawned.
fn run_shell(args: Vec<&str>) -> Option<i32> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let status = if args.is_empty() {
        // Interactive: hand the terminal straight to the login shell.
        std::process::Command::new(&shell).status()
    } else {
        // Single-shot: run the assembled command line through the shell.
        std::process::Command::new(&shell)
            .arg("-c")
            .arg(args.join(" "))
            .status()
    };
    match status {
        Ok(s) => Some(s.code().unwrap_or(1)),
        Err(_) => None,
    }
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
