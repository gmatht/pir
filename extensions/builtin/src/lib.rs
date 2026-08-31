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
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub fn register(reg: &mut Registry) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    reg.add(Box::new(Builtin::new(
        cwd,
        reg.full_auto(),
        reg.abort.clone(),
        Arc::new(AtomicBool::new(false)),
    )));
}

struct Builtin {
    cwd: PathBuf,
    full_auto: bool,
    bash_ok: bool,
    write_ok: bool,
    // Long-running commands detached mid-flight (see run_shell's
    // 10-minute check-in) live here so the model can poll/kill them with the
    // job_status / job_kill tools instead of us blocking the turn forever.
    jobs: Vec<Job>,
    next_job: u64,
    /// Shared hard-abort flag (the same `Arc` the REPL holds). When the user
    /// presses ESC/ctrl-c, the REPL sets it; `run_shell` polls it and kills the
    /// running child immediately instead of waiting for it to exit. Cleared at
    /// the start of each `bash` call so a stale abort from a previous command
    /// doesn't fire spuriously.
    abort: Arc<AtomicBool>,
    /// Shared "go silent" switch (the same `Arc` the REPL holds). When the user
    /// backgrounds a running turn (bare `&`), the REPL flips it; the bash
    /// tool's live elapsed clock then stops writing to the terminal so /// detached turn is silent instead of polluting the prompt with
    /// `· running Ns` lines. False by default (attached / foreground).
    quiet: Arc<AtomicBool>,
    /// Shared "kill every detached job" switch (the same `Arc` the REPL
    /// holds). The REPL flips it when the user presses ESC/ctrl-c (or quits):
    /// detached jobs are otherwise untouchable from the REPL (the agent that
    /// owns them is taken out of the agent slot mid-turn), so the only ways
    /// to stop one were `job_kill` by the model or exiting pir entirely. The
    /// `run_shell` wait loop of the *foreground* command polls this flag too,
    /// so an ESC kills the running command AND every detached job in one go.
    /// Consumed (cleared) by whoever acts on it.
    job_kill: Arc<AtomicBool>,
}

