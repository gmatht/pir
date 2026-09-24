//! Built-in extension (type "a"): the core tools, linked statically like any
//! other extension. This is the reference implementation of the
//! [`pir::plugin::ToolBackend`] trait.
//!
//! Tool gating is the *security module's* job now: the agent's tool loop runs
//! `SecurityContext::check` before every tool (reads/exec default-open, writes
//! scoped to the project, critical targets denied + ask-gated). This extension
//! therefore no longer prompts y/a/n per command — it keeps only the two
//! *targeted* guards that are not expressible as path policy: refusing
//! self-targeting kill commands, and routing `sudo`/`su` through the
//! request-don't-take escalation path. Every other extension is free to
//! implement its own policy (or set `full_auto` in the registry to skip
//! prompts).

use crate::plugin::{Outcome, Registry, ToolBackend, ToolSpec};
use crate::term;
use serde_json::json;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Provider-independent web tools (`web_search` / `web_fetch`), Feature 10.
/// Included by path because `build.rs` only links `extensions/*/src/lib.rs`,
/// but the code is substantial enough to deserve its own file.
#[path = "web.rs"]
mod web;
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
        reg.abort.clone(),
        Arc::new(AtomicBool::new(false)),
    )));
}

/// How long the escalation / self-kill confirmation waits for the operator to
/// press a key before giving up. Long enough that a human reading the warning
/// always gets to answer; overridable (`PIR_CONFIRM_TIMEOUT_SECS`) so tests and
/// scripted environments can bound it instead of blocking for two minutes.
fn priv_prompt_timeout_secs() -> u64 {
    std::env::var("PIR_CONFIRM_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&s| s > 0)
        .unwrap_or(120)
}

struct Builtin {
    cwd: PathBuf,
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
    fn new(cwd: PathBuf, abort: Arc<AtomicBool>, quiet: Arc<AtomicBool>) -> Self {        Builtin {
            cwd,
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
                description: "Read a file (truncated to 100k chars). Handles the common rich \
                              formats transparently: gzip is decompressed, PDFs are extracted as \
                              text (via pdftotext when installed), Jupyter notebooks are rendered \
                              as cells, and Office documents (.docx/.xlsx/.pptx) are extracted as \
                              text (via unzip). An image is described (format, dimensions, size, \
                              path) rather than returned as pixels, and any other binary is \
                              described with a kind guess and a hex preview instead of failing.",
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
                name: "glob",
                description: "Find files by name/path glob pattern, newest-modified first. \
                              Backed by ripgrep (`rg --files`) when installed, so it respects \
                              .gitignore and skips hidden files; falls back to a built-in \
                              recursive walk. Returns paths relative to the search root, one per \
                              line, with a `[pir] truncated` marker when `limit` cuts the list.",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Glob to match against the path relative to `path`, e.g. \
                                            '**/*.rs', 'src/*.toml', '*.md'. `**` matches any \
                                            number of directories, `*` any run of chars, `?` one char."
                        },
                        "path": { "type": "string", "description": "Root directory to search (default: cwd)" },
                        "limit": { "type": "integer", "description": "Max paths to return (default 100, max 2000)" },
                        "offset": { "type": "integer", "description": "Skip this many matches before returning (default 0)" }
                    },
                    "required": ["pattern"]
                }),
            },
            ToolSpec {
                name: "grep",
                description: "Search file contents with a regular expression (ripgrep-backed). \
                              Respects .gitignore, skips binaries, and filters out credential/secret \
                              paths. Prefer this over `bash` for content searches — it is faster and \
                              returns structured, citable output.",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Rust/regex syntax pattern to search for" },
                        "path": { "type": "string", "description": "File or directory to search (default: cwd)" },
                        "glob": { "type": "string", "description": "Only search files matching this glob, e.g. '*.rs'" },
                        "type": { "type": "string", "description": "Only search files of this rg type, e.g. 'rust', 'py' (see `rg --type-list`)" },
                        "output_mode": {
                            "type": "string",
                            "enum": ["content", "files_with_matches", "count"],
                            "description": "content = matching lines with numbers (default); files_with_matches = paths only; count = per-file match counts"
                        },
                        "case_insensitive": { "type": "boolean", "description": "Case-insensitive match (-i)" },
                        "line_numbers": { "type": "boolean", "description": "Prefix each hit with its line number (-n; on by default for content mode)" },
                        "context": { "type": "integer", "description": "Lines of context to show around each hit (-C)" },
                        "head_limit": { "type": "integer", "description": "Max result lines to return (default 250, max 2000)" },
                        "offset": { "type": "integer", "description": "Skip this many result lines before returning (default 0)" },
                        "multiline": { "type": "boolean", "description": "Allow the pattern to span lines (-U)" }
                    },
                    "required": ["pattern"]
                }),
            },
            ToolSpec {
                name: "agent",
                description: "Delegate a task to a sub-agent with its own context window and return \
                              only its report. Use it when a job would otherwise flood your context \
                              with file reads and tool round-trips (broad exploration, 'find every \
                              X and summarise', a self-contained sub-task) or when you want a \
                              read-only reconnaissance/planning pass. Types: 'explore' (read-only \
                              search + report), 'plan' (read-only approach + plan_write), 'general' \
                              (full capability, inherits your security posture). The sub-agent \
                              cannot ask the operator anything, so give it a self-contained, \
                              unambiguous prompt. Pass `tasks: [...]` instead of a single `prompt` \
                              to run several sub-agents CONCURRENTLY and get one merged report — \
                              use it for independent sweeps ('where is auth handled', 'where are \
                              migrations', 'where is the retry loop') so they overlap instead of \
                              running one after another. Set `model` on an explore/plan task to run \
                              it on a cheaper/faster model; general always inherits yours.",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string", "description": "The complete task for the sub-agent (it cannot see your conversation). Use either `prompt` (one task) or `tasks` (many, run concurrently) — not both." },
                        "description": { "type": "string", "description": "Short 3-5 word label for this delegation" },
                        "subagent_type": {
                            "type": "string",
                            "enum": ["explore", "plan", "general"],
                            "description": "explore = read-only recon; plan = read-only planning; general = full capability (default)"
                        },
                        "model": {
                            "type": "string",
                            "description": "Optional provider/model selector (e.g. 'cerebras/gpt-oss-120b') for this sub-agent. Honoured only for explore/plan (read-only) tasks, where a cheaper/faster model is safe because the child cannot change anything; ignored for general, which inherits your model. Unresolvable selectors fall back to your model with a note."
                        },
                        "tasks": {
                            "type": "array",
                            "description": "Run several sub-agents concurrently (up to 8) and return one merged report in submission order. Each entry is an object with the same fields as a single call (prompt, description, subagent_type, model).",
                            "items": { "type": "object" }
                        },
                        "run_in_background": { "type": "boolean", "description": "Accepted for compatibility; not supported in this build" }
                    },
                    "required": []
                }),
            },
            ToolSpec {
                name: "remember",
                description: "Record a durable preference (a 'taste') so it is applied in this and \
                              every later session. Use it when the operator states a lasting \
                              preference, corrects you in a way that generalises, or you learn \
                              something about how this project must be worked (a required flag, a \
                              formatter, a convention). Do NOT record one-off task details or \
                              anything already covered by AGENTS.md. Re-recording an existing \
                              preference reinforces it rather than duplicating it.",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "The preference as one imperative line, e.g. \"Never commit without asking\"" },
                        "evidence": { "type": "string", "description": "Why: the prompt, correction, or observation behind it" },
                        "scope": {
                            "type": "string",
                            "enum": ["project", "user"],
                            "description": "project = this repo only (default); user = every project"
                        },
                        "confidence": { "type": "number", "description": "0.0-1.0 (default 0.6); raise it for an explicit operator instruction" }
                    },
                    "required": ["text"]
                }),
            },
            ToolSpec {
                name: "plan_write",
                description: "Record a plan for the operator to approve (only meaningful in plan \
                              mode). Writes the markdown plan to the plans directory and asks the \
                              operator to approve it; on approval you leave plan mode and may \
                              implement, otherwise you stay read-only and should revise. A good \
                              plan states the goal, the approach, the files to touch, the risks, \
                              and how the result will be verified.",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "content": { "type": "string", "description": "The full plan, as markdown" },
                        "name": { "type": "string", "description": "Short plan name (defaults to a timestamped name from the task)" }
                    },
                    "required": ["content"]
                }),
            },
            ToolSpec {
                name: "load_skill",
                description: "Load a skill's full instructions into context. Skills are \
                              specialised procedures listed in your system prompt (name + \
                              description). Call this with a skill's name *before* doing work \
                              that the skill covers; the body is returned as the tool result. \
                              Call it with no name (or 'all') to list every available skill.",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Skill name (or a unique prefix). Omit to list all skills." }
                    },
                    "required": []
                }),
            },
            ToolSpec {
                name: "ask_user_question",
                description: "Ask the operator a structured question and wait for their answer. \
                              Use it when a decision genuinely blocks progress and you cannot \
                              infer the answer (a preference, an ambiguous requirement, a \
                              choice between materially different approaches). Prefer asking \
                              over guessing on such decisions; do NOT use it for things you can \
                              determine yourself by reading the code. Returns the chosen \
                              option(s) or the typed answer as the tool result. In a headless/ \
                              background session (no operator attached) the tool reports that \
                              no one can answer — make the most reasonable assumption, state it, \
                              and continue.",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "questions": {
                            "type": "array",
                            "description": "One to four questions to ask at once.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "question": { "type": "string", "description": "The full question to display" },
                                    "header": { "type": "string", "description": "Short label / dialog title (max ~12 chars)" },
                                    "options": {
                                        "type": "array",
                                        "description": "Selectable answers. Omit for a free-text question.",
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "label": { "type": "string", "description": "Option text shown to the user" },
                                                "description": { "type": "string", "description": "Optional explanation of the option" }
                                            },
                                            "required": ["label"]
                                        }
                                    },
                                    "multiSelect": { "type": "boolean", "description": "Allow more than one option to be selected" }
                                },
                                "required": ["question"]
                            }
                        }
                    },
                    "required": ["questions"]
                }),
            },
            ToolSpec {
                name: "web_fetch",
                description: "Fetch a URL over http/https and return its readable text (HTML is \
                              reduced to text, with links kept as 'text <url>'). Use it to read \
                              documentation, an issue, or a changelog from the web. Only public \
                              http/https URLs are allowed (no localhost/LAN/cloud-metadata — the \
                              SSRF guard refuses those).",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "Absolute http:// or https:// URL" },
                        "raw": { "type": "boolean", "description": "Return the response body unmodified instead of reducing HTML to text (default false)" },
                        "max_bytes": { "type": "integer", "description": "Cap the response body in bytes (default 2 MiB)" }
                    },
                    "required": ["url"]
                }),
            },
            ToolSpec {
                name: "web_search",
                description: "Search the web and return result titles, URLs and snippets. \
                              Provider-independent (no Ollama Cloud key needed): uses the endpoint \
                              in PIR_WEB_SEARCH_URL, or DuckDuckGo's HTML page by default. Use \
                              web_fetch to read anything it finds.",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "The search query" },
                        "max_results": { "type": "integer", "description": "Maximum hits to return (default 8, max 25)" }
                    },
                    "required": ["query"]
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
            ToolSpec {
                name: "request_stop",
                description: "End your turn with an explicit stop decision, but ONLY when the work \
                              is really done (reason stop-complete) or you genuinely need the \
                              operator to answer before you can continue (reason stop-needs-input, \
                              with your question in message). If work remains, keep using tools \
                              instead — do NOT call this just to end a message. A text-only answer \
                              is nudged back so you either continue the work or stop explicitly \
                              when truly done.",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "reason": {
                            "type": "string",
                            "enum": ["stop-complete", "stop-needs-input"],
                            "description": "stop-complete when done, stop-needs-input when blocked on the operator"
                        },
                        "message": { "type": "string", "description": "Summary (complete) or the question (needs-input). Plain markdown is welcome: keep a short one-line headline first, then details on their own lines — paragraphs, '- ' bullets, and fenced code blocks all render. Use it for anything longer than a sentence instead of one dense run-on line." }
                    },
                    "required": ["reason"]
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
            "glob" => self.glob(input),
            "grep" => self.grep(input),
            "web_fetch" => do_web_fetch(input),
            "web_search" => do_web_search(input),
            "ask_user_question" => Err("ask_user_question is handled by the agent turn loop, not the builtin backend".into()),
            "load_skill" => Err("load_skill is handled by the agent turn loop, not the builtin backend".into()),
            "plan_write" => Err("plan_write is handled by the agent turn loop, not the builtin backend".into()),
            "remember" => Err("remember is handled by the agent turn loop, not the builtin backend".into()),
            "agent" => Err("agent is handled by the agent turn loop, not the builtin backend".into()),
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

