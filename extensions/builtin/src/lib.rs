//! Built-in extension (type "a"): the core tools, linked statically like any
//! other extension. This is the reference implementation of the
//! [`pir::plugin::ToolBackend`] trait.
//!
//! It implements the confirm-before-acting UX (y/a/n) that the core wants for
//! tools that touch the filesystem or shell, but every other extension is free
//! to implement its own policy (or set `full_auto` in the registry to skip
//! prompts).

use crate::plugin::{Outcome, Registry, ToolBackend, ToolSpec};
use crate::term;
use serde_json::json;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub fn register(reg: &mut Registry) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    reg.add(Box::new(Builtin::new(cwd, reg.full_auto())));
}

struct Builtin {
    cwd: PathBuf,
    full_auto: bool,
    bash_ok: bool,
    write_ok: bool,
    // Long-running commands that were detached mid-flight (see run_shell's
    // 10-minute check-in) live here so the model can poll/kill them with the
    // job_status / job_kill tools instead of us blocking the turn forever.
    jobs: Vec<Job>,
    next_job: u64,
}

impl Builtin {
    fn new(cwd: PathBuf, full_auto: bool) -> Self {
        Builtin { cwd, full_auto, bash_ok: false, write_ok: false, jobs: Vec::new(), next_job: 1 }
    }
}

/// A long-running command detached from the foreground turn so the model can
/// keep working and poll/kill it. Output is drained on background threads into
/// shared, capped buffers so the pipe never blocks and job_status can show
/// partial progress without joining the drains.
struct Job {
    id: u64,
    command: String,
    child: std::process::Child,
    started: Instant,
    out: Arc<Mutex<Vec<u8>>>,
    err: Arc<Mutex<Vec<u8>>>,
    drain_out: JoinHandle<()>,
    drain_err: JoinHandle<()>,
}

impl ToolBackend for Builtin {
    fn name(&self) -> &'static str {
        "builtin"
    }

    fn specs(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "bash",
                description: "Run a shell command in the project directory (bash -c, or cmd /C on \
                              Windows). Returns stdout and stderr (truncated to 30k chars) plus the \
                              exit code. Long-running commands show a live elapsed timer; after 10 \
                              minutes the command is detached into a background job (returns a job \
                              id) so the agent can keep working and check back with job_status / \
                              job_kill. Hard-killed after 2 hours.",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The shell command to execute" }
                    },
                    "required": ["command"]
                }),
            },
            ToolSpec {
                name: "read_file",
                description: "Read a UTF-8 text file (truncated to 100k chars).",
                schema: json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }),
            },
            ToolSpec {
                name: "write_file",
                description: "Create or overwrite a file. Parent directories are created.",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string", "description": "Full new file content" }
                    },
                    "required": ["path", "content"]
                }),
            },
            ToolSpec {
                name: "edit_file",
                description: "Replace exactly one occurrence of old_string with new_string. \
                              old_string must be unique — include surrounding lines to disambiguate.",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "old_string": { "type": "string" },
                        "new_string": { "type": "string" }
                    },
                    "required": ["path", "old_string", "new_string"]
                }),
            },
            ToolSpec {
                name: "list_dir",
                description: "List the entries of a directory (non-recursive).",
                schema: json!({
                    "type": "object",
                    "properties": { "path": { "type": "string", "description": "Defaults to ." } },
                    "required": []
                }),
            },
            ToolSpec {
                name: "job_status",
                description: "Check on a long-running command that was detached (returned a job id \
                              like 'job#3'). Returns whether it is still running, its elapsed time, \
                              and any output captured so far. Pass id 0 to list all active jobs.",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer", "description": "Job id from a detached command, or 0 for all" }
                    },
                    "required": ["id"]
                }),
            },
            ToolSpec {
                name: "job_kill",
                description: "Stop a detached long-running command by its job id (from a 'job#N' \
                              result). Returns whether it was running and its exit code if it had \
                              finished.",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer", "description": "Job id to kill" }
                    },
                    "required": ["id"]
                }),
            },
            ToolSpec {
                name: "update_goal",
                description: "Persist and update the current goal/continuation plan so progress \
                              survives interrupts and can be resumed with `pir -c`. Actions: \
                              set_objective (objective:str), add_steps (steps:[str] array of step \
                              descriptions), set_status (status: active|complete|blocked|aborted), \
                              set_step (step_id:int, step_status: pending|in_progress|done|blocked, \
                              optional note:str), note (note:str free-form note). Always call this \
                              as you make progress; the goal is re-injected each turn.",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["set_objective", "add_steps", "set_status", "set_step", "note"],
                            "description": "What to change about the goal."
                        },
                        "objective": { "type": "string", "description": "For set_objective: the overall goal" },
                        "steps": {
                            "type": "array",
                            "description": "For add_steps: array of step description strings",
                            "items": { "type": "string" }
                        },
                        "status": { "type": "string", "description": "For set_status: active|complete|blocked|aborted" },
                        "step_id": { "type": "integer", "description": "For set_step: the step id to update" },
                        "step_status": { "type": "string", "description": "For set_step: pending|in_progress|done|blocked" },
                        "note": { "type": "string", "description": "Free-form note (set_step note, or note action body)" }
                    },
                    "required": ["action"]
                }),
            },
        ]
    }

    fn run(&mut self, name: &str, input: &serde_json::Value) -> Outcome {
        let result = match name {
            "bash" => self.do_bash(input),
            "read_file" => read_file(input),
            "write_file" => self.write_file(input),
            "edit_file" => self.edit_file(input),
            "list_dir" => list_dir(input, &self.cwd),
            "job_status" => self.job_status(input),
            "job_kill" => self.job_kill(input),
            other => Err(format!("unknown tool '{other}'")),
        };
        match result {
            Ok(content) => Outcome::ok(content),
            Err(content) => Outcome::err(content),
        }
    }
}