impl Builtin {
    fn new(cwd: PathBuf, full_auto: bool, abort: Arc<AtomicBool>, quiet: Arc<AtomicBool>) -> Self {
        Builtin {
            cwd,
            full_auto,
            bash_ok: false,
            write_ok: false,
            jobs: Vec::new(),
            next_job: 1,
            abort,
            quiet,
            job_kill: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Kill EVERY detached job (ESC/ctrl-c semantics). Bounded: each kill uses
    /// the poll-based `kill_process_tree` (no blocking waits), and the drained
    /// output stays readable in the shared buffer. Returns how many *running*
    /// jobs were killed.
    fn kill_all_jobs(&mut self) -> usize {
        let mut killed = 0;
        for j in self.jobs.iter_mut() {
            if j.child.try_wait().map(|s| s.is_none()).unwrap_or(true) {
                kill_process_tree(&mut j.child);
                killed += 1;
            }
            // Abandon the drains (bounded join) — never block the REPL thread.
            let do_ = std::mem::replace(&mut j.drain_out, std::thread::spawn(|| {}));
            let de = std::mem::replace(&mut j.drain_err, std::thread::spawn(|| {}));
            join_drain(do_);
            join_drain(de);
        }
        self.jobs.clear();
        killed
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

    fn set_job_kill_handle(&mut self, f: Arc<AtomicBool>) {
        self.job_kill = f;
    }

    fn kill_all_jobs(&mut self) -> usize {
        Builtin::kill_all_jobs(self)
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
        // Guard #1 (item: stop agents killing their peers). A `pkill`/`kill`
        ///`killall` aimed at `pir` (or scoped to the running user) is almost
        // always a mass-extinction trigger — both historical mass deaths here
        // were exactly an agent running `pkill -f target/.../pir` / `pkill -u
        // ai_pir`. Always require an explicit human confirm before such a
        // command runs, even in full-auto (correctness/safety beats
        // unattendedness for a command that can terminate every other session).
        if let Some(why) = Self::dangerous_kill_reason(command) {
            let answer = term::read_answer(&format!(
                "{} this command looks like it would kill pir processes ({why}). Run it anyway? [y]es / [n]o (default no) ",
                term::yellow("⚠")
            ));
            match answer.as_str() {
                "y" | "yes" => {}
                _ => return Ok("[denied] refusing to run a self-targeting kill command".into()),
            }
        }
        if !self.full_auto && !self.bash_ok {
            match ask(&format!("run {}", term::yellow(&format!("`{command}`")))) {
                Decision::No => return Ok("[denied] user declined to run this command".into()),
                Decision::Always => self.bash_ok = true,
                Decision::Yes => {}
            }
        }
        // Guard #2: privilege escalation (`sudo`/`su`/...). `sudo` prompts for a
        // password on `/dev/tty` — NOT on the stdin we hand it (which is
        // `/dev/null`), so it would block forever until the 10-minute detach,
        // looking exactly like the "hangs waiting for stdin" bug. We never let a
        // command silently sit there waiting on a password: we ask the user to
        // approve the escalation and, unless they already hold a passwordless
        // sudo rule, we refuse and tell them how to escalate out-of-band
        // (the rootreq / ai-permctl "request, don't take" model). This makes the
        // agent prompt before escalating and never blocks on a TTY password.
        if let Some(esc) = Self::priv_escalation(command) {
            match self.request_priv_escalation(command, esc) {
                Ok(()) => {}
                Err(msg) => return Ok(msg),
            }
        }
        run_shell(self, command)
    }

    /// Detect a command that escalates privilege. We don't try to parse the
    /// shell — just look for the obvious escalation shapes (`sudo …`, `su …`,
    /// `doas …`). Returns a short human label of *what* the command escalates to
    /// when one is found, else `None`. The leading token is checked at a word
    /// boundary so substrings like `mesudo` don't trigger it.
    fn priv_escalation(command: &str) -> Option<String> {
        let c = command.trim();
        // Whole-word scan: an escalation token is a word boundary on both
        // sides (start/space/quote/paren/`|`/`;`/`&` on the left, and
        // whitespace/quote/paren/`|`/`;`/`&`/end on the right). This lets us
        // catch both leading tokens and inline ones inside `bash -c '…'`
        // without a real shell parser, while ruling out false hits like
        // `mesudo`.
        let word_at = |pat: &str| -> bool {
            let mut start = 0;
            while let Some(pos) = c[start..].find(pat) {
                let idx = start + pos;
                let before_ok = idx == 0
                    || c.as_bytes()[idx - 1].is_ascii_whitespace()
                    || matches!(c.as_bytes()[idx - 1], b'\'' | b'"' | b'(' | b'|' | b';' | b'&');
                let after = idx + pat.len();
                let after_ok = after >= c.len()
                    || c.as_bytes()[after].is_ascii_whitespace()
                    || matches!(c.as_bytes()[after], b'\'' | b'"' | b')' | b'|' | b';' | b'&');
                if before_ok && after_ok {
                    return true;
                }
                start = after;
            }
            false
        };

        let sudo_present = word_at("sudo") || word_at("sudoedit");
        let su_present = word_at("su");
        let doas_present = word_at("doas");

        if sudo_present {
            // `sudo -u <user>` => that user; `sudo -i` => root login shell;
            // bare `sudo` => root.
            if let Some(rest) = c.find("sudo").and_then(|i| c[i + 4..].trim().strip_prefix("-u ")) {
                let user = rest.split_whitespace().next().unwrap_or("root");
                return Some(format!("sudo as {user}"));
            }
            let after_sudo = c.find("sudo").map(|i| &c[i + 4..]).unwrap_or("");
            if after_sudo.trim_start().starts_with("-i") {
                return Some("sudo as root (login shell)".into());
            }
            return Some("sudo as root".into());
        }
        if doas_present {
            return Some("doas (privilege escalation)".into());
        }
        if su_present {
            let after_su = c.find("su").map(|i| &c[i + 2..]).unwrap_or("");
            let target = after_su.trim_start().split_whitespace().next().unwrap_or("root");
            return Some(format!("su to {target}"));
        }
        None
    }

    /// Ask the user to approve a privilege-escalating command. Returns `Ok(())`
    /// if it may run, `Err(msg)` if refused. When stdin is not a terminal (an
    /// unattended agent, or a piped prompt), there is no human to type a
    /// password, so we *never* wait: if the calling user already holds a
    /// passwordless sudo rule for this command we let it run inline (a quick
    /// non-interactive probe, see `sudo_is_passwordless`), otherwise we refuse
    /// and point them at the proper escalation path. When a human is present we
    /// prompt, and still verify the password isn't required so the command
    /// won't hang on `/dev/tty`.
    fn request_priv_escalation(&self, command: &str, what: String) -> Result<(), String> {
        if io::stdin().is_terminal() {
            let answer = term::read_answer(&format!(
                "{} escalate privilege ({})? [y]es / [n]o (default no) ",
                term::yellow("⚠"),
                what
            ));
            match answer.as_str() {
                "y" | "yes" => {}
                _ => return Err("[denied] user declined to escalate privilege".into()),
            }
        }
        // Either unattended (no tty, refused-by-default is handled above only
        // for the prompt; here we must still check passwordless), or approved.
        // In either case, refuse to run if a password prompt would block us.
        if self.sudo_is_passwordless(command) {
            return Ok(());
        }
        Err(format!(
            "[denied] refusing to escalate ({what}): a password would be required and pir has no \
             TTY to read it, so the command would hang. Escalate out-of-band instead — e.g. via the \
             rootreq flow (`request_root`), or run the command yourself in a shell where sudo is \
             passwordless (NOPASSWD in sudoers)."
        ))
    }

    /// Non-interactively probe whether `sudo` for this command needs a password.
    /// `sudo -n` (non-interactive) returns exit status 1 (and a "password
    /// required" error) when auth is needed, and 0 when the rule already allows
    /// passwordless execution. We run a harmless `sudo -n true` rather than the
    /// real command so we don't accidentally execute escalated work just to test
    /// it; if `-n true` is allowed, the user clearly has *some* passwordless
    /// sudo and we honour the approved escalation. Returns true when no
    /// password is required.
    fn sudo_is_passwordless(&self, _command: &str) -> bool {
        #[cfg(unix)]
        {
            let r = std::process::Command::new("sudo")
                .arg("-n")
                .arg("true")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            matches!(r, Ok(code) if code.success())
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// Return `Some(reason)` if `command` looks like it would terminate `pir`
    /// processes (this or sibling sessions), else `None`. Heuristic but
    /// conservative: it matches the exact patterns that caused the two real
    /// mass extinctions. We deliberately do NOT try to parse shell — just look
    /// for the dangerous tokens. A human confirm is still required, so false
    /// positives only add a prompt, never block unattended safety-critical ops.
    fn dangerous_kill_reason(command: &str) -> Option<&'static str> {
        let c = command;
        // `pkill`/`killall` against `pir`/`target/...pir` (the #1 extinction).
        if (c.contains("pkill") || c.contains("killall")) && c.contains("pir") {
            return Some("pkill/killall targeting pir");
        }
        // `pkill -u <this user>` (the #2 extinction killed `ai_pir`).
        if c.contains("pkill -u") || c.contains("pkill -U") {
            return Some("pkill scoped to a user");
        }
        // A bare `kill -9` / `kill -TERM` with a broad target is ambiguous, but
        // `kill` of a process group (`kill -<sig> -<pgid>`) or `killall` of
        // `pir` is the dangerous shape; `kill <pid>` of is allowed
        // (that's normal job control). We only flag killall already covered.
        None
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
        // Kill the WHOLE process group (the child got process_group(0)), not
        // just the direct `bash -c` child: grandchildren like `cargo`/`rustc`
        // survive a lone SIGKILL to the child and keep the output pipes open,
        // which used to hang the drain joins below forever — job_kill never
        // returned, so the agent loop and the REPL with it (the recorded
        // "job_kill then silence" hang). If it had already exited, just reap.
        let status = if was_running {
            kill_process_tree(&mut slot.child)
        } else {
            slot.child.wait().ok()
        };
        // Take the drain handles out of the slot so we can join them — but
        // bounded: if anything still holds the pipe (a grandchild that raced
        // the kill, an orphaned `cmd &`), abandon the drain thread instead of
        // blocking the REPL forever. The thread dies with the process and the
        // output already captured in the shared buffer stays readable.
        let drain_out = std::mem::replace(&mut slot.drain_out, std::thread::spawn(|| {}));
        let drain_err = std::mem::replace(&mut slot.drain_err, std::thread::spawn(|| {}));
        join_drain(drain_out);
        join_drain(drain_err);
        self.jobs.retain(|j| j.id != id);
        match status {
            Some(s) if !was_running => Ok(format!(
                "job#{} had already finished (exit {}); nothing to kill",
                id,
                s.code().unwrap_or(-1)
            )),
            Some(s) => match s.code() {
                Some(c) => Ok(format!("job#{} stopped (exit {})", id, c)),
                None => Ok(format!("job#{} killed (by signal)", id)),
            },
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
    // Overridable for tests/CI via PIR_SHELL_CHECK_IN_SECS.
    let check_in_secs: u64 = std::env::var("PIR_SHELL_CHECK_IN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&s| s > 0)
        .unwrap_or(10 * 60);
    let check_in = Duration::from_secs(check_in_secs);

    // The shared hard-abort flag (`b.abort` is the same Arc as the REPL's
    // `fg_cancel`). When the user presses ESC/ctrl-c, the REPL sets it; we poll
    // it in the wait loop and kill the child immediately instead of waiting for
    // it to exit. We do NOT clear it here: it is reset by the worker at the
    // start of each turn (`cancel.store(false)`), and clearing it now could
    // wipe a cancel request meant for the turn as a whole. Capture its state
    // the instant we act on it so the final status line is accurate.

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
    // Capture the shared "go silent" switch (same Arc as the REPL's
    // `fg_quiet`). When the user backgrounds the running turn (bare `&`), the
    // REPL flips it; this clock thread then stops writing to the terminal so a
    // detached turn is silent instead of polluting the prompt with `· running
    // Ns` lines every 250ms.
    let quiet_w = b.quiet.clone();
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
            // Detached turn: go silent immediately. Erase any clock already on
            // the line so it doesn't linger after the turn is backgrounded.
            if quiet_w.load(Ordering::SeqCst) {
                if clock_shown_w.swap(false, Ordering::SeqCst) {
                    eprint!("\r\x1b[K");
                    let mut serr = io::stderr();
                    let _ = serr.flush();
                }
                std::thread::sleep(Duration::from_millis(250));
                continue;
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

    let abort = b.abort.clone();
    let deadline = Instant::now() + TIMEOUT;
    let next_check_in = Instant::now() + check_in;
    // Kill the whole process group of the child (item: child is in its own
    // group via process_group(0)), so shell pipelines / subcommands die too,
    // and so cancelling a runaway command never takes pir down with it.
    // TERM first, bounded grace, then KILL for survivors.
    let kill_tree = |child: &mut std::process::Child| {
        kill_process_tree(child);
    };
    let status = loop {
        // ESC/ctrl-c hard-abort: if the user asked to cancel the command, kill
        // the child immediately and stop — no waiting for it to exit on its own.
        // Also consume the shared job-kill switch: an ESC must stop EVERY
        // detached job too, not just this foreground command (they used to keep
        // running, holding output pipes and wedging later commands/jobs).
        if abort.load(Ordering::SeqCst) {
            b.kill_all_jobs();
            kill_tree(&mut child);
            break None;
        }
        if b.job_kill.swap(false, Ordering::SeqCst) {
            // Same semantics, arriving via the registry's job-kill switch.
            b.kill_all_jobs();
            kill_tree(&mut child);
            break None;
        }
        match child.try_wait().map_err(|e| format!("wait: {e}"))? {
            Some(status) => break Some(status),
            None => {
                if Instant::now() >= deadline {
                    b.kill_all_jobs();
                    kill_tree(&mut child);
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

    // Bounded joins: if something outside the group still holds the pipe (an
    // orphaned `cmd &` from the command string), don't block the REPL forever
    // waiting for EOF — abandon the drain thread instead.
    let drains_done = join_drain(drain_out) && join_drain(drain_err);
    let out = out_buf.lock().unwrap().clone();
    let err = err_buf.lock().unwrap().clone();
    let _ = drains_done;

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
        None if abort.load(Ordering::SeqCst) => text.push_str("\n[pir] command aborted by user (ESC/ctrl-c)"),
        None => text.push_str(&format!("\n[pir] timed out after {}s, killed", TIMEOUT.as_secs())),
    }
    // In-process capability-deny detection (the chosen lightweight option;
    // docs/SECURITY_INTENT.md §6). Runs ONCE per command, after it exits —
    // ~µs, no per-syscall cost. Flags attempts that visibly needed a (dropped)
    // capability: a permission/operation denial in the output, or a privileged
    // tool in the command. Gated to the cap-dropped container context (or an
    // explicit PIR_DETECT_CAPS=1) so benign "permission denied" elsewhere
    // doesn't spam. auditd / seccomp RET_LOG|ERRNO|USER_NOTIF are documented
    // configurable alternatives if in-kernel attempt logging is ever needed.
    let caps_dropped = crate::security::overlay::container_engaged()
        || std::env::var_os("PIR_DETECT_CAPS").map(|v| v != "0" && !v.is_empty()).unwrap_or(false);
    if caps_dropped {
        let lower = text.to_ascii_lowercase();
        let hint: Option<&'static str> = if lower.contains("operation not permitted")
            || lower.contains("permission denied")
        {
            Some("output shows a permission/operation denial")
        } else {
            const PRIV_TOOLS: &[&str] = &[
                "mount ", "umount ", "chroot ", "setcap ", "nsenter ", "capsh ",
                "iptables ", "nft ", "mknod ", "setuid ", "setgid ", "capset ",
            ];
            let cl = command.to_ascii_lowercase();
            PRIV_TOOLS.iter().find(|t| cl.contains(**t)).copied()
        };
        if let Some(hint) = hint {
            eprintln!(
                "{}",
                crate::term::yellow(&format!(
                    "[pir] possible capability-required operation by the agent: {hint}\n       cmd: {command}\n       (root capabilities are dropped in this container; if legitimate, grant explicitly via /su-security or a parcel rather than letting it be silently denied)"
                ))
            );
            text.push_str(&format!("\n[pir] note: this command appears to need a dropped capability ({hint})"));
        }
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

/// Kill a detached job's WHOLE process group: TERM the group, wait briefly,
/// then KILL any survivors. `Child::kill()` only signals the direct
/// `bash -c` child, so grandchildren (`cargo`, `rustc`, `sleep 30` behind a
/// pipe) survived and kept the output pipes open — which hung `job_kill`'s
/// drain joins forever and wedged the agent loop/REPL. The child was spawned
/// with `process_group(0)`, so its pgid == its pid and `kill(-pgid)` reaches
/// every descendant that didn't create its own group.
///
/// Returns the child's exit status (reaped exactly once, here).
pub(crate) fn kill_process_tree(child: &mut std::process::Child) -> Option<std::process::ExitStatus> {
    #[cfg(unix)]
    {
        let pgid = child.id() as i32;
        // Negative pid => signal the whole group; TERM first so well-behaved
        // children can flush, then a bounded grace period, then KILL for
        // anything still alive. Errors ignored (already exited / reparented /
        // group gone).
        unsafe {
            let _ = libc::kill(-pgid, libc::SIGTERM);
        }
        // Overall budget for the whole kill dance. Every wait below is a
        // *polling* try_wait — never a blocking `child.wait()` — because a
        // SIGKILLed child that is stuck in an uninterruptible sleep (D state,
        // e.g. on a wedged mount) can take arbitrarily long to die; a blocking
        // wait would hold `job_kill` forever and wedge the agent loop/REPL
        // (the "job_kill then silence" hang). We reap exactly once, whenever
        // try_wait first reports the child gone, and give up after the budget.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            match child.try_wait() {
                // TERM (or KILL) landed: this try_wait already reaped the child.
                Ok(Some(status)) => return Some(status),
                // Still alive and TERM's grace is spent: escalate to SIGKILL.
                Ok(None) if std::time::Instant::now() >= deadline => {
                    unsafe {
                        let _ = libc::kill(-pgid, libc::SIGKILL);
                    }
                    let _ = child.kill();
                    // Keep polling; a D-state child may survive KILL for a
                    // while. Never block — drop out once the budget expires and
                    // let the caller (job_kill) report what it knows.
                    let hard = std::time::Instant::now() + std::time::Duration::from_millis(100);
                    loop {
                        match child.try_wait() {
                            Ok(Some(status)) => return Some(status),
                            Ok(None) if std::time::Instant::now() >= hard => return None,
                            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
                            Err(_) => return None,
                        }
                    }
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
                Err(_) => return None,
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
        child.wait().ok()
    }
}

/// Join a drain thread, but never longer than `max`: a pipe whose write end
/// is still held by a surviving/orphaned process would otherwise block this
/// join forever and wedge the agent loop. Returns true if the thread finished
/// in time; if not, it is detached and will die with the process (output
/// already captured stays in the shared buffer).
fn join_drain(h: JoinHandle<()>) -> bool {
    // JoinHandle has no timed join on stable; poll `is_finished()` and join
    // only once the thread is done (join then returns immediately). If the
    // deadline passes, drop the handle — that detaches the thread, which dies
    // with the process, and the already-captured output stays in the buffer.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    loop {
        if h.is_finished() {
            let _ = h.join();
            return true;
        }
        if std::time::Instant::now() >= deadline {
            drop(h); // detach; never block the REPL on a stuck pipe
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
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
    // Hard-kill behind every `timeout`: GNU `timeout` alone sends only
    // SIGTERM, so a child that ignores/traps TERM (or is wedged) survives the
    // model's `timeout 120 cargo test`, keeps the stdout pipe open, and hangs
    // the `| tail` consumer — which is exactly how a "timeout 120" runs 400s+
    // in the field. We don't parse or rewrite the command text (fragile,
    // quoting-aware); instead every pir-spawned `bash -c` sources `BASH`
    // (non-interactive shells read it), where the model's timeout is wrapped
    // with `-k 5` (KILL 5s after the TERM if still alive). User-defined
    // overrides in the same file win over this default.
    #[cfg(unix)]
    {
        let script = r#"
__pir_timeout_default() { command timeout -k 5 "$@"; }
timeout() { __pir_timeout_default "$@"; }
"#;
        let stash_dir = std::env::temp_dir().join("pir-timeout");
        let _ = fs::create_dir_all(&stash_dir);
        let path = stash_dir.join("bashenv");
        let exists = fs::read_to_string(&path).map(|s| s.contains("__pir_timeout_default")).unwrap_or(false);
        if !exists {
            let _ = fs::write(&path, script);
        }
    }
    let build = |prog: &str, flag: &str| {
        let mut c = Command::new(prog);
        c.arg(flag).arg(command);
        c.current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // Put the command in its OWN process group (item: don't let a runaway
        // command share pir's foreground process group, so killing the search
        // kills only the search, not pir). On unix this uses setpgid(0,0);
        // `process_group(0)` is the cross-platform way to request it.
        #[cfg(unix)]
        c.process_group(0);
        // Source the hard-kill timeout wrapper (see above): every
        // non-interactive `bash -c` sources `$BASH_ENV`. (`BASH` itself can't
        // be used — bash overwrites it with its own path.) `sh -c` fallbacks
        // don't read it but that path is rare and this wrapper is
        // defence-in-depth, not the primary containment.
        #[cfg(unix)]
        {
            let path = std::env::temp_dir().join("pir-timeout/bashenv");
            if path.is_file() {
                c.env("BASH_ENV", path);
            }
        }
        // Confine this command to the per-project `ai_X` sandbox user. This is
        // the "drop privs for the agents, not the user" boundary: `pir` itself
        // stays the invoking identity (so the operator keeps authority), but
        // every command the model spawns is confined to the agent exec user in
        // its child `before_exec`, with the saved uid collapsed so it can never
        // escalate back. No-op when no agent user is configured (plain `pir`).
        #[cfg(unix)]
        c.before_exec(crate::user::drop_to_agent_user);
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

#[cfg(test)]
mod esc_tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    #[test]
    fn abort_kills_running_command() {
        // A slow command (`sleep 30`). Flipping the abort flag must kill it
        // almost immediately (well under a second), not wait for it to exit.
        let abort = Arc::new(AtomicBool::new(false));
        let mut b = Builtin::new(PathBuf::from("."), true, abort.clone(), Arc::new(AtomicBool::new(false)));
        let start = Instant::now();
        let handle = std::thread::spawn(move || run_shell(&mut b, "sleep 30"));
        std::thread::sleep(Duration::from_millis(200));
        abort.store(true, Ordering::SeqCst);
        let out = handle.join().unwrap().expect("run_shell result");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "abort took too long ({elapsed:?}) — command should be killed promptly"
        );
        assert!(out.contains("aborted by user"), "aborted command should report user abort, got: {out}");
    }

    #[test]
    fn no_abort_completes() {
        // Without abort, a fast command runs to completion and reports success.
        let abort = Arc::new(AtomicBool::new(false));
        let mut b = Builtin::new(PathBuf::from("."), true, abort.clone(), Arc::new(AtomicBool::new(false)));
        let out = run_shell(&mut b, "echo hello-from-pir").expect("run_shell result");
        assert!(out.contains("hello-from-pir"), "got: {out}");
    }

    #[test]
    fn abort_only_noop_when_idle() {
        // The abort flag alone (no running command) must not abort a later one.
        let abort = Arc::new(AtomicBool::new(false));
        abort.store(true, Ordering::SeqCst);
        abort.store(false, Ordering::SeqCst);
        let mut b = Builtin::new(PathBuf::from("."), true, abort.clone(), Arc::new(AtomicBool::new(false)));
        let out = run_shell(&mut b, "echo still-ran").expect("run_shell result");
        assert!(out.contains("still-ran"), "got: {out}");
    }

    #[test]
    fn job_kill_survives_grandchild_holding_pipe() {
        // The exact production hang: a detached `sleep` keeps running with its
        // output redirected (it inherits and holds the stdout/stderr pipes).
        // The old job_kill only killed the direct `bash -c` child and then
        // joined the drain threads, which blocked forever on EOF that never
        // came — job_kill never returned, the agent loop wedged, and the REPL
        // never came back. Regression: the group kill + bounded joins must
        // make job_kill return promptly.
        std::env::set_var("PIR_SHELL_CHECK_IN_SECS", "1");
        let mut b = Builtin::new(PathBuf::from("."), true, Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let start = Instant::now();
        let detached = run_shell(&mut b, "sleep 60 > /dev/null 2>&1 & echo detached-ok; sleep 60").expect("run_shell result");
        assert!(detached.contains("[detached]"), "expected detachment, got: {detached}");
        let out = b.job_kill(&json!({ "id": 1 })).expect("job_kill must return");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(10),
            "job_kill took {elapsed:?} — the old forever-join hang is back"
        );
        assert!(out.contains("job#1"), "unexpected job_kill reply: {out}");
    }

    #[test]
    fn job_kill_reports_signal_not_success() {
        // Killing a running job should report it was stopped by a signal, not
        // a success exit code (previously `s.code()` on a signal death was
        // surfaced as -1, reading like a normal failure).
        std::env::set_var("PIR_SHELL_CHECK_IN_SECS", "1");
        let mut b = Builtin::new(PathBuf::from("."), true, Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let detached = run_shell(&mut b, "sleep 60").expect("run_shell result");
        assert!(detached.contains("[detached]"), "expected detachment, got: {detached}");
        let out = b.job_kill(&json!({ "id": 1 })).expect("job_kill must return");
        assert!(
            out.contains("signal") || out.contains("stopped"),
            "expected a stopped/killed report, got: {out}"
        );
        assert!(!out.contains("exit 0"), "signal death must not read as success, got: {out}");
    }

    #[test]
    fn esc_flag_sweep_kills_detached_jobs() {
        std::env::set_var("PIR_SHELL_CHECK_IN_SECS", "1");
        let job_kill = Arc::new(AtomicBool::new(false));
        let mut b = Builtin::new(PathBuf::from("."), true, Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        b.set_job_kill_handle(job_kill.clone());
        // Detach job #1 (a long command), then job #2 (its parent also hangs).
        let d1 = run_shell(&mut b, "sleep 60").expect("detach 1");
        assert!(d1.contains("[detached]"), "got: {d1}");
        let d2 = run_shell(&mut b, "sleep 60 & sleep 60").expect("detach 2");
        assert!(d2.contains("[detached]"), "got: {d2}");
        assert_eq!(b.jobs.len(), 2, "two detached jobs must be tracked");
        // ESC semantics: flip the shared flag, the wait loop consumes it and
        // sweeps every detached job.
        job_kill.store(true, Ordering::SeqCst);
        assert!(b.job_kill.swap(false, Ordering::SeqCst), "flag must be observable");
        assert!(!b.job_kill.load(Ordering::SeqCst), "flag must be consumed");
        let killed = b.kill_all_jobs();
        assert_eq!(killed, 2, "both running jobs must be killed");
        assert!(b.jobs.is_empty(), "sweep must drop every detached job");
    }

    #[test]
    fn kill_all_jobs_via_trait_kills_running_children() {
        std::env::set_var("PIR_SHELL_CHECK_IN_SECS", "1");
        let mut b = Builtin::new(PathBuf::from("."), true, Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let d = run_shell(&mut b, "sleep 60").expect("detach");
        assert!(d.contains("[detached]"), "got: {d}");
        let killed = ToolBackend::kill_all_jobs(&mut b);
        assert_eq!(killed, 1, "the running job must be reported killed");
        assert!(b.jobs.is_empty());
    }

    #[test]
    fn job_kill_after_finish_reports_already_finished() {
        std::env::set_var("PIR_SHELL_CHECK_IN_SECS", "1");
        let mut b = Builtin::new(PathBuf::from("."), true, Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        // 2s command, 1s check-in: guaranteed to detach before it can finish.
        let detached = run_shell(&mut b, "sleep 2").expect("run_shell result");
        assert!(detached.contains("[detached]"), "expected detachment, got: {detached}");
        // Wait for the job to finish on its own, then kill the corpse.
        std::thread::sleep(Duration::from_millis(2500));
        let out = b.job_kill(&json!({ "id": 1 })).expect("job_kill must return");
        assert!(out.contains("already finished"), "got: {out}");
    }

    #[test]
    fn job_kill_escaped_grandchild_holding_pipe_returns_promptly() {
        // The remaining production hang after the process-group fix: a
        // grandchild that calls `setsid` puts itself in a NEW session/process
        // group, so `kill(-pgid)` never reaches it. It also keeps the
        // stdout/stderr pipe write-ends open (it didn't redirect them), so the
        // drain threads block forever. The old kill_process_tree then did a
        // *blocking* `child.wait()` after SIGKILL — but the `bash -c` child may
        // linger in an uninterruptible sleep waiting on that still-open pipe,
        // so the wait never returns and job_kill wedges the agent loop/REPL
        // (the "job_kill then silence" hang). Regression: the whole kill dance
        // must stay poll-based and bounded, so job_kill returns promptly even
        // when a child won't die on the first KILL.
        std::env::set_var("PIR_SHELL_CHECK_IN_SECS", "1");
        let mut b = Builtin::new(PathBuf::from("."), true, Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let start = Instant::now();
        // `setsid` escapes the group AND inherits (holds) stdout/stderr.
        let detached = run_shell(
            &mut b,
            "setsid bash -c 'while true; do echo alive; sleep 1; done' & echo detached-ok; sleep 60",
        )
        .expect("run_shell result");
        assert!(detached.contains("[detached]"), "expected detachment, got: {detached}");
        let out = b.job_kill(&json!({ "id": 1 })).expect("job_kill must return");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "job_kill took {elapsed:?} — the escaped-grandchild hang is back"
        );
        assert!(out.contains("job#1"), "unexpected job_kill reply: {out}");
    }
}

#[cfg(test)]
mod priv_escalation_tests {
    use super::*;

    // sudo/su/doas shapes are detected and labelled with the target user.
    #[test]
    fn detects_sudo_su_doas_escalation() {
        assert_eq!(Builtin::priv_escalation("sudo ls"), Some("sudo as root".into()));
        assert_eq!(
            Builtin::priv_escalation("sudo -u ai_x id"),
            Some("sudo as ai_x".into())
        );
        assert_eq!(
            Builtin::priv_escalation("sudo -i"),
            Some("sudo as root (login shell)".into())
        );
        assert_eq!(Builtin::priv_escalation("su"), Some("su to root".into()));
        assert_eq!(Builtin::priv_escalation("su postgres"), Some("su to postgres".into()));
        assert_eq!(
            Builtin::priv_escalation("doas reboot"),
            Some("doas (privilege escalation)".into())
        );
    }

    // Inline escalation buried inside `bash -c '…'` is still caught.
    #[test]
    fn detects_inline_sudo_in_bash_c() {
        assert!(Builtin::priv_escalation("bash -c 'sudo rm -rf /'").is_some());
        assert!(Builtin::priv_escalation("echo hi && su root").is_some());
    }

    // Ordinary commands must NOT be mistaken for escalation.
    #[test]
    fn no_false_positive_for_plain_commands() {
        assert_eq!(Builtin::priv_escalation("echo mesudo"), None);
        assert_eq!(Builtin::priv_escalation("cargo build"), None);
        assert_eq!(Builtin::priv_escalation("git status"), None);
        assert_eq!(Builtin::priv_escalation("sudoedit"), Some("sudo as root".into()));
    }

    // Non-interactive sudo probe must be non-blocking and only true when
    // passwordless sudo is genuinely available. On a box where it is not, it
    // returns false (so the escalation is refused rather than hanging).
    #[test]
    fn passwordless_probe_does_not_block() {
        let b = Builtin::new(PathBuf::from("."), true, Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        // Just assert it returns promptly and deterministically; the actual
        // value depends on the test-runner's sudoers, not the code.
        let _ = b.sudo_is_passwordless("sudo ls");
    }
}