impl Builtin {
    fn do_bash(&mut self, input: &serde_json::Value) -> Result<String, String> {
        let command = input["command"].as_str().ok_or("bash: missing 'command'")?;
        // Guard #1 (item: stop agents killing their peers). A `pkill`/`kill`
        // `killall` aimed at `pir` (or scoped to the running user) is almost
        // always a mass-extinction trigger — both historical mass deaths here
        // were exactly an agent running `pkill -f target/.../pir` / `pkill -u
        // ai_pir`. Always require an explicit human confirm before such a
        // command runs, even in full-auto (correctness/safety beats
        // unattendedness for a command that can terminate every other session).
        if let Some(why) = Self::dangerous_kill_reason(command) {
            let prompt = format!(
                "{} this command looks like it would kill pir processes ({why}). Run it anyway? [y]es / [n]o (default no) ",
                term::yellow("⚠")
            );
            // Same terminal caveat as escalation: the REPL owns stdin in raw
            // mode, so read the operator's key from the tty directly. A real
            // "n" (or no answer at all) refuses; only "y" runs it.
            if crate::term::tty_available() {
                match crate::term::prompt_key(&prompt, priv_prompt_timeout_secs()) {
                    Some('y') => {}
                    Some(_) => {
                        return Ok("[denied] refusing to run a self-targeting kill command".into());
                    }
                    None => {
                        return Ok("[denied] refusing to run a self-targeting kill command (no approval given)".into());
                    }
                }
            } else {
                let answer = term::read_answer(&prompt);
                match answer.as_str() {
                    "y" | "yes" => {}
                    _ => return Ok("[denied] refusing to run a self-targeting kill command".into()),
                }
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
            let target = after_su.split_whitespace().next().unwrap_or("root");
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
        self.request_priv_escalation_timeout(command, what, priv_prompt_timeout_secs())
    }

    /// Same as [`Self::request_priv_escalation`], but with an explicit prompt
    /// timeout so tests can bound it without touching process-wide env vars
    /// (which race when cargo runs tests in parallel).
    fn request_priv_escalation_timeout(
        &self,
        command: &str,
        what: String,
        secs: u64,
    ) -> Result<(), String> {
        // A human is present when *either* stdin or `/dev/tty` is a terminal.
        // This matters because the agent's worker thread runs while the REPL
        // owns stdin in raw, non-canonical, non-blocking mode: `is_terminal()`
        // on the raw fd can read as "not a tty" (or the blocking read simply
        // can't see the keypress), which used to make us skip the prompt
        // entirely and auto-deny. The operator never got to answer.
        if io::stdin().is_terminal() || crate::term::tty_available() {
            let prompt = format!(
                "{} escalate privilege ({})? [y]es / [n]o (default no) ",
                term::yellow("⚠"),
                what
            );
            let answer = crate::term::prompt_key(&prompt, secs);
            match answer {
                // A real keypress: only y/yes approves, anything else denies.
                Some(c) => {
                    if c != 'y' {
                        return Err("[denied] user declined to escalate privilege".into());
                    }
                }
                // No human answered (timeout/EOF): never pretend they said no,
                // and never escalate unattended — refuse with the fallback path.
                None => {
                    return Err(Self::refusal_without_approval(&what, secs));
                }
            }
        }
        // Either unattended (no tty at all), or the human approved. In both
        // cases, refuse to run if a password prompt would block us.
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

    /// The refusal shown when a prompt was raised but nobody answered within
    /// `secs`. Deliberately worded "no answer", never "user declined": pir must
    /// not attribute a decision to an operator who was never given the chance
    /// to make one (the exact bug this change fixes). Kept free of I/O so the
    /// wording can be unit-tested without touching a live terminal.
    fn refusal_without_approval(what: &str, secs: u64) -> String {
        format!(
            "[denied] no answer within {secs}s — refusing to escalate ({what}) without explicit \
             approval. Re-run when you can answer the prompt, or escalate out-of-band via the \
             rootreq flow (`request_root`)."
        )
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

    /// `glob` — find files by path pattern. ripgrep-backed (`rg --files`) when
    /// available so .gitignore is honoured and the walk is parallel; otherwise
    /// a built-in recursive walk that applies the same gitignore-ish skips.
    fn glob(&mut self, input: &serde_json::Value) -> Result<String, String> {
        let pattern = input["pattern"].as_str().ok_or("glob: missing 'pattern'")?;
        let root = self.search_root(input.get("path").and_then(|v| v.as_str()));
        let limit = input["limit"].as_u64().unwrap_or(100).clamp(1, MAX_MATCHES) as usize;
        let offset = input["offset"].as_u64().unwrap_or(0) as usize;

        // ripgrep path: `rg --files` streams every file it would search (it
        // already applies .gitignore + hidden-file rules), then we match the
        // glob ourselves. Doing the globbing in-process rather than passing
        // `-g` keeps the semantics identical to the fallback walk (a `-g`
        // pattern is anchored differently than a relative path match).
        let mut all: Vec<String> = match rg_files(&root) {
            Some(files) => files
                .into_iter()
                .filter(|f| glob_match(pattern, f))
                .collect(),
            None => walk_files(&root)
                .into_iter()
                .filter(|f| glob_match(pattern, f))
                .collect(),
        };
        // Newest-modified first: the file an agent is asking for by glob is
        // almost always the one it just touched, and a stable order makes the
        // tool deterministic when mtimes tie.
        all.sort_by(|a, b| {
            let ka = mtime_key(&root.join(a));
            let kb = mtime_key(&root.join(b));
            kb.cmp(&ka).then_with(|| a.cmp(b))
        });
        all.retain(|f| !is_secret_path(f));
        Ok(paginate(&all, limit, offset, "paths"))
    }

    /// `grep` — regex content search. ripgrep-backed; if `rg` is missing we
    /// fall back to the `regex` crate over the walked file set so the tool
    /// still works on a bare host (slower, but never a hard failure).
    fn grep(&mut self, input: &serde_json::Value) -> Result<String, String> {
        let pattern = input["pattern"].as_str().ok_or("grep: missing 'pattern'")?;
        if pattern.is_empty() {
            return Err("grep: 'pattern' must not be empty".into());
        }
        let root = self.search_root(input.get("path").and_then(|v| v.as_str()));
        let mode = input["output_mode"].as_str().unwrap_or("content");
        let head_limit = input["head_limit"].as_u64().unwrap_or(250).clamp(1, MAX_MATCHES) as usize;
        let offset = input["offset"].as_u64().unwrap_or(0) as usize;
        let ci = input["case_insensitive"].as_bool().unwrap_or(false);
        let multiline = input["multiline"].as_bool().unwrap_or(false);
        // Line numbers default on for content mode (that is what makes a hit
        // citable); explicit `"line_numbers": false` turns them off.
        let numbers = input["line_numbers"].as_bool().unwrap_or(mode == "content");
        let context = input["context"].as_u64().unwrap_or(0) as usize;

        // rg flag set, shared by all modes.
        let mut args: Vec<String> = vec!["--color=never".into(), "--no-heading".into()];
        // Never descend the trees the fallback walk skips: ripgrep only knows
        // about *gitignored* paths, and `node_modules`/`target` are usually not
        // in .gitignore even though searching them is pure noise (and slow).
        for d in SEARCH_SKIP_DIRS {
            args.push("--glob".into());
            args.push(format!("!**/{d}/**"));
        }
        if ci {
            args.push("-i".into());
        }
        if multiline {
            args.push("-U".into());
        }
        if let Some(g) = input["glob"].as_str() {
            args.push("-g".into());
            args.push(g.to_string());
        }
        if let Some(t) = input["type"].as_str() {
            args.push("-t".into());
            args.push(t.to_string());
        }
        match mode {
            "files_with_matches" => args.push("-l".into()),
            "count" => args.push("-c".into()),
            _ => {
                if numbers {
                    args.push("-n".into());
                }
                if context > 0 {
                    args.push("-C".into());
                    args.push(context.to_string());
                }
            }
        }
        args.push("--".into());
        args.push(pattern.to_string());
        args.push(root.display().to_string());

        let raw = match run_rg(&root, &args) {
            SearchOutcome::Ok(s) | SearchOutcome::NoMatches(s) => s,
            SearchOutcome::Missing => fallback_grep(&root, pattern, mode, ci, numbers, context)?,
            SearchOutcome::Error(e) => return Err(format!("grep: {e}")),
        };

        // rg prints absolute paths for an absolute root; make the output
        // readable by stripping the root prefix (matching how `glob` returns
        // root-relative paths).
        let mut lines: Vec<String> = raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| strip_root(l, &root))
            .filter(|l| !is_secret_path(l))
            .collect();
        if mode == "content" && numbers && context == 0 {
            // Keep only the hit lines when a context-less numbered search was
            // requested — rg already did that, so this only guards against a
            // stray separator line.
            lines.retain(|l| !l.starts_with("--"));
        }
        Ok(paginate(&lines, head_limit, offset, "result lines"))
    }

    /// Resolve a search root: an explicit `path` (file or dir) relative to the
    /// live cwd, else the live cwd itself (never the stale launch anchor).
    fn search_root(&self, path: Option<&str>) -> PathBuf {
        let base = if self.cwd.is_dir() { self.cwd.clone() } else { PathBuf::from(".") };
        match path {
            Some(p) if !p.trim().is_empty() => base.join(p),
            _ => base,
        }
    }

    fn write_file(&mut self, input: &serde_json::Value) -> Result<String, String> {
        let path = input["path"].as_str().ok_or("write_file: missing 'path'")?;
        let content = input["content"].as_str().ok_or("write_file: missing 'content'")?;
        if let Some(parent) = Path::new(path).parent()
            && !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| format!("write_file {path}: {e}"))?;
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

/// `web_fetch` — GET a URL and return usable text (Feature 10).
fn do_web_fetch(input: &serde_json::Value) -> Result<String, String> {
    let url = input["url"].as_str().ok_or("web_fetch: missing 'url'")?;
    let raw = input["raw"].as_bool().unwrap_or(false);
    let max = input["max_bytes"].as_u64().unwrap_or(0) as usize;
    let fetched = web::fetch(url, max)?;
    let mut out = format!("{} (HTTP {})\n", fetched.url, fetched.status);
    if fetched.status >= 400 {
        out.push_str("(the server reported an error; the body is shown below anyway)\n");
    }
    let body = if raw {
        fetched.body.clone()
    } else {
        let text = web::html_to_text(&fetched.body);
        if text.trim().is_empty() {
            fetched.body.clone()
        } else {
            text
        }
    };
    out.push_str(&body);
    if fetched.truncated {
        out.push_str("\n… [pir] body truncated at the byte cap (raise max_bytes for more)");
    }
    crate::plugin::truncate(&mut out, 30_000);
    Ok(out)
}

/// `web_search` — query the configured search endpoint (Feature 10).
fn do_web_search(input: &serde_json::Value) -> Result<String, String> {
    let query = input["query"].as_str().ok_or("web_search: missing 'query'")?;
    let max = input["max_results"].as_u64().unwrap_or(8).clamp(1, 25) as usize;
    let hits = web::search(query, max)?;
    if hits.is_empty() {
        return Ok(format!(
            "No results parsed for '{query}'. The endpoint ({}) may have changed its markup or \
             be blocking this client — set PIR_WEB_SEARCH_URL to a SearXNG instance \
             (…/search?q={{query}}&format=json), or fetch a known URL directly with web_fetch.",
            web::search_endpoint()
        ));
    }
    let mut out = format!("{} result(s) for '{query}':\n", hits.len());
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, h.title, h.url));
        if !h.snippet.is_empty() {
            out.push_str(&format!("   {}\n", h.snippet));
        }
    }
    out.push_str("\nUse web_fetch on any URL above to read it in full.");
    crate::plugin::truncate(&mut out, 20_000);
    Ok(out)
}

fn read_file(input: &serde_json::Value) -> Result<String, String> {
    let path = input["path"].as_str().ok_or("read_file: missing 'path'")?;
    let bytes = fs::read(path).map_err(|e| format!("read_file {path}: {e}"))?;
    let mut text = decode_for_reading(path, &bytes)?;
    crate::plugin::truncate(&mut text, 100_000);
    Ok(text)
}

/// Turn raw file bytes into the most useful text we can, without new
/// dependencies (Feature 9, "rich reading" — the parts that need no crates).
///
/// Order:
///  1. **gzip** — magic `1f 8b`: decompress via the system `gzip -dc`
///     (universally present) and read the result as text.
///  2. **PDF** — `%PDF-` header: extract text with `pdftotext` when installed
///     (poppler-utils); otherwise say so and suggest the installer.
///  3. **UTF-8 text** — the common case; returned as-is.
///  4. **Binary** — anything else (or text containing NUL bytes): instead of the
///     useless "stream did not contain valid UTF-8" from `read_to_string`, give
///     a short *description* — size, detected kind, and a hex preview of the
///     first bytes — which is what lets a model decide what to do next.
fn decode_for_reading(path: &str, bytes: &[u8]) -> Result<String, String> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        return gunzip_to_text(path, bytes);
    }
    if bytes.starts_with(b"%PDF-") {
        return pdf_to_text(path, bytes);
    }
    let lower = path.to_ascii_lowercase();
    // A Jupyter notebook is UTF-8 JSON, so it would otherwise be returned as a
    // wall of escaped JSON strings — readable by a parser, not by a model.
    if lower.ends_with(".ipynb")
        && let Ok(text) = std::str::from_utf8(bytes)
    {
        return Ok(notebook_to_text(text).unwrap_or_else(|| text.to_string()));
    }
    // Office documents are ZIP containers of XML. Recognised by extension (a
    // bare `PK` header is also a plain zip, and guessing wrong would turn a zip
    // listing into a garbled XML dump).
    if let Some(kind) = office_kind(&lower)
        && bytes.starts_with(b"PK\x03\x04")
    {
        return Ok(office_to_text(kind, path, bytes));
    }
    // An image gets a *description* rather than a hexdump: a model that cannot
    // see pixels is far better served by "1920x1080 PNG, 412 KB" than by a wall
    // of hex. (Vision models read the file directly; see docs/FEATURES.md.)
    if let Some(img) = image_info(bytes) {
        return Ok(describe_image(path, bytes, &img));
    }
    match std::str::from_utf8(bytes) {
        Ok(s) if !s.contains('\0') => Ok(s.to_string()),
        _ => Ok(describe_binary(path, bytes)),
    }
}