enum Decision {
    Yes,
    Always,
    No,
}

fn ask(what: &str) -> Decision {
    let answer = term::read_answer(&format!("Allow {what}? [y]es / [a]lways / [n]o (default no)"));
    match answer.as_str() {
        "y" | "yes" => Decision::Yes,
        "a" | "always" => Decision::Always,
        _ => Decision::No,
    }
}

impl Builtin {
    fn do_bash(&mut self, input: &serde_json::Value) -> Result<String, String> {
        let command = input["command"].as_str().ok_or("bash: missing 'command'")?;
        if !self.full_auto && !self.bash_ok {
            match ask(&format!("run {}", term::yellow(&format!("`{command}`")))) {
                Decision::No => return Ok("[denied] user declined to run this command".into()),
                Decision::Always => self.bash_ok = true,
                Decision::Yes => {}
            }
        }
        run_shell(self, command)
    }

    fn write_file(&mut self, input: &serde_json::Value) -> Result<String, String> {
        let path = input["path"].as_str().ok_or("write_file: missing 'path'")?;
        let content = input["content"].as_str().ok_or("write_file: missing 'content'")?;
        if !self.full_auto && !self.write_ok {
            let verb = if Path::new(path).exists() { "overwrite" } else { "create" };
            match ask(&format!("{verb} {}", term::yellow(path))) {
                Decision::No => return Ok("[denied] user declined this write".into()),
                Decision::Always => self.write_ok = true,
                Decision::Yes => {}
            }
        }
        if let Some(parent) = Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| format!("write_file {path}: {e}"))?;
            }
        }
        fs::write(path, content).map_err(|e| format!("write_file {path}: {e}"))?;
        Ok(format!("wrote {path} ({} lines, {} bytes)", content.lines().count(), content.len()))
    }

    fn edit_file(&mut self, input: &serde_json::Value) -> Result<String, String> {
        let path = input["path"].as_str().ok_or("edit_file: missing 'path'")?;
        let old = input["old_string"].as_str().ok_or("edit_file: missing 'old_string'")?;
        let new = input["new_string"].as_str().ok_or("edit_file: missing 'new_string'")?;
        let src = fs::read_to_string(path).map_err(|e| format!("edit_file {path}: {e}"))?;
        let hits = src.matches(old).count();
        if hits == 0 {
            return Err(format!("edit_file {path}: old_string not found"));
        }
        if hits > 1 {
            return Err(format!(
                "edit_file {path}: old_string appears {hits}x — add surrounding lines to make it unique"
            ));
        }
        if !self.full_auto && !self.write_ok {
            match ask(&format!("edit {}", term::yellow(path))) {
                Decision::No => return Ok("[denied] user declined this edit".into()),
                Decision::Always => self.write_ok = true,
                Decision::Yes => {}
            }
        }
        let updated = src.replacen(old, new, 1);
        fs::write(path, updated).map_err(|e| format!("edit_file {path}: {e}"))?;
        Ok(format!("edited {path}"))
    }

    /// Poll a detached long-running job (from a "job#N" result), or list all
    /// active jobs when id is 0. Shows partial output captured so far.
    fn job_status(&mut self, input: &serde_json::Value) -> Result<String, String> {
        let id = input["id"].as_u64().ok_or("job_status: missing 'id'")?;
        if id == 0 {
            if self.jobs.is_empty() {
                return Ok("(no active background jobs)".to_string());
            }
            let mut out = String::from("active background jobs:\n");
            for j in &mut self.jobs {
                let running = j.child.try_wait().map(|s| s.is_none()).unwrap_or(true);
                out.push_str(&format!(
                    "  job#{}  running={}  elapsed={}  cmd={}\n",
                    j.id,
                    running,
                    fmt_dur(j.started.elapsed()),
                    truncate_mid(&j.command, 60)
                ));
            }
            return Ok(out);
        }
        let idx = self.jobs.iter().position(|j| j.id == id);
        let Some(slot) = idx.map(|i| &mut self.jobs[i]) else {
            return Err(format!("job_status: no such job #{id}"));
        };
        match slot.child.try_wait().map_err(|e| format!("wait: {e}"))? {
            Some(status) => {
                let out = cap_str(&String::from_utf8_lossy(&slot.out.lock().unwrap()), 20_000);
                let err = cap_str(&String::from_utf8_lossy(&slot.err.lock().unwrap()), 20_000);
                let mut text = format!(
                    "job#{} finished (exit {}) after {}\n",
                    id,
                    status.code().unwrap_or(-1),
                    fmt_dur(slot.started.elapsed())
                );
                if !err.trim().is_empty() {
                    text.push_str("[stderr]\n");
                    text.push_str(&err);
                }
                text.push_str("[stdout]\n");
                text.push_str(&out);
                self.jobs.retain(|j| j.id != id);
                Ok(text)
            }
            None => {
                let out = cap_str(&String::from_utf8_lossy(&slot.out.lock().unwrap()), 8_000);
                let err = cap_str(&String::from_utf8_lossy(&slot.err.lock().unwrap()), 8_000);
                let mut text = format!(
                    "job#{} still running (elapsed {})\n",
                    id,
                    fmt_dur(slot.started.elapsed())
                );
                if !err.trim().is_empty() {
                    text.push_str("[stderr so far]\n");
                    text.push_str(&err);
                }
                if !out.trim().is_empty() {
                    text.push_str("[stdout so far]\n");
                    text.push_str(&out);
                }
                Ok(text)
            }
        }
    }

    /// Stop a detached job by id.
    fn job_kill(&mut self, input: &serde_json::Value) -> Result<String, String> {
        let id = input["id"].as_u64().ok_or("job_kill: missing 'id'")?;
        let idx = self.jobs.iter().position(|j| j.id == id);
        let Some(slot) = idx.map(|i| &mut self.jobs[i]) else {
            return Err(format!("job_kill: no such job #{id}"));
        };
        let was_running = slot.child.try_wait().map(|s| s.is_none()).unwrap_or(true);
        if was_running {
            let _ = slot.child.kill();
            let _ = slot.child.wait();
        }
        let status = slot.child.try_wait().map_err(|e| format!("wait: {e}"))?;
        // Take the drain handles out of the slot so we can join them.
        let drain_out = std::mem::replace(&mut slot.drain_out, std::thread::spawn(|| {}));
        let drain_err = std::mem::replace(&mut slot.drain_err, std::thread::spawn(|| {}));
        let _ = drain_out.join();
        let _ = drain_err.join();
        self.jobs.retain(|j| j.id != id);
        match status {
            Some(s) => Ok(format!("job#{} stopped (exit {})", id, s.code().unwrap_or(-1))),
            None => Ok(format!("job#{} killed", id)),
        }
    }
}

