//! Core plugin ABI (extension type "a": statically compiled Rust extensions).
//!
//! Every tool — built-in or dropped-in — is exposed to the model through one
//! uniform contract: a [`ToolBackend`] that lists [`ToolSpec`]s and runs them,
//! returning an [`Outcome`]. Extensions are linked at compile time by
//! `build.rs`, which scans `extensions/*/src/lib.rs` and emits a
//! `register_all` that pushes each backend into a [`Registry`].
//!
//! # Writing an extension
//!
//! Drop a folder into `extensions/<name>/` containing `src/lib.rs`:
//!
//! ```rust,ignore
//! use crate::plugin::{Outcome, Registry, ToolBackend, ToolSpec};
//! use serde_json::json;
//!
//! pub fn register(reg: &mut Registry) {
//!     reg.add(Box::new(MyExt));
//! }
//!
//! struct MyExt;
//! impl ToolBackend for MyExt {
//!     fn name(&self) -> &'static str { "my-ext" }
//!     fn specs(&self) -> Vec<ToolSpec> {
//!         vec![ToolSpec {
//!             name: "hello",
//!             description: "say hello",
//!             schema: json!({ "type": "object", "properties": {}, "required": [] }),
//!         }]
//!     }
//!     fn run(&mut self, name: &str, _input: &serde_json::Value) -> Outcome {
//!         match name {
//!             "hello" => Outcome::ok("hello".into()),
//!             other => Outcome::err(format!("unknown tool '{other}'")),
//!         }
//!     }
//! }
//! ```
//!
//! That's it — rebuild and the tool appears in the model's tool list. Because
//! the extension is compiled into `pir`, it can call any `crate::*` module
//! (e.g. `crate::term`, `crate::config`). External/binary/lua/http
//! extensions are themselves just more Rust extensions that spawn processes or
//! embed an interpreter — the core never special-cases them.

use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
}

pub struct Outcome {
    pub content: String,
    pub is_error: bool,
}

impl Outcome {
    pub fn ok(content: String) -> Self {
        Outcome { content, is_error: false }
    }
    pub fn err(content: String) -> Self {
        Outcome { content, is_error: true }
    }
}

/// A source of one or more tools. Implement this in an extension and register
/// it via [`Registry::add`] from your `register` entry point.
pub trait ToolBackend: Send {
    /// Human-readable extension name (for logging).
    fn name(&self) -> &'static str;
    /// Tools this backend provides.
    fn specs(&self) -> Vec<ToolSpec>;
    /// Run a tool by name. Return [`Outcome::err`] for unknown names.
    fn run(&mut self, name: &str, input: &Value) -> Outcome;

    /// Called once when the backend is attached to an agent, before the first
    /// turn. `launch_cwd` is the directory pir was started in (used by
    /// extensions that must anchor state to the project rather than the
    /// current working directory).
    fn on_session_start(&mut self, _launch_cwd: &std::path::Path) {}

    /// Called once, after `on_session_start` and after the agent is fully
    /// constructed, to let a backend print a status banner at startup. Return
    /// `Some(text)` and `pir` prints it (dimmed) before the first prompt;
    /// `None` is silent. Default no-op (silent).
    fn startup_report(&mut self) -> Option<String> {
        None
    }

    /// Called once when the agent/REPL is shutting down. Use it to release
    /// resources the backend created (e.g. git worktrees). Returning an error
    /// is logged but does not abort exit.
    fn on_exit(&mut self) {}

    /// Called after each user turn completes (success or provider error), with
    /// the user's prompt text. Extensions use this for side effects that should
    /// follow every prompt — e.g. auto-committing the working tree and deriving
    /// the commit message from `prompt`. An extension may also return
    /// follow-up prompts (strings) that the agent should run as additional
    /// queued turns after this one — e.g. "tests failed, please fix". The agent
    /// only invokes this for non-background turns. Default no-op returning no
    /// follow-ups.
    fn on_turn_end(&mut self, _prompt: &str) -> Vec<String> {
        Vec::new()
    }

    /// Share the REPL's "go silent" switch with this backend, replacing its
    /// private handle. The REPL holds the same `Arc`, so flipping it (to
    /// background a running turn) silences any in-flight progress output the
    /// backend emits — e.g. the `bash` tool's live elapsed clock — without the
    /// REPL owning the worker. Default no-op for backends that don't stream to
    /// the terminal.
    fn set_quiet_handle(&mut self, _q: Arc<AtomicBool>) {}
}