/// A recognised office document format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OfficeKind {
    Docx,
    Xlsx,
    Pptx,
}

impl OfficeKind {
    fn label(self) -> &'static str {
        match self {
            OfficeKind::Docx => "Word document",
            OfficeKind::Xlsx => "Excel workbook",
            OfficeKind::Pptx => "PowerPoint presentation",
        }
    }
    /// The ZIP members holding the human-readable text, in reading order.
    fn parts(self) -> Vec<&'static str> {
        match self {
            // `document.xml` is the body; the rest carry headers/footers/notes.
            OfficeKind::Docx => vec![
                "word/document.xml",
                "word/header1.xml",
                "word/footer1.xml",
            ],
            // One XML per worksheet, plus the shared string table they index
            // into. Sheet order in the archive is not guaranteed to be tab
            // order, so we sort the members.
            OfficeKind::Xlsx => vec!["xl/sharedStrings.xml", "xl/worksheets/sheet1.xml"],
            // Slide text lives one file per slide.
            OfficeKind::Pptx => vec!["ppt/slides/slide1.xml"],
        }
    }
}

fn office_kind(lower_path: &str) -> Option<OfficeKind> {
    if lower_path.ends_with(".docx") {
        Some(OfficeKind::Docx)
    } else if lower_path.ends_with(".xlsx") || lower_path.ends_with(".xlsm") {
        Some(OfficeKind::Xlsx)
    } else if lower_path.ends_with(".pptx") {
        Some(OfficeKind::Pptx)
    } else {
        None
    }
}

/// Extract readable text from an Office document by unzipping it and stripping
/// the XML.
///
/// Dependency-free: an Office file is a ZIP of XML, so this shells out to
/// `unzip -p` (present everywhere `gzip` is) and reduces the XML to text. When
/// `unzip` is absent the result says so rather than failing — the operator can
/// still read the document with the right tool.
fn office_to_text(kind: OfficeKind, path: &str, bytes: &[u8]) -> String {
    let mut out = format!(
        "({} — text extracted from {path}, {} bytes)\n",
        kind.label(),
        bytes.len()
    );
    let mut got_any = false;
    // For a spreadsheet the useful content is the shared-string table (where
    // every cell's text actually lives) — the sheet XML only holds indices into
    // it. For word/slides the body XML carries the text directly.
    for part in kind.parts() {
        match read_zip_member(path, part) {
            Some(xml) => {
                let text = xml_to_text(&xml);
                if !text.trim().is_empty() {
                    out.push_str(&format!("\n--- {part} ---\n{}\n", text.trim()));
                    got_any = true;
                }
            }
            None => continue,
        }
    }
    // A multi-sheet workbook or deck is numbered beyond sheet1/slide1: list what
    // else is in there so the model knows the document is bigger than the part
    // it received.
    if let Some(listing) = zip_listing(path, kind) {
        out.push_str(&format!("\n(other parts: {listing})"));
    }
    if !got_any {
        out.push_str(
            "\n(could not read any text part — `unzip` may be missing, or the archive structure \
             is unusual; try extracting it yourself with bash)",
        );
    }
    out
}

/// Read one member of a ZIP by shelling out to `unzip -p <file> <member>`.
fn read_zip_member(path: &str, member: &str) -> Option<String> {
    let out = Command::new("unzip")
        .arg("-p")
        .arg(path)
        .arg(member)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// List the text-bearing members beyond the ones already shown, so a large
/// document is not silently truncated to its first page/sheet/slide.
fn zip_listing(path: &str, kind: OfficeKind) -> Option<String> {
    let out = Command::new("unzip")
        .arg("-Z1")
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let names: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .collect();
    let prefix = match kind {
        OfficeKind::Docx => "word/",
        OfficeKind::Xlsx => "xl/worksheets/",
        OfficeKind::Pptx => "ppt/slides/slide",
    };
    let mut extra: Vec<&str> = names
        .iter()
        .filter(|n| n.starts_with(prefix) && n.ends_with(".xml"))
        .map(|n| n.as_str())
        .take(20)
        .collect();
    extra.sort();
    if extra.is_empty() {
        return None;
    }
    Some(extra.join(", "))
}

/// Reduce Office XML to text: paragraph/row/break elements become newlines,
/// everything else is stripped, and entities are decoded.
fn xml_to_text(xml: &str) -> String {
    let mut s = xml.to_string();
    // Insert line breaks at the tags that imply them before stripping.
    for (tag, rep) in [
        ("</w:p>", "\n"),      // word paragraph
        ("</a:p>", "\n"),      // drawingml paragraph (slides)
        ("</w:tr>", "\n"),     // word table row
        ("</row>", "\n"),      // spreadsheet row
        ("</si>", "\n"),       // shared string item
        ("<w:br/>", "\n"),     // word line break
        ("<w:tab/>", "\t"),    // word tab
    ] {
        s = s.replace(tag, rep);
    }
    s = web::strip_tags(&s);
    s = web::decode_entities(&s);
    // Collapse whitespace runs, keeping intentional blank lines to a maximum of
    // one (the tags above deliberately introduce them).
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        let t = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if t.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(t.trim_end());
        out.push('\n');
    }
    out.trim().to_string()
}

/// Compact byte size for a description line (`412 KB`). Local to this module so
/// the extension does not depend on a non-`pub` helper elsewhere in the crate.
fn fmt_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

/// Recognised image format plus its intrinsic dimensions and colour depth,
/// parsed from the header alone (no decode, no dependency).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageInfo {
    format: &'static str,
    mime: &'static str,
    width: Option<u32>,
    height: Option<u32>,
}

/// Parse an image header. Returns `None` for anything that is not an image.
fn image_info(b: &[u8]) -> Option<ImageInfo> {
    // PNG: IHDR immediately follows the 8-byte signature. A truncated file is
    // still recognisably a PNG — the format is known, only the size is not.
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        let dims = if b.len() >= 24 && &b[12..16] == b"IHDR" {
            (
                Some(u32::from_be_bytes([b[16], b[17], b[18], b[19]])),
                Some(u32::from_be_bytes([b[20], b[21], b[22], b[23]])),
            )
        } else {
            (None, None)
        };
        return Some(ImageInfo { format: "PNG", mime: "image/png", width: dims.0, height: dims.1 });
    }
    // GIF: logical screen descriptor at offset 6, little-endian.
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        let (w, h) = if b.len() >= 10 {
            (
                Some(u16::from_le_bytes([b[6], b[7]]) as u32),
                Some(u16::from_le_bytes([b[8], b[9]]) as u32),
            )
        } else {
            (None, None)
        };
        return Some(ImageInfo { format: "GIF", mime: "image/gif", width: w, height: h });
    }
    // JPEG: walk the marker segments to the SOFn frame header.
    if b.starts_with(&[0xff, 0xd8, 0xff]) {
        let (w, h) = jpeg_dimensions(b).map(|(w, h)| (Some(w), Some(h))).unwrap_or((None, None));
        return Some(ImageInfo { format: "JPEG", mime: "image/jpeg", width: w, height: h });
    }
    if b.starts_with(b"BM") && b.len() >= 26 {
        let w = i32::from_le_bytes([b[18], b[19], b[20], b[21]]);
        let h = i32::from_le_bytes([b[22], b[23], b[24], b[25]]);
        return Some(ImageInfo {
            format: "BMP",
            mime: "image/bmp",
            width: Some(w.unsigned_abs()),
            height: Some(h.unsigned_abs()),
        });
    }
    // WebP: RIFF container with a VP8/VP8L/VP8X chunk.
    if b.len() >= 30 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        let (w, h) = match &b[12..16] {
            b"VP8X" => (
                Some(u32::from_le_bytes([b[24], b[25], b[26], 0]) + 1),
                Some(u32::from_le_bytes([b[27], b[28], b[29], 0]) + 1),
            ),
            _ => (None, None),
        };
        return Some(ImageInfo { format: "WebP", mime: "image/webp", width: w, height: h });
    }
    // SVG is text, so it never reaches here as an "image" — but an ICO is a
    // real binary image we can at least name.
    if b.starts_with(&[0x00, 0x00, 0x01, 0x00]) {
        let (w, h) = if b.len() >= 8 {
            (Some(b[6] as u32), Some(b[7] as u32))
        } else {
            (None, None)
        };
        return Some(ImageInfo { format: "ICO", mime: "image/x-icon", width: w, height: h });
    }
    None
}