fn read_file(input: &serde_json::Value) -> Result<String, String> {
    let path = input["path"].as_str().ok_or("read_file: missing 'path'")?;
    let mut text = fs::read_to_string(path).map_err(|e| format!("read_file {path}: {e}"))?;
    crate::plugin::truncate(&mut text, 100_000);
    Ok(text)
}

fn list_dir(input: &serde_json::Value, anchor: &Path) -> Result<String, String> {
    // Honor a live `chdir` into a worktree: resolve relative to the process
    // cwd when it is inside the launch dir, else fall back to the launch anchor.
    let live = std::env::current_dir().unwrap_or_else(|_| anchor.to_path_buf());
    let base = if live.starts_with(anchor) { live } else { anchor.to_path_buf() };
    let path = input["path"].as_str().unwrap_or(".");
    let dir = base.join(path);
    let mut entries: Vec<String> = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| format!("list_dir {}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().to_string();
        let suffix = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) { "/" } else { "" };
        entries.push(format!("{name}{suffix}"));
    }
    if entries.is_empty() {
        return Ok("(empty)".into());
    }
    entries.sort();
    Ok(entries.join("\n"))
}

fn run_shell(b: &mut Builtin, command: &str) -> Result<String, String> {
    // Hard ceiling for a single foreground command. Raised from 120s so the
    // 10-minute check-in below is reachable; if it overruns the ceiling it is
    // hard-killed, but normally it is detached into a job at 10 minutes.
    const TIMEOUT: Duration = Duration::from_secs(120 * 60);
    // Show a live elapsed clock once the command has been running this long.
    const SHOW_AFTER: Duration = Duration::from_secs(10);
    // After this long, detach into a background job and hand control back to
    // the model (so an unattended agent never blocks waiting on a human).
    const CHECK_IN: Duration = Duration::from_secs(10 * 60);

    // Run in the *live* process cwd, not the launch cwd, so extension backends
    // (e.g. the worktree extension) can `cd` the agent into a linked worktree
    // and have bash honor it. Falls back to `b.cwd` if the process cwd is gone.
    let live_cwd = std::env::current_dir().unwrap_or_else(|_| b.cwd.clone());
    let mut child = spawn_shell(command, &live_cwd)?;
    // Shared, capped output buffers drained on background threads so the pipe
    // never blocks and a detached job can be polled for partial output.
    let out_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::with_capacity(64 * 1024)));
    let err_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::with_capacity(64 * 1024)));
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let out_buf_w = out_buf.clone();
    let drain_out = std::thread::spawn(move || copy_capped(&mut stdout, &out_buf_w));
    let err_buf_w = err_buf.clone();
    let drain_err = std::thread::spawn(move || copy_capped(&mut stderr, &err_buf_w));

    // Live elapsed-clock state, rendered/cleared from a ticker thread that we
    // stop via an `AtomicBool` so it cannot outlive the call. Only shown when
    // stderr is a TTY (an unattended agent logging to a file shouldn't get a
    // carriage-return clock every 250ms).
    let started = Instant::now();
    let clock_shown = Arc::new(AtomicBool::new(false));
    let clock_stop = Arc::new(AtomicBool::new(false));
    let clock_shown_w = clock_shown.clone();
    let clock_stop_w = clock_stop.clone();
    let clock_tty = term::is_terminal();
    let elapsed_tid = std::thread::spawn(move || {
        if !clock_tty {
            while !clock_stop_w.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(500));
            }
            return;
        }
        loop {
            if clock_stop_w.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
            let elapsed = started.elapsed();
            if elapsed < SHOW_AFTER {
                continue;
            }
            let line = format!("{} running {}s", term::yellow("·"), elapsed.as_secs());
            eprint!("\r{}", line);
            let mut serr = io::stderr();
            let _ = serr.flush();
            clock_shown_w.store(true, Ordering::SeqCst);
        }
    });

    let deadline = Instant::now() + TIMEOUT;
    let mut next_check_in = Instant::now() + CHECK_IN;
    let status = loop {
        match child.try_wait().map_err(|e| format!("wait: {e}"))? {
            Some(status) => break Some(status),
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                if Instant::now() >= next_check_in {
                    // Detach: hand the running command to the model as a job
                    // so it can keep working and decide later (poll with
                    // job_status, stop with job_kill).
                    let elapsed = started.elapsed();
                    clock_stop.store(true, Ordering::SeqCst);
                    let _ = elapsed_tid.join();
                    if clock_shown.load(Ordering::SeqCst) {
                        eprint!("\r\x1b[K");
                        let mut serr = io::stderr();
                        let _ = serr.flush();
                    }
                    let id = b.next_job;
                    b.next_job += 1;
                    b.jobs.push(Job {
                        id,
                        command: command.to_string(),
                        child,
                        started,
                        out: out_buf,
                        err: err_buf,
                        drain_out,
                        drain_err,
                    });
                    return Ok(format!(
                        "[detached] command still running after {}; it is now background job #{}. \
                         The agent can keep working and check on it with job_status({}) or stop it \
                         with job_kill({}).",
                        fmt_dur(elapsed),
                        id,
                        id,
                        id
                    ));
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    };

    clock_stop.store(true, Ordering::SeqCst);
    let _ = elapsed_tid.join();
    if clock_shown.load(Ordering::SeqCst) {
        eprint!("\r\x1b[K");
        let mut serr = io::stderr();
        let _ = serr.flush();
    }

    let _ = drain_out.join();
    let _ = drain_err.join();
    let out = out_buf.lock().unwrap().clone();
    let err = err_buf.lock().unwrap().clone();

    let mut text = String::from_utf8_lossy(&out).to_string();
    let err_text = String::from_utf8_lossy(&err).to_string();
    if !err_text.trim().is_empty() {
        if !text.trim().is_empty() {
            text.push('\n');
        }
        text.push_str("[stderr]\n");
        text.push_str(&err_text);
    }
    match status {
        Some(s) if s.success() => {}
        Some(s) => text.push_str(&format!("\n[exit code {}]", s.code().unwrap_or(-1))),
        None => text.push_str(&format!("\n[pir] timed out after {}s, killed", TIMEOUT.as_secs())),
    }
    crate::plugin::truncate(&mut text, 30_000);
    Ok(text)
}

/// Drain `r` into the shared buffer, stopping once it exceeds 4 MB (enough for
/// any realistic agent command; older output is dropped from the head).
fn copy_capped(r: &mut impl Read, buf: &Arc<Mutex<Vec<u8>>>) {
    use std::io::Read as _;
    let mut tmp = [0u8; 8192];
    loop {
        match r.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                let mut g = buf.lock().unwrap();
                g.extend_from_slice(&tmp[..n]);
                const CAP: usize = 4 * 1024 * 1024;
                if g.len() > CAP {
                    let drop = g.len() - CAP;
                    g.drain(0..drop);
                }
            }
            Err(_) => break,
        }
    }
}