/// Holds every linked backend. The model only ever sees `specs()`; it never
/// knows (or cares) which extension a tool came from.
pub struct Registry {
    cwd: PathBuf,
    full_auto: bool,
    backends: Vec<Box<dyn ToolBackend>>,
    /// Hard-abort flag for the foreground `bash` command, set the instant the
    /// user presses ESC/ctrl-c. The cooperative `cancel` flag only stops a turn
    /// *after* the current step finishes, but a long-running `bash` command can
    /// still be executing inside that step — so the `bash` tool polls this and
    /// kills its child immediately when it flips, aborting the command right
    /// away (not after it exits on its own). The REPL sets it via
    /// [`Registry::abort_active_command`]; the `bash` tool clears it on start.
    pub abort: Arc<AtomicBool>,
}

impl Registry {
    pub fn new(cwd: PathBuf, full_auto: bool, abort: Arc<AtomicBool>) -> Self {
        Registry { cwd, full_auto, backends: Vec::new(), abort }
    }

    /// Signal the running foreground `bash` command to abort immediately. The
    /// `bash` tool checks this between waits and kills its child. The REPL also
    /// flips the cooperative `cancel` flag, so the turn ends after this step.
    /// Always returns true (a no-op abort is harmless even if no command runs).
    pub fn abort_active_command(&mut self) -> bool {
        self.abort.store(true, Ordering::SeqCst);
        true
    }

    /// Link an extension's backend.
    pub fn add(&mut self, backend: Box<dyn ToolBackend>) {
        self.backends.push(backend);
    }

    /// Share the REPL's "go silent" switch with every backend, so a turn
    /// detached to the background (bare `&`) silences their in-flight terminal
    /// output (e.g. the bash tool's live elapsed clock) without the REPL owning
    /// the worker. Call once, right after construction (before the first turn).
    pub fn set_quiet_handle(&mut self, q: Arc<AtomicBool>) {
        for b in &mut self.backends {
            b.set_quiet_handle(q.clone());
        }
    }

    /// Flat list of every tool across all backends — sent to the model.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.backends.iter().flat_map(|b| b.specs()).collect()
    }

    pub fn execute(&mut self, name: &str, input: &Value) -> Outcome {
        for b in &mut self.backends {
            if b.specs().iter().any(|s| s.name == name) {
                return b.run(name, input);
            }
        }
        Outcome::err(format!("unknown tool '{name}'"))
    }

    pub fn cwd(&self) -> &PathBuf {
        &self.cwd
    }
    pub fn full_auto(&self) -> bool {
        self.full_auto
    }
    pub fn len(&self) -> usize {
        self.backends.len()
    }
    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }

    /// Notify every backend that a session has begun.
    pub fn session_started(&mut self, launch_cwd: &std::path::Path) {
        for b in &mut self.backends {
            b.on_session_start(launch_cwd);
        }
    }

    /// Collect startup-report banners from every backend (e.g. the worktree
    /// extension reporting the current worktree). Returns the lines for the
    /// caller to print (dimmed) before the first prompt.
    pub fn startup_reports(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        for b in &mut self.backends {
            if let Some(line) = b.startup_report() {
                out.push(line);
            }
        }
        out
    }

    /// Notify every backend that the agent is shutting down.
    pub fn exited(&mut self) {
        for b in &mut self.backends {
            b.on_exit();
        }
    }

    /// Notify every backend that a user turn just completed. `prompt` is the
    /// text the user submitted for that turn. Returns any follow-up prompts
    /// the backends want the agent to run next (e.g. a "fix the failing tests"
    /// nudge), concatenated across all backends.
    pub fn on_turn_end(&mut self, prompt: &str) -> Vec<String> {
        let mut follow: Vec<String> = Vec::new();
        for b in &mut self.backends {
            follow.extend(b.on_turn_end(prompt));
        }
        follow
    }
}

/// Shared helper: truncate a string in place to `max_chars`, appending a
/// marker. Used by backends that may return large text.
pub fn truncate(s: &mut String, max_chars: usize) {
    if s.chars().count() > max_chars {
        let cut = s.char_indices().nth(max_chars).map(|(i, _)| i).unwrap_or(s.len());
        s.truncate(cut);
        s.push_str("\n… [pir] output truncated");
    }
}