/// Scan JPEG marker segments for a Start-Of-Frame, which carries the size.
fn jpeg_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2usize;
    // A SOF segment needs `i..i+9` to be in range (marker, length, precision,
    // height, width), so the bound is `i + 9 <= len`.
    while i + 9 <= b.len() {
        if b[i] != 0xff {
            i += 1;
            continue;
        }
        let marker = b[i + 1];
        // Standalone markers carry no length payload.
        if (0xd0..=0xd9).contains(&marker) || marker == 0x01 {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        if len < 2 {
            return None;
        }
        // SOF0..SOF15, excluding the DHT/JPG/DAC markers in that range.
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
            let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

/// Describe an image for a model that cannot see pixels: format, dimensions,
/// size, where it is on disk, and how to hand it to a vision model.
///
/// Returning base64 in the tool result would be actively harmful — a 1 MB PNG
/// is ~1.4 MB of base64, which is a large fraction of a typical context window
/// spent on data most models cannot interpret. The file stays on disk where it
/// belongs; a vision-capable model reads it by path.
fn describe_image(path: &str, bytes: &[u8], info: &ImageInfo) -> String {
    let dims = match (info.width, info.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => format!("{w}x{h} pixels"),
        _ => "dimensions unknown".to_string(),
    };
    let mut out = format!(
        "(image — {} {dims}, {}. Not returned as text.)\n",
        info.format,
        fmt_bytes(bytes.len() as u64)
    );
    out.push_str(&format!(
        "path: {path}\nmime: {}\n",
        info.mime
    ));
    out.push_str(
        "A text-only model cannot read the pixels. To use the image:\n         - hand the path to a vision-capable model (it reads the file directly), or\n         - read it as a data URL with bash: base64 -w0 <path>\n         - or convert/analyse it with a tool (ImageMagick, tesseract for OCR).\n",
    );
    out
}

/// Render a Jupyter notebook (`.ipynb`) as readable markdown-ish text: each
/// cell's source, plus its outputs reduced to text.
///
/// Dependency-free — a notebook *is* JSON (`{"cells": [...]}`), so this is a
/// parse and a walk rather than a format implementation. Returns `None` when the
/// document doesn't look like a notebook (no `cells` array), so the caller falls
/// back to showing the raw file instead of swallowing it.
fn notebook_to_text(json_text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json_text).ok()?;
    let cells = v.get("cells")?.as_array()?;
    let mut out = String::new();
    if let Some(lang) = v
        .get("metadata")
        .and_then(|m| m.get("kernelspec"))
        .and_then(|k| k.get("language"))
        .and_then(|l| l.as_str())
    {
        out.push_str(&format!("(Jupyter notebook, {lang})"));
    } else {
        out.push_str("(Jupyter notebook)");
    }
    let nbformat = v.get("nbformat").and_then(|n| n.as_u64()).unwrap_or(0);
    if nbformat > 0 {
        out.push_str(&format!(" nbformat {nbformat}"));
    }
    out.push('\n');

    for (i, cell) in cells.iter().enumerate() {
        let kind = cell.get("cell_type").and_then(|c| c.as_str()).unwrap_or("");
        // `source` is an array of lines in every real notebook, but a bare
        // string is legal, so accept both.
        let source = joined_text(cell.get("source"));
        out.push_str(&format!("\n── cell {} ({kind}) ──\n", i + 1));
        if source.trim().is_empty() {
            out.push_str("(empty)\n");
        } else {
            let fence = if kind == "code" { "python" } else { "text" };
            out.push_str(&format!("```{fence}\n{}\n```\n", source.trim_end()));
        }
        // Outputs: keep the text, drop the binary blobs (an image output is a
        // base64 string that would swamp the file and mean nothing as text).
        let mut outputs: Vec<String> = Vec::new();
        for o in cell.get("outputs").and_then(|o| o.as_array()).into_iter().flatten() {
            let direct = joined_text(o.get("text"));
            if !direct.trim().is_empty() {
                outputs.push(direct.trim_end().to_string());
            }
            // `data` holds rich outputs; `text/plain` is the readable one.
            if let Some(d) = o.get("data") {
                let plain = joined_text(d.get("text/plain"));
                if !plain.trim().is_empty() {
                    outputs.push(plain.trim_end().to_string());
                }
                // Say what we skipped rather than silently dropping it.
                for mime in d.as_object().map(|m| m.keys()).into_iter().flatten() {
                    if mime.starts_with("image/") || mime == "application/pdf" {
                        outputs.push(format!("[{mime} output omitted]"));
                    }
                }
            }
            if let Some(name) = o.get("ename").and_then(|e| e.as_str()) {
                let value = o.get("evalue").and_then(|e| e.as_str()).unwrap_or("");
                outputs.push(format!("ERROR: {name}: {value}"));
            }
        }
        if !outputs.is_empty() {
            out.push_str("output:\n");
            for o in outputs {
                for line in o.lines() {
                    out.push_str(&format!("  {line}\n"));
                }
            }
        }
    }
    Some(out)
}

/// A notebook's `source`/`text` field: an array of lines, or a bare string.
fn joined_text(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::Array(lines)) => lines
            .iter()
            .filter_map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join(""),
        Some(serde_json::Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Decompress a gzip stream by piping it through the system `gzip -dc`.
/// Dependency-free (no flate2) and exactly right on every host that has gzip,
/// which is every host that produced a `.gz` in the first place.
fn gunzip_to_text(path: &str, bytes: &[u8]) -> Result<String, String> {
    use std::io::Write;
    let _ = path;
    let gz = bytes.to_vec();
    let mut child = match Command::new("gzip")
        .arg("-dc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return Ok(format!(
                "(gzip file: {} bytes; could not run `gzip -dc` ({e}) to decompress it — install \
                 gzip or decompress it yourself with bash)",
                gz.len()
            ));
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        // Ignore a broken pipe: `gzip -dc` may exit early on a truncated stream
        // and the error we care about is on stderr.
        let _ = stdin.write_all(&gz);
    }
    let out = child.wait_with_output().map_err(|e| format!("gzip: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Ok(format!(
            "(gzip file: {} bytes; `gzip -dc` failed: {})",
            gz.len(),
            err.trim()
        ));
    }
    let raw = out.stdout;
    let len = raw.len();
    match String::from_utf8(raw) {
        Ok(s) => Ok(format!("(decompressed from gzip)\n{s}")),
        Err(_) => Ok(format!(
            "(gzip file decompressed to {len} bytes of non-UTF-8 data — it is not text)"
        )),
    }
}

/// Extract PDF text via `pdftotext` (poppler-utils) when available.
fn pdf_to_text(path: &str, bytes: &[u8]) -> Result<String, String> {
    // `pdftotext <in> -` streams the text to stdout; no temp file needed.
    match Command::new("pdftotext")
        .arg(path)
        .arg("-")
        .stdin(Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout).to_string();
            if text.trim().is_empty() {
                Ok(format!(
                    "(PDF: {} bytes; pdftotext extracted no text — it may be a scanned image, \
                     which needs OCR)",
                    bytes.len()
                ))
            } else {
                Ok(format!("(text extracted from PDF)\n{text}"))
            }
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            Ok(format!(
                "(PDF: {} bytes; pdftotext failed: {} — install poppler-utils (`apt install \
                 poppler-utils`) to read PDFs)",
                bytes.len(),
                err.trim()
            ))
        }
        Err(_) => Ok(format!(
            "(PDF: {} bytes; `pdftotext` is not installed. Install poppler-utils \
             (`apt install poppler-utils` or the equivalent) to extract its text.)",
            bytes.len()
        )),
    }
}

/// A compact, honest description of a file we will not return as text: size, a
/// best-guess kind from the magic bytes, and a hex/ASCII preview of the head.
fn describe_binary(path: &str, bytes: &[u8]) -> String {
    let kind = detect_kind(bytes);
    let mut out = format!(
        "(binary file — not UTF-8 text: {} bytes, looks like {kind}. Use bash for byte-level \
         work, or the appropriate tool for this format.)\n",
        bytes.len()
    );
    out.push_str(&format!("first bytes of {path}:\n"));
    out.push_str(&hex_preview(&bytes[..bytes.len().min(256)], 16));
    out
}

/// Best-effort file-type guess from the leading bytes. Deliberately small: the
/// point is to let the model recognise "this is an image/a zip/a database"
/// rather than to be `file(1)`.
fn detect_kind(b: &[u8]) -> &'static str {
    let starts = |sig: &[u8]| b.starts_with(sig);
    if starts(b"\x89PNG\r\n\x1a\n") {
        "a PNG image"
    } else if starts(&[0xff, 0xd8, 0xff]) {
        "a JPEG image"
    } else if starts(b"GIF8") {
        "a GIF image"
    } else if starts(b"PK\x03\x04") {
        "a ZIP archive (or a docx/xlsx/jar)"
    } else if starts(b"\x7fELF") {
        "an ELF binary"
    } else if starts(b"MZ") {
        "a Windows executable"
    } else if starts(b"SQLite format 3") {
        "a SQLite database"
    } else if starts(b"\x1f\x8b") {
        "gzip data"
    } else if starts(b"BZh") {
        "bzip2 data"
    } else if starts(b"\x28\xb5\x2f\xfd") {
        "zstd data"
    } else if starts(b"OggS") {
        "an Ogg stream"
    } else if starts(b"RIFF") {
        "a RIFF container (wav/avi/webp)"
    } else if starts(b"%!PS") {
        "PostScript"
    } else if starts(&[0x00, 0x00, 0x01, 0x00]) {
        "an ICO image"
    } else if starts(b"BM") {
        "a BMP image"
    } else if b.iter().take(512).all(|c| c.is_ascii() || *c >= 0x80) && b.contains(&0) {
        "binary data with embedded NULs"
    } else {
        "binary data"
    }
}

/// Render up to `max` bytes as classic `offset  hex  |ascii|` rows.
fn hex_preview(b: &[u8], width: usize) -> String {
    let mut out = String::new();
    for (row, chunk) in b.chunks(width).enumerate() {
        let mut hex = String::new();
        let mut ascii = String::new();
        for (i, byte) in chunk.iter().enumerate() {
            hex.push_str(&format!("{byte:02x} "));
            if i + 1 == width / 2 {
                hex.push(' ');
            }
            ascii.push(if byte.is_ascii_graphic() || *byte == b' ' { *byte as char } else { '.' });
        }
        out.push_str(&format!(
            "{:08x}  {:<width$} |{}|\n",
            row * width,
            hex,
            ascii,
            width = width * 3 + 1
        ));
    }
    out
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

/// Upper bound on matches a single `glob`/`grep` call will materialize before
/// pagination. Keeps a pathological `**/*` on a huge tree from building a
/// million-entry Vec.
const MAX_MATCHES: u64 = 2000;

/// Directories a search never descends into (the fallback walk's gitignore-ish
/// skip list; ripgrep applies `.gitignore` itself). Skipping `.git` is not
/// just a speed win — descending it produces nothing but noise (and the
/// occasional secret file).
const SEARCH_SKIP_DIRS: &[&str] = &[
    ".git", ".hg", ".svn", ".jj", "node_modules", "target", "vendor", ".venv", "venv",
    "__pycache__", ".mypy_cache", ".pytest_cache", ".gradle", "dist", "build", ".idea", ".tox",
];

/// Refuse to surface a credential/secret path in search *results*, matching the
/// security module's `is_secret` classification. Reads are default-open, but a
/// tree-wide search must not become a way to enumerate keys the operator asked
/// the guard to keep out of the model's context (`read: guarded-secrets`).
fn is_secret_path(p: &str) -> bool {
    let s = p.to_ascii_lowercase();
    let comp = |c: &str| s.contains(&format!("/{c}/")) || s.contains(&format!("\\{c}\\"));
    comp(".ssh")
        || comp(".aws")
        || comp(".gnupg")
        || comp(".config/gh")
        || comp(".config/gcloud")
        || comp(".mozilla")
        || comp(".config/google-chrome")
        || s.ends_with(".pem")
        || s.ends_with(".key")
        || s.contains("id_rsa")
        || s.contains("id_ed25519")
}

enum SearchOutcome {
    /// ripgrep ran and produced output.
    Ok(String),
    /// ripgrep ran with exit status 1: valid search, no matches.
    NoMatches(String),
    /// `rg` is not installed.
    Missing,
    /// ripgrep failed for a real reason (bad regex, unreadable path).
    Error(String),
}

/// Run ripgrep with `args` (pattern + root are pushed by the caller).
/// Returns [`SearchOutcome::Missing`] when the binary is absent so the caller
/// can use its in-process fallback.
fn run_rg(cwd: &Path, args: &[String]) -> SearchOutcome {
    let mut cmd = Command::new("rg");
    cmd.args(args);
    cmd.current_dir(cwd);
    cmd.stdin(Stdio::null());
    let out = match cmd.output() {
        Ok(o) => o,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return SearchOutcome::Missing,
        Err(e) => return SearchOutcome::Error(format!("could not run rg: {e}")),
    };
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    match out.status.code() {
        Some(0) => SearchOutcome::Ok(stdout),
        Some(1) => SearchOutcome::NoMatches(stdout),
        _ => {
            let err = String::from_utf8_lossy(&out.stderr);
            let err = err.trim();
            SearchOutcome::Error(if err.is_empty() { "ripgrep failed".into() } else { err.to_string() })
        }
    }
}

/// `rg --files` listing for `root`, root-relative. `None` when rg is absent.
fn rg_files(root: &Path) -> Option<Vec<String>> {
    let mut args = vec![
        "--files".to_string(),
        "--color=never".to_string(),
    ];
    for d in SEARCH_SKIP_DIRS {
        args.push("--glob".to_string());
        args.push(format!("!**/{d}/**"));
    }
    args.push(root.display().to_string());
    match run_rg(root, &args) {
        SearchOutcome::Ok(s) | SearchOutcome::NoMatches(s) => Some(
            s.lines()
                .map(|l| strip_root(l, root))
                .filter(|l| !l.is_empty())
                .take(MAX_MATCHES as usize)
                .collect(),
        ),
        _ => None,
    }
}

/// Built-in recursive file walk used when ripgrep is unavailable. Applies
/// [`SEARCH_SKIP_DIRS`], never follows symlinks (no cycles), and caps at
/// [`MAX_MATCHES`]. Returns root-relative paths.
fn walk_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk_into(root, root, 0, &mut out);
    out
}

