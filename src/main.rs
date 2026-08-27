mod agent;
mod config;
mod goal;
mod notify;
mod plugin;
mod project;
mod provider;
mod term;
mod types;
mod user;

// Statically linked extensions, emitted by build.rs (type "a").
include!(concat!(env!("OUT_DIR"), "/gen_registry.rs"));

use crate::agent::Agent;
use crate::config::Provider;
use crate::notify::{AgentEvent, SharedBus};
use std::io::BufRead;
use std::io::Write;
use std::path::PathBuf;
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
  -h, --help  -V, --version

CONFIG (reused from pi, never modified)
  ~/.pi/models.json          providers, models, api keys ("{env:VAR}" supported)
  ~/.pi/agent/settings.json  optional default model ("model" key)
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
  /help  /model <sel>  /models  /sessions  /goal [objective]  /continue
  /bg <text>  /jobs  /fg <id>  /clear  /usage  /exit
  /project init            create the ai_<project> user and chown the cwd (root)
  /create [name]           scaffold a new project (seeds from clipboard .md spec)

  Lines ending in & run in the background: "fix the parser &"  => /bg fix the parser
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
}

impl BackgroundJobs {
    fn new() -> Self {
        BackgroundJobs { next_id: 1, jobs: Vec::new() }
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
                Err(e) => agent.notify_on_exit(AgentEvent::Error { message: e }),
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
        out.push_str(&term::dim("foreground with: /fg <id>  (reloads that session)\n"));
        out
    }

    fn mark_joined(&mut self, id: usize) {
        if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
            j.joined = true;
        }
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
    term::set_model_providers(&providers);

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

    // Drop privileges to the per-project user *after* config/providers are
    // loaded but *before* the agent (and any tool) runs. On non-unix this is a
    // no-op. All `bash`/file tools then execute as that user automatically.
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

    let mut agent = match Agent::new(
        provider.clone(),
        model.clone(),
        full_auto,
        false,
        bus.clone(),
        resume.as_ref(),
    ) {
        Ok(a) => a,
        Err(e) => die(&e),
    };

    // Resume prior history if `-r`/`-c` was given.
    if let Some(session) = &resume {
        agent.load_session(session);
    }

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
        jobs.spawn(prompt, agent.log_path.clone().unwrap_or_default(), {
            let bus = bus.clone();
            move || {
                Agent::new(provider, model, full_auto, true, bus, None)
                    .expect("agent build in background thread")
            }
        });
        agent.notify_on_exit(AgentEvent::Idle);
        return;
    }
    if let Some(p) = &agent.log_path {
        let hist = p.with_extension("history");
        term::set_history_file(&hist);
    }

    if !prompt.is_empty() {
        match agent.turn(&prompt.join(" ")) {
            Ok(()) => agent.notify_on_exit(agent.turn_done_event()),
            Err(e) => agent.notify_on_exit(AgentEvent::Error { message: e }),
        }
        return;
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
    // Show the execution user when it isn't the invoking root (per-project
    // `ai_X` sandbox), so it's clear commands run as that identity.
    #[cfg(unix)]
    if let Some(u) = resolved_user.as_deref() {
        println!("{}", term::dim(&format!("running as user {u}")));
    }
    if let Some(p) = &agent.log_path {
        println!("{}", term::dim(&format!("session log: {}", p.display())));
    }
    println!("{}", term::dim("/help for commands · ctrl-d to quit"));

    let mut jobs = BackgroundJobs::new();

    let mut line = String::new();
    loop {
        line.clear();

        // Surface notifications from ALL agents (foreground + background) on the
        // active screen before showing the prompt. Background sessions publish
        // to the shared bus while this loop is blocked on read_line, so this is
        // where their "done" notifications become visible.
        let feed = bus.drain_feed();
        let rendered = crate::notify::render_feed(&feed);
        if !rendered.is_empty() {
            print!("{rendered}");
            let _ = std::io::stdout().flush();
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
        // `&` suffix => run the rest in the background.
        let bg = input.ends_with('&') && !input.trim_end_matches('&').is_empty();
        let input = input.trim_end_matches('&').trim();
        if let Some(cmd) = input.strip_prefix('/') {
            handle_command(cmd, &mut agent, &providers, &mut jobs, full_auto, &bus);
        } else if bg {
            // Run this prompt as a fresh background job that keeps its own
            // session log (the foreground session is unaffected).
            let log = session_log_path();
            let provider = agent.provider();
            let model = agent.model();
            let bus = bus.clone();
            jobs.spawn(input.to_string(), log, move || {
                Agent::new(provider, model, full_auto, true, bus, None).expect("bg agent")
            });
        } else {
            let _ = agent.turn(input);
            agent.notify_on_exit(AgentEvent::Idle);
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

/// A fresh session log path for a background job (tagged so it never collides
/// with the foreground session or another job).
fn session_log_path() -> PathBuf {
    let dir = config::pi_dir().join("agent").join("sessions");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("pir-{}-sh{}-bg{}.jsonl", term::timestamp_compact(), term::parent_shell_pid(), std::process::id()))
}

fn handle_command(cmd: &str, agent: &mut Agent, providers: &[Provider], jobs: &mut BackgroundJobs, full_auto: bool, bus: &SharedBus) {
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
        "sessions" => print!("{}", list_sessions()),
        "bg" => {
            let prompt: String = rest.join(" ");
            if prompt.trim().is_empty() {
                eprintln!("usage: /bg <prompt>  (or end a line with &)");
            } else {
                let log = session_log_path();
                let provider = agent.provider();
                let model = agent.model();
                let bus = bus.clone();
                jobs.spawn(prompt, log, move || {
                    Agent::new(provider, model, full_auto, true, bus, None).expect("bg agent")
                });
            }
        }
        "jobs" | "background" => print!("{}", jobs.list()),
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
            agent.clear();
            agent.load_session(&log);
            jobs.mark_joined(id);
            println!("{} foregrounded job #{} from {}", term::bold("·"), id, log.display());
        }
        "project" => {
            if rest.is_empty() || rest[0] == "init" {
                run_project_subcommand(&rest.iter().map(|s| s.to_string()).collect::<Vec<_>>());
            } else {
                eprintln!("unknown /project subcommand '{}' — try /project init", rest[0]);
            }
        }
        "create" => {
            let name: String = rest.join(" ");
            create_project(&name);
        }
        "goal" => {
            let obj: String = rest.join(" ");
            if obj.trim().is_empty() {
                agent.show_goal();
            } else {
                agent.start_goal(&obj);
                println!("goal started: {}", obj);
            }
        }
        "continue" | "cont" => {
            let lp = agent.log_path.clone();
            if let Some(p) = lp {
                agent.attach_goal(&p);
            }
            agent.continue_goal();
        }
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
    use std::path::PathBuf;
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

/// `/create [name]` — scaffold a new project directory under `PIR_PROJECTS_DIR`
/// (default `~/.pi/projects`). If the system clipboard holds a project markdown
/// spec (the `unmd2.sh` format of `### path` headers + ``` code blocks), offer
/// to extract it into the new project.
fn create_project(name: &str) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;

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
        eprintln!("DBG clipboard len={} mdlike={} first={:?}", text.len(), crate::project::looks_like_project_md(&text), text.chars().next());
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