/// Compact human duration: "37s", "2m13s", "1h02m".
fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    let rs = s % 60;
    if m < 60 {
        return format!("{m}m{rs:02}s");
    }
    let h = m / 60;
    format!("{h}h{:02}m", m % 60)
}

/// Truncate `s` to `max` chars, keeping the tail and prefixing a marker.
fn cap_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut = s.char_indices().nth(s.chars().count().saturating_sub(max)).map(|(i, _)| i).unwrap_or(0);
    format!("… [pir] truncated]\n{}", &s[cut..])
}

/// Truncate from the middle, keeping head and tail, for displaying commands.
fn truncate_mid(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let half = max / 2;
    let head: String = s.chars().take(half).collect();
    let tail: String = s.chars().rev().take(half).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{head}…{tail}")
}

fn spawn_shell(command: &str, cwd: &Path) -> Result<std::process::Child, String> {
    let build = |prog: &str, flag: &str| {
        let mut c = Command::new(prog);
        c.arg(flag).arg(command);
        c.current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        c
    };
    let (prog, flag) = if cfg!(windows) { ("cmd", "/C") } else { ("bash", "-c") };
    match build(prog, flag).spawn() {
        Ok(c) => Ok(c),
        Err(e) if !cfg!(windows) => {
            build("sh", "-c").spawn().map_err(|_| format!("spawn {prog}: {e}"))
        }
        Err(e) => Err(format!("spawn {prog}: {e}")),
    }
}