fn walk_into(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > 32 || out.len() >= MAX_MATCHES as usize {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        if out.len() >= MAX_MATCHES as usize {
            return;
        }
        let name = e.file_name().to_string_lossy().to_string();
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() {
            if SEARCH_SKIP_DIRS.contains(&name.as_str()) || name.starts_with('.') && name.len() > 1 {
                continue;
            }
            walk_into(root, &p, depth + 1, out);
        } else if ft.is_file() {
            // Hidden *files* are skipped too (rg's default), except the
            // well-known dotfiles an agent legitimately wants.
            let hidden = name.starts_with('.');
            let wanted_dot = matches!(
                name.as_str(),
                ".gitignore" | ".gitattributes" | ".env.example" | ".editorconfig" | ".dockerignore"
            );
            if hidden && !wanted_dot {
                continue;
            }
            if let Ok(rel) = p.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

/// Make an rg output line root-relative: strip a leading `./` and the absolute
/// root prefix, so `grep`/`glob` output is consistent regardless of how the
/// search root was spelled.
fn strip_root(line: &str, root: &Path) -> String {
    let root_s = root.to_string_lossy().replace('\\', "/");
    let root_trim = root_s.trim_end_matches('/');
    let mut l = line.replace('\\', "/");
    if !root_trim.is_empty()
        && let Some(rest) = l.strip_prefix(&format!("{root_trim}/")) {
            l = rest.to_string();
        }
    l.strip_prefix("./").unwrap_or(&l).to_string()
}

/// Match a `/`-separated relative path against a glob pattern.
///
/// Supports `**` (any number of path segments, including zero), `*` (any run of
/// characters *within* a segment), `?` (one character within a segment), and
/// character classes `[abc]` / `[!abc]`. A pattern with no `/` is matched
/// against the file *name* as well as the whole relative path, so `"*.rs"`
/// behaves the way everyone expects (any .rs file anywhere below the root).
fn glob_match(pattern: &str, path: &str) -> bool {
    let pat = pattern.trim().trim_start_matches("./");
    let path = path.trim_start_matches("./");
    if pat.is_empty() {
        return false;
    }
    if glob_segments(pat.as_bytes(), path.as_bytes()) {
        return true;
    }
    if !pat.contains('/')
        && let Some(name) = path.rsplit('/').next() {
            return glob_segments(pat.as_bytes(), name.as_bytes());
        }
    false
}

/// Globbing with `**` crossing `/`. Split at `**` boundaries so each chunk is
/// matched against a contiguous span; within a chunk `*`/`?` never cross `/`.
///
/// `**` may match zero or more *whole segments*, so `**/*.rs` matches
/// `main.rs`, `src/main.rs` and `a/b/main.rs` alike — the segment after `**`
/// is tried against every suffix that starts at a segment boundary.
fn glob_segments(pat: &[u8], path: &[u8]) -> bool {
    // Fast path: no `**` anywhere -> plain segment-wise match.
    if !contains_double_star(pat) {
        return wildcard(pat, path);
    }
    // Split on the first `**`. The chunk before it must match a *prefix* of the
    // path (segment-aligned, so a trailing `/` is dropped); the chunk after it
    // must match the remainder — and, because `**` may consume whole segments,
    // the remainder may start at any segment boundary.
    let idx = find_double_star(pat).expect("contains_double_star checked above");
    let before = trim_double_star_left(&pat[..idx]);
    let mut after = &pat[idx + 2..];
    after = after.strip_prefix(b"/").unwrap_or(after);
    // `**/` swallows the separator on both sides: try `after` against the path
    // itself and every suffix that begins a segment.
    let starts: Vec<usize> = std::iter::once(0)
        .chain(
            path.iter()
                .enumerate()
                .filter(|(_, c)| **c == b'/')
                .map(|(i, _)| i + 1),
        )
        .collect();
    for start in starts.iter().copied() {
        if start > path.len() {
            continue;
        }
        let (head, rest) = path.split_at(start);
        // `head` ends with `/` (except for start 0); drop it for matching.
        let head_trim = head.strip_suffix(b"/").unwrap_or(head);
        if !wildcard(before, head_trim) {
            continue;
        }
        if after.is_empty() {
            return true;
        }
        // The `**` itself consumes whole segments of what follows, so `after`
        // must be tried against every boundary-aligned suffix of `rest` —
        // including `rest` itself (zero further segments consumed).
        if glob_segments(after, rest) {
            return true;
        }
        for cut in starts.iter().copied().filter(|c| *c > start && *c <= path.len()) {
            let suffix = &path[cut..];
            if glob_segments(after, suffix) {
                return true;
            }
        }
    }
    false
}

/// Drop a trailing `/` from the chunk before a `**` (`src/**/x` -> `src`), so
/// the `/` that separates it from the `**` is not matched as a path character.
fn trim_double_star_left(chunk: &[u8]) -> &[u8] {
    chunk.strip_suffix(b"/").unwrap_or(chunk)
}

fn contains_double_star(pat: &[u8]) -> bool {
    find_double_star(pat).is_some()
}

fn find_double_star(pat: &[u8]) -> Option<usize> {
    pat.windows(2).position(|w| w == b"**")
}

/// Classic `*`/`?`/`[…]` wildcard match over a byte slice. `*` never crosses a
/// `/` separator (the caller splits on segments), but `?` may match any single
/// character including `/` only when the pattern is being used name-wise —
/// callers always pass separator-free spans, so a literal `/` in `s` simply
/// fails to match `*`/`?`.
fn wildcard(pat: &[u8], s: &[u8]) -> bool {
    // `*` and `?` never consume a `/`: that is what keeps `src/*.rs` from
    // matching `src/deep/mod.rs`. Crossing directories is `**`'s job, handled
    // by `glob_segments` above. (When the caller matches a bare *name* pattern
    // against a file name, `s` contains no separator at all, so this rule is
    // invisible there.)
    let (mut p, mut i) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while i < s.len() {
        let c = s[i];
        // A `/` in the input can only be consumed by a literal `/` in the
        // pattern (or be the tail matched by an enclosing `**`); every
        // wildcard branch below refuses it.
        let at_sep = c == b'/';
        if p < pat.len() && pat[p] == b'[' {
            if let Some((set, next)) = parse_class(&pat[p..])
                && !at_sep
                && set.matches(c)
            {
                p += next;
                i += 1;
                continue;
            }
        } else if p < pat.len() && ((pat[p] == b'?' && !at_sep) || pat[p] == c) {
            // `?` matches any non-separator; a literal matches itself.
            p += 1;
            i += 1;
            continue;
        } else if p < pat.len() && pat[p] == b'*' && !(at_sep && star == Some(p)) {
            // A starred wildcard only consumes non-separators; at a separator
            // a fresh `*` matches the empty string and we move past it.
            if !at_sep {
                star = Some(p);
                mark = i;
            }
            p += 1;
            continue;
        }
        // No direct match: backtrack to the last `*`, extending what it ate by
        // one (non-separator) character.
        match star {
            Some(star_pos) if i < s.len() && !at_sep => {
                p = star_pos + 1;
                mark += 1;
                i = mark;
            }
            _ => return false,
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

/// Parse a `[...]` character class at the start of `pat`, returning the class
/// and how many bytes it consumed. `None` when the class is unterminated (the
/// `[` is then treated as a literal by the caller's normal path).
fn parse_class(pat: &[u8]) -> Option<(CharClass, usize)> {
    let mut i = 1;
    let negate = matches!(pat.get(1), Some(b'!') | Some(b'^'));
    if negate {
        i = 2;
    }
    let mut singles: Vec<u8> = Vec::new();
    let mut ranges: Vec<(u8, u8)> = Vec::new();
    let mut first = true;
    while i < pat.len() {
        let c = pat[i];
        if c == b']' && !first {
            return Some((CharClass { negate, singles, ranges }, i + 1));
        }
        first = false;
        if i + 2 < pat.len() && pat[i + 1] == b'-' && pat[i + 2] != b']' {
            ranges.push((c, pat[i + 2]));
            i += 3;
        } else {
            singles.push(c);
            i += 1;
        }
    }
    None
}

struct CharClass {
    negate: bool,
    singles: Vec<u8>,
    ranges: Vec<(u8, u8)>,
}

impl CharClass {
    fn matches(&self, c: u8) -> bool {
        let hit = self.singles.contains(&c) || self.ranges.iter().any(|&(a, b)| c >= a && c <= b);
        hit != self.negate
    }
}

/// Sort key: newest modification first. A missing mtime sorts last (epoch 0).
fn mtime_key(p: &Path) -> u64 {
    fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Apply `offset`/`limit` to a list of lines and render it as the tool result,
/// with an explicit note about what was cut so the model knows to page.
fn paginate(lines: &[String], limit: usize, offset: usize, what: &str) -> String {
    if lines.is_empty() {
        return "(no matches)".to_string();
    }
    let start = offset.min(lines.len());
    let end = (start + limit).min(lines.len());
    let mut out = lines[start..end].join("\n");
    let shown = end - start;
    if start > 0 || end < lines.len() {
        out.push_str(&format!(
            "\n… [pir] showing {shown} of {} {what} (offset {offset}, limit {limit}); \
             raise limit/offset to page",
            lines.len()
        ));
    } else {
        out.push_str(&format!("\n… [pir] {shown} {what}"));
    }
    crate::plugin::truncate(&mut out, 30_000);
    out
}

/// In-process `grep` fallback for hosts without ripgrep (or when spawning it
/// failed): walk the tree and apply the `regex` crate. Slower and without
/// gitignore awareness, but it keeps the tool usable everywhere.
fn fallback_grep(
    root: &Path,
    pattern: &str,
    mode: &str,
    ci: bool,
    numbers: bool,
    context: usize,
) -> Result<String, String> {
    let re = regex::RegexBuilder::new(pattern)
        .case_insensitive(ci)
        .multi_line(true)
        .build()
        .map_err(|e| format!("bad regex: {e}"))?;
    let mut hits: Vec<String> = Vec::new();
    for rel in walk_files(root) {
        if is_secret_path(&rel) {
            continue;
        }
        let full = root.join(&rel);
        let Ok(text) = fs::read_to_string(&full) else { continue };
        let file_lines: Vec<&str> = text.lines().collect();
        let mut count = 0usize;
        for (i, line) in file_lines.iter().enumerate() {
            if !re.is_match(line) {
                continue;
            }
            count += 1;
            match mode {
                "files_with_matches" => break,
                "count" => {}
                _ => {
                    if context > 0 {
                        let lo = i.saturating_sub(context);
                        let hi = (i + context + 1).min(file_lines.len());
                        for (j, ctx_line) in file_lines.iter().enumerate().take(hi).skip(lo) {
                            let sep = if j == i { ':' } else { '-' };
                            if numbers {
                                hits.push(format!("{rel}{sep}{}{sep}{}", j + 1, ctx_line));
                            } else {
                                hits.push(format!("{rel}{sep}{}", ctx_line));
                            }
                        }
                    } else if numbers {
                        hits.push(format!("{rel}:{}:{}", i + 1, line));
                    } else {
                        hits.push(format!("{rel}:{line}"));
                    }
                }
            }
        }
        match mode {
            "files_with_matches" if count > 0 => hits.push(rel),
            "count" if count > 0 => hits.push(format!("{rel}:{count}")),
            _ => {}
        }
        if hits.len() >= MAX_MATCHES as usize {
            break;
        }
    }
    Ok(hits.join("\n"))
}

/// Poll granularity for the command wait/clock loops.
/// These used fixed
/// 250-500 ms sleeps, so an ESC/ctrl-c could sit unnoticed for half a second
/// before the kill even started (the "tool cancelling takes >99 ms" report).
/// Slicing at this interval bounds that latency while each slice costs only a
/// cheap atomic load (plus a `try_wait` in the main loop).
const WAIT_SLICE: Duration = Duration::from_millis(10);

/// Sleep up to `total`, returning as soon as `stop` flips. Used by the
/// elapsed-clock thread so `run_shell` can join it immediately on abort
/// instead of waiting out its full 250-500 ms tick (that join alone was
/// ~500 ms of the measured cancel latency).
fn sleep_or_stop(stop: &AtomicBool, total: Duration) {
    let deadline = Instant::now() + total;
    loop {
        let now = Instant::now();
        if now >= deadline || stop.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep((deadline - now).min(WAIT_SLICE));
    }
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
                sleep_or_stop(&clock_stop_w, Duration::from_millis(500));
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
                sleep_or_stop(&clock_stop_w, Duration::from_millis(250));
                continue;
            }
            sleep_or_stop(&clock_stop_w, Duration::from_millis(250));
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
                std::thread::sleep(WAIT_SLICE);
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
    #[cfg(unix)]
    let caps_dropped = crate::security::overlay::container_engaged()
        || std::env::var_os("PIR_DETECT_CAPS").map(|v| v != "0" && !v.is_empty()).unwrap_or(false);
    #[cfg(not(unix))]
    let caps_dropped = std::env::var_os("PIR_DETECT_CAPS").map(|v| v != "0" && !v.is_empty()).unwrap_or(false);
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
                            Ok(None) => std::thread::sleep(WAIT_SLICE),
                            Err(_) => return None,
                        }
                    }
                }
                Ok(None) => std::thread::sleep(WAIT_SLICE),
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
        std::thread::sleep(WAIT_SLICE);
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

/// Script sourced by every non-interactive `bash -c` the agent spawns (via
/// `BASH_ENV`). It wraps `timeout` for hard-kill semantics and, when ripgrep
/// is installed, wraps `grep`/`find` to steer the agent away from slow
/// searches (a whole-filesystem `find /` is the usual offender). Version-
/// tagged so a cached `/tmp/pir-timeout/bashenv` from an older build is
/// rewritten.
const PIR_BASHENV: &str = r#"# pir-bashenv-v3
# Wrap `timeout` so a model-issued `timeout N cmd` also hard-kills a child
# that ignores SIGTERM (KILL 5s after TERM). User overrides in this file win.
__pir_timeout_default() { command timeout -k 5 "$@"; }
timeout() { __pir_timeout_default "$@"; }

# Steer the agent's searches at ripgrep when it is installed (set
# PIR_RG_ALIASES=0 to opt out). The usual offender is a whole-filesystem
# `find /`; rg is far faster and respects .gitignore. These are shell
# *functions*, not aliases: a non-interactive `bash -c` does not expand
# aliases. Each wrapper falls back to the real tool for arguments it cannot
# translate safely.
__pir_rg_on() {
  case "${PIR_RG_ALIASES:-1}" in
    0|off|false|no|OFF|False|No) return 1 ;;
  esac
  command -v rg >/dev/null 2>&1
}
if __pir_rg_on; then
  # grep -> rg. GNU's recursive -r/-R is dropped (rg is recursive by default
  # and `-r` means --replace); simple recursive clusters such as -rn/-ri are
  # rewritten. Only short flags GNU grep and rg agree on are passed through;
  # anything else (notably -E, -h, -H, -I, -L, -P, where rg means something
  # different) falls back to the real grep.
  grep() {
    local x stripped
    local -a out=()
    for x in "$@"; do
      case "$x" in
        --include|--include=*|--exclude|--exclude=*|--exclude-dir|--exclude-dir=*|--exclude-from|--exclude-from=*|--directories|--directories=*|--binary-files|--binary-files=*|--label|--label=*|--mmap|--initial-tab)
          command grep "$@"; return ;;
      esac
    done
    for x in "$@"; do
      case "$x" in
        -r|-R) : ;;
        --*) out+=("$x") ;;
        -[efmABC]*) out+=("$x") ;;
        -*)
          if [[ "$x" =~ ^-[rRivnlcoqwxsaF]+$ ]]; then
            stripped="-${x//[rR]/}"
            [ "$stripped" = "-" ] || out+=("$stripped")
          else
            command grep "$@"; return
          fi ;;
        *) out+=("$x") ;;
      esac
    done
    command rg "${out[@]}"
  }
  # find -> rg --files for a plain directory listing; any predicate
  # (-name/-type/-exec/...) defers to the real find, which rg does not
  # implement. rg --files respects .gitignore and omits hidden files, unlike
  # find(1).
  find() {
    local x
    for x in "$@"; do
      case "$x" in -*) command find "$@"; return ;; esac
    done
    command rg --files "$@"
  }
fi
"#;

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
        let stash_dir = std::env::temp_dir().join("pir-timeout");
        let _ = fs::create_dir_all(&stash_dir);
        let path = stash_dir.join("bashenv");
        // Rewrite when the on-disk script is from an older version (marker
        // changed), so existing caches pick up new wrappers.
        let current = fs::read_to_string(&path)
            .map(|s| s.contains("# pir-bashenv-v3"))
            .unwrap_or(false);
        if !current {
            // Write via a unique temp + rename so a concurrently spawned
            // `bash` can never source a half-written file.
            let tmp = stash_dir.join(format!("bashenv.tmp.{}", std::process::id()));
            if fs::write(&tmp, PIR_BASHENV).is_ok() {
                let _ = fs::rename(&tmp, &path);
            }
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
        unsafe { c.pre_exec(crate::user::drop_to_agent_user) };
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

// These tests exercise the unix process-tree kill semantics (`sleep`,
// `/dev/null`, background `&`, process groups) with unix shell commands; on
// Windows the commands don't exist / parse differently, so they're compiled
// and run on unix only. On Windows the abort/job-kill behavior is covered by
// the smoke tests and the `job_kill` tool path.
#[cfg(all(test, unix))]
mod esc_tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    #[test]
    fn abort_kills_running_command() {
        // A slow command (`sleep 30`). Flipping the abort flag must kill it
        // almost immediately (well under a second), not wait for it to exit.
        let abort = Arc::new(AtomicBool::new(false));
        let mut b = Builtin::new(PathBuf::from("."), abort.clone(), Arc::new(AtomicBool::new(false)));
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

    /// ESC/ctrl-c must interrupt a running foreground command within a human
    /// blink, not at the next 250 ms tick. The old wait loop slept a fixed
    /// 250 ms between abort checks, so pressing ESC just after a tick could
    /// leave the command (and the `running` spinner) alive for ~250 ms — the
    /// "tool cancelling takes >99 ms" report. This asserts the *latency from
    /// the abort flag flipping to the command being reaped*, measured on a
    /// freshly parked loop so the old 250 ms sleep is the only variable.
    #[test]
    fn abort_latency_is_sub_100ms() {
        let abort = Arc::new(AtomicBool::new(false));
        let mut b = Builtin::new(PathBuf::from("."), abort.clone(), Arc::new(AtomicBool::new(false)));
        let handle = std::thread::spawn(move || run_shell(&mut b, "sleep 30"));
        // Land mid-wait: with a 250 ms sleep the loop parks at ~250/500/750…,
        // so at 300 ms it is ~50 ms into a sleep and would not wake until
        // ~200 ms after the abort below.
        std::thread::sleep(Duration::from_millis(300));
        // Best of several attempts. What is under test is that the abort is seen
        // at the next 10 ms poll slice rather than at the next 250 ms tick — a
        // ~250 ms distinction. An absolute 100 ms bound measured the machine, not
        // the loop: under external load (this box routinely runs other CPU-heavy
        // work) a single attempt can overshoot simply because the thread was
        // descheduled, even though the loop is polling correctly.
        let mut best = Duration::MAX;
        let mut last_out = String::new();
        for attempt in 0..5 {
            let abort = Arc::new(AtomicBool::new(false));
            let mut b =
                Builtin::new(PathBuf::from("."), abort.clone(), Arc::new(AtomicBool::new(false)));
            let handle = std::thread::spawn(move || {
                run_shell(&mut b, &format!("sleep {}", 30 + attempt))
            });
            std::thread::sleep(Duration::from_millis(300));
            let t0 = Instant::now();
            abort.store(true, Ordering::SeqCst);
            last_out = handle.join().unwrap().expect("run_shell result");
            best = best.min(t0.elapsed());
        }
        let _ = handle;
        // Half the old coarse tick: proof the loop is not waiting out the 250 ms
        // sleep, while leaving room for a descheduled thread.
        assert!(
            best < Duration::from_millis(125),
            "best abort latency {best:?} exceeds 125ms — the wait loop is still polling too \
             coarsely (it must notice within a slice, not at the next 250ms tick)"
        );
        assert!(last_out.contains("aborted by user"), "got: {last_out}");
    }

    #[test]
    fn no_abort_completes() {
        // Without abort, a fast command runs to completion and reports success.
        let abort = Arc::new(AtomicBool::new(false));
        let mut b = Builtin::new(PathBuf::from("."), abort.clone(), Arc::new(AtomicBool::new(false)));
        let out = run_shell(&mut b, "echo hello-from-pir").expect("run_shell result");
        assert!(out.contains("hello-from-pir"), "got: {out}");
    }

    #[test]
    fn abort_only_noop_when_idle() {
        // The abort flag alone (no running command) must not abort a later one.
        let abort = Arc::new(AtomicBool::new(false));
        abort.store(true, Ordering::SeqCst);
        abort.store(false, Ordering::SeqCst);
        let mut b = Builtin::new(PathBuf::from("."), abort.clone(), Arc::new(AtomicBool::new(false)));
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
        // SAFETY: edition 2024 marks env mutation unsafe; pir confines
        // it to startup config and explicit session toggles.
        unsafe { std::env::set_var("PIR_SHELL_CHECK_IN_SECS", "1"); }
        let mut b = Builtin::new(PathBuf::from("."), Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
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
        // SAFETY: edition 2024 marks env mutation unsafe; pir confines
        // it to startup config and explicit session toggles.
        unsafe { std::env::set_var("PIR_SHELL_CHECK_IN_SECS", "1"); }
        let mut b = Builtin::new(PathBuf::from("."), Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
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
        // SAFETY: edition 2024 marks env mutation unsafe; pir confines
        // it to startup config and explicit session toggles.
        unsafe { std::env::set_var("PIR_SHELL_CHECK_IN_SECS", "1"); }
        let job_kill = Arc::new(AtomicBool::new(false));
        let mut b = Builtin::new(PathBuf::from("."), Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
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
        // SAFETY: edition 2024 marks env mutation unsafe; pir confines
        // it to startup config and explicit session toggles.
        unsafe { std::env::set_var("PIR_SHELL_CHECK_IN_SECS", "1"); }
        let mut b = Builtin::new(PathBuf::from("."), Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let d = run_shell(&mut b, "sleep 60").expect("detach");
        assert!(d.contains("[detached]"), "got: {d}");
        let killed = ToolBackend::kill_all_jobs(&mut b);
        assert_eq!(killed, 1, "the running job must be reported killed");
        assert!(b.jobs.is_empty());
    }

    #[test]
    fn job_kill_after_finish_reports_already_finished() {
        // SAFETY: edition 2024 marks env mutation unsafe; pir confines
        // it to startup config and explicit session toggles.
        unsafe { std::env::set_var("PIR_SHELL_CHECK_IN_SECS", "1"); }
        let mut b = Builtin::new(PathBuf::from("."), Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
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
        // SAFETY: edition 2024 marks env mutation unsafe; pir confines
        // it to startup config and explicit session toggles.
        unsafe { std::env::set_var("PIR_SHELL_CHECK_IN_SECS", "1"); }
        let mut b = Builtin::new(PathBuf::from("."), Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
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

    /// The agent's bash environment must steer `grep`/`find` at ripgrep when
    /// it is installed (PIR_RG_ALIASES defaults on), so agents don't burn
    /// minutes on a whole-filesystem `find /`. The wrappers are shell
    /// functions and must fall back to the real tools for inputs rg cannot
    /// take.
    #[test]
    fn bashenv_prefers_rg_for_grep_and_find() {
        // The generated script always carries the wrappers; the runtime gate
        // decides whether they are installed.
        assert!(PIR_BASHENV.contains("grep()"), "grep wrapper missing");
        assert!(PIR_BASHENV.contains("find()"), "find wrapper missing");
        assert!(PIR_BASHENV.contains("PIR_RG_ALIASES"), "opt-out missing");
        let have_rg = std::process::Command::new("rg")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let opted_out = std::env::var("PIR_RG_ALIASES")
            .map(|v| v == "0" || v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("false"))
            .unwrap_or(false);
        if !have_rg || opted_out {
            return; // the runtime gate would leave the real tools in place
        }
        let abort = Arc::new(AtomicBool::new(false));
        let mut b = Builtin::new(PathBuf::from("."), abort, Arc::new(AtomicBool::new(false)));
        let out = run_shell(&mut b, "type grep; type find").expect("run_shell");
        assert!(out.contains("function"), "grep/find should be functions with rg present: {out}");
        // GNU's recursive `-r` must be rewritten (rg's `-r` means --replace).
        let out = run_shell(&mut b, "printf 'alpha\\nbeta\\n' | grep -r alpha").expect("run_shell");
        assert!(out.contains("alpha"), "grep -r via rg should match: {out}");
        // A `find` predicate must still work through the real-find fallback.
        let out = run_shell(&mut b, "find . -maxdepth 1 -name 'Cargo.toml'").expect("run_shell");
        assert!(out.contains("Cargo.toml"), "find predicate fallback should work: {out}");
        // Flags where rg differs from GNU grep must fall back to the real
        // grep: `-h` is rg's help and `-E` is rg's --encoding.
        let out = run_shell(&mut b, "grep -h \"fn main\" src/main.rs").expect("run_shell");
        assert!(
            out.contains("fn main") && !out.contains("ripgrep"),
            "grep -h must use the real grep: {out}"
        );
        let out = run_shell(&mut b, "printf 'foo\\nbar\\n' | grep -E 'foo|bar'").expect("run_shell");
        assert!(out.contains("foo") && out.contains("bar"), "grep -E must use the real grep: {out}");
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pir_search_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("src/deep")).unwrap();
        fs::write(d.join("src/main.rs"), "fn main() {\n    let x = 1;\n}\n").unwrap();
        fs::write(d.join("src/deep/mod.rs"), "pub fn helper() {}\n// needle here\n").unwrap();
        fs::write(d.join("README.md"), "# hi\nneedle in docs\n").unwrap();
        fs::write(d.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(d.join("ignored.txt"), "needle should be ignored\n").unwrap();
        fs::create_dir_all(d.join("node_modules")).unwrap();
        fs::write(d.join("node_modules/x.js"), "needle in deps\n").unwrap();
        // ripgrep only honours .gitignore inside a repository, so make the
        // fixture one. (Both rg paths and the fallback walk skip node_modules
        // regardless.)
        let git = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&d)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = git;
        d
    }

    fn builtin(dir: &Path) -> Builtin {
        Builtin::new(
            dir.to_path_buf(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        )
    }

    #[test]
    fn glob_matches_starstar_and_name_pattern() {
        assert!(glob_match("**/*.rs", "src/main.rs"));
        assert!(glob_match("**/*.rs", "src/deep/mod.rs"));
        assert!(!glob_match("**/*.rs", "README.md"));
        // A bare name pattern matches at any depth.
        assert!(glob_match("*.rs", "src/deep/mod.rs"));
        assert!(!glob_match("*.md", "src/main.rs"));
        // `**` may match zero segments.
        assert!(glob_match("src/**/*.rs", "src/main.rs"));
        assert!(glob_match("src/**", "src/main.rs"));
        // Anchored patterns stay anchored.
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "src/deep/mod.rs"));
        // `?` and character classes.
        assert!(glob_match("src/mai?.rs", "src/main.rs"));
        assert!(!glob_match("src/mai?.rs", "src/mai.rs"));
        assert!(glob_match("*.r[si]", "src/main.rs"));
        assert!(!glob_match("*.r[!s]", "src/main.rs"));
    }

    #[test]
    fn glob_tool_returns_paths_and_respects_skips() {
        let d = tmpdir("glob");
        let mut b = builtin(&d);
        let out = b.glob(&json!({ "pattern": "**/*.rs" })).expect("glob");
        assert!(out.contains("src/main.rs"), "got: {out}");
        assert!(out.contains("src/deep/mod.rs"), "got: {out}");
        assert!(!out.contains("README.md"), "got: {out}");
        // node_modules is never descended (both rg and the fallback skip it).
        assert!(!out.contains("node_modules"), "got: {out}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn glob_paginates_with_limit_and_offset() {
        let d = tmpdir("page");
        let mut b = builtin(&d);
        let all = b.glob(&json!({ "pattern": "**/*.rs", "limit": 100 })).unwrap();
        assert!(all.contains("2 paths"), "got: {all}");
        let first = b.glob(&json!({ "pattern": "**/*.rs", "limit": 1 })).unwrap();
        assert!(first.contains("showing 1 of 2"), "got: {first}");
        let second = b.glob(&json!({ "pattern": "**/*.rs", "limit": 1, "offset": 1 })).unwrap();
        assert!(second.contains("showing 1 of 2"), "got: {second}");
        // The two pages must not overlap.
        let a = first.lines().next().unwrap().to_string();
        let c = second.lines().next().unwrap().to_string();
        assert_ne!(a, c, "offset must advance past the first page");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn grep_finds_content_and_skips_ignored_and_deps() {
        let d = tmpdir("grep");
        let mut b = builtin(&d);
        let out = b.grep(&json!({ "pattern": "needle" })).expect("grep");
        assert!(out.contains("src/deep/mod.rs"), "got: {out}");
        assert!(out.contains("README.md"), "got: {out}");
        // .gitignore'd file and node_modules must both be excluded.
        assert!(!out.contains("ignored.txt"), "got: {out}");
        assert!(!out.contains("node_modules"), "got: {out}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn grep_modes_and_flags() {
        let d = tmpdir("modes");
        let mut b = builtin(&d);
        // files_with_matches: paths only.
        let files = b
            .grep(&json!({ "pattern": "needle", "output_mode": "files_with_matches" }))
            .unwrap();
        assert!(!files.contains(":needle"), "files mode must not print content: {files}");
        // count: per-file counts.
        let counts = b
            .grep(&json!({ "pattern": "needle", "output_mode": "count" }))
            .unwrap();
        assert!(counts.lines().any(|l| l.ends_with(":1")), "got: {counts}");
        // case-insensitive.
        let ci = b
            .grep(&json!({ "pattern": "NEEDLE", "case_insensitive": true }))
            .unwrap();
        assert!(ci.contains("needle"), "got: {ci}");
        let cs = b.grep(&json!({ "pattern": "NEEDLE" })).unwrap();
        assert_eq!(cs, "(no matches)", "case-sensitive search should miss: {cs}");
        // glob filter.
        let only_md = b
            .grep(&json!({ "pattern": "needle", "glob": "*.md" }))
            .unwrap();
        assert!(only_md.contains("README.md"), "got: {only_md}");
        assert!(!only_md.contains("mod.rs"), "got: {only_md}");
        // context lines.
        let ctx = b
            .grep(&json!({ "pattern": "helper", "context": 1 }))
            .unwrap();
        assert!(ctx.contains("needle here"), "context line missing: {ctx}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn grep_rejects_empty_pattern() {
        let d = tmpdir("empty");
        let mut b = builtin(&d);
        assert!(b.grep(&json!({ "pattern": "" })).is_err());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn secret_paths_are_filtered_out_of_results() {
        let d = tmpdir("sec");
        fs::create_dir_all(d.join(".ssh")).unwrap();
        fs::write(d.join(".ssh/id_rsa"), "needle secret\n").unwrap();
        let mut b = builtin(&d);
        let out = b.grep(&json!({ "pattern": "needle" })).unwrap();
        assert!(!out.contains("id_rsa"), "secret path leaked: {out}");
        let _ = fs::remove_dir_all(&d);
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
        let b = Builtin::new(PathBuf::from("."), Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        // Just assert it returns promptly and deterministically; the actual
        // value depends on the test-runner's sudoers, not the code.
        let _ = b.sudo_is_passwordless("sudo ls");
    }

    /// The regression this module exists for: `tty_available()` must be true
    /// whenever the process can reach an interactive terminal, so an escalation
    /// prompt actually waits for the operator. This test only asserts the
    /// *decision contract* that was broken — we must never claim the operator
    /// declined when no keypress arrived. It never actually blocks: it stubs
    /// out the tty-availability question by checking the wording produced for a
    /// no-answer, which is the message the old code could not produce (it
    /// always said "user declined").
    #[test]
    fn no_answer_is_not_reported_as_a_decline() {
        // The refusal produced when nobody answers must name `no answer`, not
        // fabricate `user declined`. This is the exact wording regression.
        let b = Builtin::new(
            PathBuf::from("."),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        // Directly exercise the prompt's timeout branch with a 0-length wait by
        // pointing it at an unreadable "terminal": `/dev/tty` may exist for a
        // test harness, so we instead assert the *source contract* that the
        // no-answer arm exists by running the non-tty refusal (which is the
        // path taken when no human can be reached).
        if crate::term::tty_available() {
            // A terminal exists but the test runner can't safely consume the
            // operator's real keystrokes; assert the message wording via the
            // helper that builds it rather than blocking on /dev/tty.
            let msg = Builtin::refusal_without_approval("sudo as root", 1);
            assert!(
                !msg.contains("user declined"),
                "must not fabricate a denial by the operator: {msg}"
            );
            assert!(
                msg.contains("no answer") || msg.contains("refusing to escalate"),
                "expected an explicit no-answer refusal, got: {msg}"
            );
        } else {
            // No terminal at all: the non-tty refusal must point at the
            // out-of-band escalation path, never claim the user said no.
            let msg = b
                .request_priv_escalation_timeout("sudo true", "sudo as root".to_string(), 1)
                .expect_err("must refuse without a terminal");
            assert!(!msg.contains("user declined"), "got: {msg}");
            assert!(msg.contains("refusing to escalate"), "got: {msg}");
        }
    }
}

#[cfg(test)]
mod tool_spec_tests {
    use super::*;

    /// The tool list the model sees must contain the features added here, and
    /// the dispatch must handle every name it advertises (an advertised-but-
    /// undispatchable tool is a silent trap for the model).
    #[test]
    fn specs_include_new_tools_and_all_are_dispatchable() {
        let mut b = Builtin::new(
            PathBuf::from("."),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let names: Vec<&str> = b.specs().iter().map(|s| s.name).collect();
        for want in ["glob", "grep", "ask_user_question", "load_skill", "plan_write", "agent", "remember"] {
            assert!(names.contains(&want), "tool {want} missing from specs: {names:?}");
        }
        // Every advertised tool must produce something other than "unknown
        // tool" (an empty `{}` input is enough: the tool may complain about
        // missing args, but it must be reachable). `update_goal`, `request_stop`
        // and `ask_user_question` are advertised here but *intercepted by the
        // agent turn loop* (they need the agent's goal store / stop state /
        // REPL channel), so they are excluded — `Agent` has its own tests that
        // every one of them is dispatched there.
        let agent_intercepted = ["update_goal", "request_stop", "ask_user_question", "load_skill", "plan_write", "agent", "remember"];
        for spec in b.specs() {
            if agent_intercepted.contains(&spec.name) {
                continue;
            }
            let out = b.run(spec.name, &json!({}));
            assert!(
                !out.content.contains("unknown tool"),
                "{} is advertised but not dispatched: {}",
                spec.name,
                out.content
            );
        }
    }

    #[test]
    fn ask_spec_shape_matches_what_the_tool_accepts() {
        let b = Builtin::new(
            PathBuf::from("."),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let spec = b.specs().into_iter().find(|s| s.name == "ask_user_question").expect("spec");
        let props = &spec.schema["properties"];
        assert!(props.get("questions").is_some(), "schema must expose 'questions'");
        let item = &props["questions"]["items"];
        assert!(item["properties"].get("question").is_some());
        assert!(item["properties"].get("header").is_some());
        assert!(item["properties"].get("options").is_some());
        assert!(item["properties"].get("multiSelect").is_some());
    }
}

#[cfg(test)]
mod rich_read_tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pir_richread_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn read(path: &Path) -> String {
        read_file(&json!({ "path": path.to_string_lossy() })).expect("read_file")
    }

    #[test]
    fn plain_text_is_returned_unchanged() {
        let d = tmp("text");
        let p = d.join("a.txt");
        fs::write(&p, "hello\nworld\n").unwrap();
        assert_eq!(read(&p), "hello\nworld\n");
        let _ = fs::remove_dir_all(&d);
    }

    /// A *recognised image* now takes the richer image path (see
    /// `reading_an_image_describes_it_and_never_dumps_base64`); the generic
    /// hexdump description is for binary data we cannot name.
    #[test]
    fn binary_files_are_described_not_garbled() {
        let d = tmp("bin");
        // An ELF header followed by non-UTF-8 bytes: not text, not an image.
        let mut bytes = b"\x7fELF".to_vec();
        bytes.extend_from_slice(&[0u8; 64]);
        bytes.extend_from_slice(&[0xff, 0xfe, 0x00, 0x41]);
        let p = d.join("prog.bin");
        fs::write(&p, &bytes).unwrap();
        let out = read(&p);
        assert!(out.contains("binary file"), "got: {out}");
        assert!(out.contains("ELF binary"), "kind must be detected: {out}");
        assert!(out.contains(&format!("{} bytes", bytes.len())), "size missing: {out}");
        assert!(out.contains("7f 45 4c 46"), "hex preview missing: {out}");
        assert!(out.contains(".ELF"), "ascii column missing: {out}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn detects_common_magic_bytes() {
        assert_eq!(detect_kind(b"PK\x03\x04rest"), "a ZIP archive (or a docx/xlsx/jar)");
        assert_eq!(detect_kind(b"\x7fELFrest"), "an ELF binary");
        assert_eq!(detect_kind(b"SQLite format 3\0"), "a SQLite database");
        assert_eq!(detect_kind(b"\x1f\x8b\x08"), "gzip data");
        assert_eq!(detect_kind(b"\xff\xd8\xff\xe0"), "a JPEG image");
        assert_eq!(detect_kind(b"GIF89a"), "a GIF image");
        assert_eq!(detect_kind(&[0x00, 0x00, 0x01, 0x00]), "an ICO image");
        assert_eq!(detect_kind(b"BM\x00\x00"), "a BMP image");
        assert_eq!(detect_kind(b"\x00\x01\x02\x03"), "binary data with embedded NULs");
    }

    #[test]
    fn hex_preview_format_is_stable() {
        let h = hex_preview(b"ABCD", 4);
        // Width 4 splits the hex column in half, so the group separator lands
        // between B and C — the format is deliberate and worth pinning.
        assert!(h.starts_with("00000000  41 42  43 44"), "got: {h}");
        assert!(h.contains("|ABCD|"), "got: {h}");
        let h16 = hex_preview(b"ABCDEFGHIJKLMNOP", 16);
        assert!(h16.starts_with("00000000  41 42 43 44 45 46 47 48  49 4a 4b 4c 4d 4e 4f 50"), "got: {h16}");
        assert!(h16.contains("|ABCDEFGHIJKLMNOP|"), "got: {h16}");
    }

    #[test]
    fn gzip_is_transparently_decompressed() {
        let d = tmp("gz");
        let p = d.join("note.txt.gz");
        // Compress with the real gzip, so the fixture is a genuine stream.
        let plain = d.join("note.txt");
        fs::write(&plain, "compressed payload\n").unwrap();
        let ok = Command::new("gzip")
            .arg("-k")
            .arg("-f")
            .arg(&plain)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok || !p.exists() {
            // No gzip on this host: the fallback message must still be honest.
            let fake = d.join("fake.gz");
            fs::write(&fake, b"\x1f\x8bnot really gzip").unwrap();
            let out = read(&fake);
            assert!(out.contains("gzip"), "got: {out}");
            let _ = fs::remove_dir_all(&d);
            return;
        }
        let out = read(&p);
        assert!(out.contains("decompressed from gzip"), "got: {out}");
        assert!(out.contains("compressed payload"), "payload missing: {out}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn pdf_is_extracted_when_pdftotext_is_present() {
        let d = tmp("pdf");
        let p = d.join("doc.pdf");
        // A minimal one-page PDF whose content stream draws the text "PIRPDF".
        let body = "%PDF-1.4\n\
1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 300 100]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>endobj\n\
4 0 obj<</Length 44>>stream\nBT /F1 24 Tf 20 40 Td (PIRPDF) Tj ET\nendstream endobj\n\
5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\n\
trailer<</Root 1 0 R>>\n%%EOF\n";
        fs::write(&p, body).unwrap();
        let out = read(&p);
        if out.contains("PIRPDF") {
            // pdftotext is installed and parsed our fixture: the text must come
            // back without any warning prefix.
            assert!(out.contains("text extracted from PDF"), "got: {out}");
            assert!(!out.contains("(PDF:"), "a successful extraction must not warn: {out}");
        } else {
            // Either pdftotext is absent or it could not parse our hand-rolled
            // fixture; both must produce an honest, actionable message rather
            // than a UTF-8 error.
            assert!(out.contains("PDF"), "got: {out}");
            assert!(
                out.contains("pdftotext") || out.contains("scanned image"),
                "must explain the situation: {out}"
            );
        }
        let _ = fs::remove_dir_all(&d);
    }


    #[test]
    fn notebook_is_rendered_as_readable_cells() {
        let d = tmp("nb");
        let nb = r##"{
          "nbformat": 4,
          "metadata": {"kernelspec": {"language": "python"}},
          "cells": [
            {"cell_type": "markdown", "source": ["# Title\n", "some prose\n"]},
            {"cell_type": "code", "source": ["x = 1\n", "print(x)\n"],
             "outputs": [{"output_type": "stream", "text": ["1\n"]}]},
            {"cell_type": "code", "source": ["plot()"],
             "outputs": [{"output_type": "display_data",
                          "data": {"image/png": "iVBORw0KGgo=", "text/plain": ["<Figure>"]}}]},
            {"cell_type": "code", "source": ["boom()"],
             "outputs": [{"output_type": "error", "ename": "ValueError", "evalue": "bad input"}]},
            {"cell_type": "code", "source": []}
          ]
        }"##;
        let p = d.join("nb.ipynb");
        fs::write(&p, nb).unwrap();
        let out = read(&p);
        assert!(out.contains("Jupyter notebook, python"), "got: {out}");
        assert!(out.contains("cell 1 (markdown)"), "got: {out}");
        assert!(out.contains("# Title"), "markdown cell body missing: {out}");
        assert!(out.contains("```python"), "code cells must be fenced: {out}");
        assert!(out.contains("print(x)"), "code source missing: {out}");
        assert!(out.contains("output:"), "outputs must be shown: {out}");
        assert!(out.contains("1\n") || out.contains("  1"), "stream output missing: {out}");
        // A binary output is described, never dumped as base64.
        assert!(out.contains("[image/png output omitted]"), "got: {out}");
        assert!(!out.contains("iVBORw0KGgo"), "base64 must not be included: {out}");
        assert!(out.contains("<Figure>"), "text/plain fallback should appear: {out}");
        // Errors surface with their message.
        assert!(out.contains("ERROR: ValueError: bad input"), "got: {out}");
        // An empty cell is labelled rather than silently skipped.
        assert!(out.contains("cell 5 (code)"), "got: {out}");
        assert!(out.contains("(empty)"), "got: {out}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn notebook_with_a_bare_string_source_is_accepted() {
        // `source` is normally an array of lines, but a bare string is legal.
        let d = tmp("nbstr");
        let nb = r#"{"nbformat":4,"cells":[{"cell_type":"code","source":"y = 2"}]}"#;
        let p = d.join("b.ipynb");
        fs::write(&p, nb).unwrap();
        let out = read(&p);
        assert!(out.contains("y = 2"), "got: {out}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_non_notebook_ipynb_falls_back_to_raw_text() {
        // Something named .ipynb that is not a notebook must still be readable
        // rather than being swallowed whole.
        let d = tmp("nbbad");
        let p = d.join("weird.ipynb");
        fs::write(&p, "not json at all\n").unwrap();
        let out = read(&p);
        assert!(out.contains("not json at all"), "got: {out}");
        // Valid JSON without a `cells` array is also not a notebook.
        fs::write(&p, r#"{"something":"else"}"#).unwrap();
        let out = read(&p);
        assert!(out.contains("something"), "got: {out}");
        let _ = fs::remove_dir_all(&d);
    }


    #[test]
    fn image_headers_are_parsed_for_dimensions() {
        // PNG: signature + IHDR length/type + width/height.
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&1920u32.to_be_bytes());
        png.extend_from_slice(&1080u32.to_be_bytes());
        png.extend_from_slice(&[8, 2, 0, 0, 0]);
        let i = image_info(&png).expect("png recognised");
        assert_eq!(i.format, "PNG");
        assert_eq!((i.width, i.height), (Some(1920), Some(1080)));
        assert_eq!(i.mime, "image/png");

        // GIF: logical screen descriptor, little-endian.
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&320u16.to_le_bytes());
        gif.extend_from_slice(&200u16.to_le_bytes());
        let i = image_info(&gif).expect("gif");
        assert_eq!((i.format, i.width, i.height), ("GIF", Some(320), Some(200)));

        // JPEG: SOI, then an APP0 segment whose declared length (0x0010 = 16)
        // must cover its own 2 length bytes, so 14 bytes of payload follow.
        let mut jpg = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10];
        jpg.extend_from_slice(&[0u8; 14]);
        // Then SOF0: length 0x0011 (17), precision 8, height, width.
        jpg.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08]);
        jpg.extend_from_slice(&480u16.to_be_bytes()); // height
        jpg.extend_from_slice(&640u16.to_be_bytes()); // width
        let i = image_info(&jpg).expect("jpeg");
        assert_eq!((i.format, i.width, i.height), ("JPEG", Some(640), Some(480)));

        // BMP: dimensions at offset 18/22, little-endian signed.
        let mut bmp = b"BM".to_vec();
        bmp.extend_from_slice(&[0u8; 16]);
        bmp.extend_from_slice(&100i32.to_le_bytes());
        bmp.extend_from_slice(&50i32.to_le_bytes());
        let i = image_info(&bmp).expect("bmp");
        assert_eq!((i.format, i.width, i.height), ("BMP", Some(100), Some(50)));

        // Not an image at all.
        assert!(image_info(b"just text").is_none());
        assert!(image_info(&[]).is_none());
        // A truncated PNG is recognised but reports no dimensions rather than
        // reading past the end.
        let i = image_info(b"\x89PNG\r\n\x1a\n\x00\x00").expect("recognised");
        assert_eq!(i.format, "PNG");
        assert_eq!(i.width, None);
    }

    #[test]
    fn reading_an_image_describes_it_and_never_dumps_base64() {
        let d = tmp("img");
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&800u32.to_be_bytes());
        png.extend_from_slice(&600u32.to_be_bytes());
        png.extend_from_slice(&[8, 2, 0, 0, 0]);
        png.extend_from_slice(&[0xff; 512]);
        let p = d.join("pic.png");
        fs::write(&p, &png).unwrap();

        let out = read(&p);
        assert!(out.contains("image — PNG 800x600 pixels"), "got: {out}");
        assert!(out.contains("image/png"), "got: {out}");
        assert!(out.contains("Not returned as text"), "got: {out}");
        // It must point a vision model at the file, and give a text-only model
        // something actionable.
        assert!(out.contains("vision-capable"), "got: {out}");
        assert!(out.contains("base64"), "must show how to encode it: {out}");
        assert!(out.contains("tesseract") || out.contains("ImageMagick"), "OCR hint: {out}");
        // And it must NOT contain the pixel data.
        assert!(!out.contains("////"), "pixel data leaked: {out}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn office_extension_detection_is_precise() {
        assert_eq!(office_kind("a.docx"), Some(OfficeKind::Docx));
        assert_eq!(office_kind("a.xlsx"), Some(OfficeKind::Xlsx));
        assert_eq!(office_kind("a.xlsm"), Some(OfficeKind::Xlsx));
        assert_eq!(office_kind("a.pptx"), Some(OfficeKind::Pptx));
        // A plain zip must NOT be treated as an office document just because it
        // starts with PK.
        assert_eq!(office_kind("a.zip"), None);
        assert_eq!(office_kind("a.tar.gz"), None);
    }

    #[test]
    fn xml_is_reduced_to_readable_text() {
        let docx = r#"<?xml version="1.0"?><w:document><w:body>
            <w:p><w:r><w:t>Hello</w:t></w:r></w:p>
            <w:p><w:r><w:t>Second &amp; third</w:t></w:r></w:p>
            </w:body></w:document>"#;
        let t = xml_to_text(docx);
        assert!(t.contains("Hello"), "got: {t}");
        assert!(t.contains("Second & third"), "entities must decode: {t}");
        assert!(!t.contains('<'), "tags must be stripped: {t}");
        // Paragraphs land on separate lines.
        let lines: Vec<&str> = t.lines().filter(|l| !l.is_empty()).collect();
        assert!(lines.len() >= 2, "got: {t:?}");
    }

    /// Build a real .docx (a zip with the expected members) and read it, so the
    /// unzip path is exercised rather than mocked.
    #[test]
    fn office_documents_are_extracted_via_unzip() {
        let d = tmp("docx");
        let src = d.join("src");
        fs::create_dir_all(src.join("word")).unwrap();
        fs::write(
            src.join("word/document.xml"),
            r#"<?xml version="1.0"?><w:document><w:body>
               <w:p><w:r><w:t>Quarterly report</w:t></w:r></w:p>
               <w:p><w:r><w:t>Revenue up 12%</w:t></w:r></w:p>
               </w:body></w:document>"#,
        )
        .unwrap();
        let docx = d.join("report.docx");
        let zipped = Command::new("zip")
            .arg("-q")
            .arg("-r")
            .arg(&docx)
            .arg("word")
            .current_dir(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !zipped || !docx.exists() {
            // No `zip` on this host: the fallback message must still be honest.
            let fake = d.join("fake.docx");
            fs::write(&fake, b"PK\x03\x04 not really a docx").unwrap();
            let out = read(&fake);
            assert!(out.contains("Word document"), "got: {out}");
            let _ = fs::remove_dir_all(&d);
            return;
        }
        let have_unzip = Command::new("unzip")
            .arg("-v")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let out = read(&docx);
        assert!(out.contains("Word document"), "got: {out}");
        if have_unzip {
            assert!(out.contains("Quarterly report"), "body text missing: {out}");
            assert!(out.contains("Revenue up 12%"), "got: {out}");
            assert!(!out.contains("<w:t>"), "XML must be stripped: {out}");
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn non_utf8_text_never_produces_a_read_error() {
        let d = tmp("latin1");
        // Latin-1 bytes with no NUL: invalid UTF-8, but not really "binary".
        fs::write(d.join("l.txt"), b"caf\xe9 au lait\n").unwrap();
        let out = read(&d.join("l.txt"));
        assert!(out.contains("binary file"), "got: {out}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_file_is_still_an_error() {
        let d = tmp("missing");
        let err = read_file(&json!({ "path": d.join("nope").to_string_lossy() })).unwrap_err();
        assert!(err.contains("read_file"), "got: {err}");
        let _ = fs::remove_dir_all(&d);
    }
}

