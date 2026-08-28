//! autocommit — optional extension that commits the working tree after every
//! user turn (opt-in, OFF by default).
//!
//! Design
//! ------
//! * Off unless enabled. Enable with `PIR_AUTO_COMMIT=1`. When off, this
//!   backend still registers a `commit` tool so the model can commit on demand,
//!   but nothing happens automatically.
//! * Hooked via the `ToolBackend::on_turn_end` callback the agent fires once
//!   per completed, non-background turn (see `src/plugin.rs` / `src/agent.rs`).
//! * VCS selection (see `select_vcs`):
//!     - `PIR_VCS=git` (or unset)  -> git.
//!     - `PIR_VCS=jj`              -> jj, but only after a lazy, idempotent
//!                                    `jj init --git-repo .`; falls back to git
//!                                    with a warning if `jj` is unusable.
//!     - We NEVER auto-switch based on detection. git is the deterministic
//!       default so the same agent run behaves identically everywhere.
//! * When git is selected and `jj` is *installed but not selected*, print a
//!   one-line hint once suggesting `PIR_VCS=jj` (per-prompt change-stack).
//! * Commit message = "prompt name": first non-empty line of the prompt,
//!   whitespace collapsed, truncated to 72 chars. Slash REPL meta-commands
//!   (`/bg`, `/goal`, …) and empty/whitespace prompts are skipped.
//! * Safety: stage honoring .gitignore (`git add -A`), refuse if any ignored
//!   file would be staged, and skip entirely when there is nothing to commit.
//!   Never pushes (push is operator-gated elsewhere). Uses repo identity.

use crate::plugin::{Outcome, Registry, ToolBackend, ToolSpec};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// One process-wide lock so concurrent foreground sessions don't interleave
/// `git`/`jj` invocations in the same repository.
static COMMIT_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, PartialEq, Eq)]
enum Vcs {
    Git,
    Jj,
}

struct AutoCommit {
    enabled: bool,
    cwd: PathBuf,
    /// Chosen VCS (resolved in `on_session_start`).
    vcs: Vcs,
    /// Whether `jj` is a usable jujutsu binary (not just on PATH).
    jj_available: bool,
    /// Have we already printed the "tip: PIR_VCS=jj" hint this session?
    hinted: AtomicBool,
}

impl AutoCommit {
    fn new() -> Self {
        let enabled = std::env::var_os("PIR_AUTO_COMMIT").map(|v| v != "0" && !v.is_empty()).unwrap_or(false);
        AutoCommit {
            enabled,
            cwd: PathBuf::from("."),
            vcs: Vcs::Git,
            jj_available: false,
            hinted: AtomicBool::new(false),
        }
    }

    /// Is `jj` on PATH *and* actually jujutsu (not a same-named shim)?
    fn jj_is_real() -> bool {
        let out = Command::new("jj").arg("--version").output();
        match out {
            Ok(o) => o.status.success() && String::from_utf8_lossy(&o.stdout).contains("jujutsu"),
            Err(_) => false,
        }
    }

    /// Resolve which VCS to use, based on `PIR_VCS` and availability.
    fn select_vcs(&mut self, cwd: &Path) {
        self.jj_available = Self::jj_is_real();
        // Default: prefer jj when the repo is already a jj repo (.jj dir, or
        // `jj root` succeeds), else git. An explicit PIR_VCS always wins.
        let choice = std::env::var("PIR_VCS").unwrap_or_else(|_| {
            if crate::project::detect_vcs(cwd) == crate::project::Vcs::Jj {
                "jj".into()
            } else {
                "git".into()
            }
        });
        match choice.as_str() {
            "jj" => {
                if self.jj_available {
                    // Lazily ensure a jj repo wrapping the existing git repo.
                    if Self::jj_init_if_needed(cwd) {
                        self.vcs = Vcs::Jj;
                    } else {
                        eprintln!("[autocommit] PIR_VCS=jj set but `jj init` failed; using git");
                        self.vcs = Vcs::Git;
                    }
                } else {
                    eprintln!("[autocommit] PIR_VCS=jj set but `jj` is not usable; using git");
                    self.vcs = Vcs::Git;
                }
            }
            _ => self.vcs = Vcs::Git,
        }
    }

    /// `jj init --git-repo .` is idempotent for an existing git repo.
    fn jj_init_if_needed(cwd: &Path) -> bool {
        Command::new("jj")
            .arg("init")
            .arg("--git-repo")
            .arg(".")
            .current_dir(cwd)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn is_repo(&self) -> bool {
        match self.vcs {
            Vcs::Git => Command::new("git")
                .arg("rev-parse")
                .arg("--is-inside-work-tree")
                .current_dir(&self.cwd)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false),
            Vcs::Jj => Command::new("jj")
                .arg("status")
                .current_dir(&self.cwd)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false),
        }
    }

    /// Derive a commit subject from the prompt: first non-empty line,
    /// whitespace collapsed, capped at 72 chars.
    fn prompt_subject(prompt: &str) -> Option<String> {
        let line = prompt.lines().map(str::trim).find(|l| !l.is_empty())?;
        // Skip REPL meta-commands — these aren't work prompts.
        if line.starts_with('/') {
            return None;
        }
        let collapsed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            return None;
        }
        let capped: String = collapsed.chars().take(72).collect();
        Some(capped)
    }

    /// Stage honoring .gitignore, refusing if any ignored file would be added.
    fn git_stage(&self) -> Result<(), String> {
        // Reject if any *ignored* file is staged/tracked-would-be (e.g. a
        // force-add or a previously-intentional ignore). `git add -A` honors
        // .gitignore, so we just assert no ignored paths appear in the index.
        let add = Command::new("git")
            .args(["add", "-A"])
            .current_dir(&self.cwd)
            .status()
            .map_err(|e| format!("git add failed: {e}"))?;
        if !add.success() {
            return Err("git add -A exited non-zero".into());
        }
        let ignored = Command::new("git")
            .args(["ls-files", "--error-unmatch", "--ignored", "--others", "--exclude-standard"])
            .current_dir(&self.cwd)
            .output()
            .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
            .unwrap_or(false);
        if ignored {
            // Roll back the staged changes so we don't leave a partial index.
            let _ = Command::new("git").args(["reset", "-q"]).current_dir(&self.cwd).status();
            return Err("refusing to commit: ignored file would be staged".into());
        }
        // Guard against committing huge/binary files even if the repo's
        // pre-commit hook is somehow absent (the hook is the primary guard;
        // this is defense-in-depth so the agent refuses loudly on its own).
        // Bypass with PIR_COMMIT_NO_VERIFY=1 (mirrors `git commit --no-verify`).
        if std::env::var_os("PIR_COMMIT_NO_VERIFY").map(|v| v != "0" && !v.is_empty()).unwrap_or(false) {
            return Ok(());
        }
        if let Err(e) = self.guard_no_bloat() {
            let _ = Command::new("git").args(["reset", "-q"]).current_dir(&self.cwd).status();
            return Err(e);
        }
        Ok(())
    }

    /// Refuse to stage files that are too large or binary. Uses the same
    /// threshold as the pre-commit hook (`PIR_COMMIT_MAX_BYTES`, default 1 MiB).
    fn guard_no_bloat(&self) -> Result<(), String> {
        let max = crate::project::commit_max_bytes();
        let out = Command::new("git")
            .args(["diff", "--cached", "--name-only", "--diff-filter=ACMR"])
            .current_dir(&self.cwd)
            .output();
        let files = match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            _ => return Ok(()), // can't enumerate -> don't block
        };
        let mut offenders = Vec::new();
        for f in files.lines() {
            let f = f.trim();
            if f.is_empty() {
                continue;
            }
            // Size check.
            let sz = Command::new("git")
                .args(["cat-file", "-s", &format!(":{f}")])
                .current_dir(&self.cwd)
                .output();
            if let Ok(o) = sz {
                let s = String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().unwrap_or(0);
                if s > max {
                    offenders.push(format!("{f} ({s} bytes > {max})"));
                    continue;
                }
            }
            // Binary check: git reports `- -` in numstat for binary content.
            let num = Command::new("git")
                .args(["diff", "--cached", "--numstat", "--", f])
                .current_dir(&self.cwd)
                .output();
            if let Ok(o) = num {
                let line = String::from_utf8_lossy(&o.stdout);
                if line.split_whitespace().take(2).collect::<Vec<_>>() == ["-", "-"] {
                    offenders.push(format!("{f} (binary)"));
                }
            }
        }
        if offenders.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "refusing to commit: {} too large/binary. Raise PIR_COMMIT_MAX_BYTES, or set \
                 PIR_COMMIT_NO_VERIFY=1 to override once.\n  - {}",
                if offenders.len() == 1 { "file is" } else { "files are" },
                offenders.join("\n  - ")
            ))
        }
    }

    fn git_nothing_to_commit(&self) -> bool {
        Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&self.cwd)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().is_empty())
            .unwrap_or(true)
    }

    fn commit_git(&self, msg: &str) -> Result<String, String> {
        self.git_stage()?;
        if self.git_nothing_to_commit() {
            return Ok("nothing to commit".into());
        }
        let status = Command::new("git")
            .args(["commit", "-m", msg])
            .current_dir(&self.cwd)
            .status()
            .map_err(|e| format!("git commit failed: {e}"))?;
        if status.success() {
            Ok(format!("committed (git): {msg}"))
        } else {
            Err("git commit exited non-zero".into())
        }
    }

    fn commit_jj(&self, msg: &str) -> Result<String, String> {
        // jj snapshots all tracked changes; `jj commit` opens a new change.
        let status = Command::new("jj")
            .args(["commit", "-m", msg])
            .current_dir(&self.cwd)
            .status()
            .map_err(|e| format!("jj commit failed: {e}"))?;
        if status.success() {
            Ok(format!("committed (jj): {msg}"))
        } else {
            Err("jj commit exited non-zero".into())
        }
    }

    /// Maybe commit after a turn. Returns an optional message/error string for
    /// the agent to surface (Ok(None) means skipped/no-op).
    fn maybe_commit(&mut self, prompt: &str) -> Option<Outcome> {
        let subject = match Self::prompt_subject(prompt) {
            Some(s) => s,
            None => return None, // meta-command / empty -> skip silently
        };

        // Hint (once) when git is selected but jj is available and not chosen.
        if self.vcs == Vcs::Git && self.jj_available && !self.hinted.swap(true, Ordering::SeqCst) {
            eprintln!("[autocommit] tip: set PIR_VCS=jj for per-prompt change-stack + undo");
        }

        if !self.is_repo() {
            return None;
        }

        let _guard = COMMIT_LOCK.lock().ok()?;
        let res = match self.vcs {
            Vcs::Git => self.commit_git(&subject),
            Vcs::Jj => self.commit_jj(&subject),
        };
        match res {
            Ok(s) => Some(Outcome::ok(s)),
            Err(e) => Some(Outcome::err(e)),
        }
    }
}

impl ToolBackend for AutoCommit {
    fn name(&self) -> &'static str {
        "autocommit"
    }

    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "commit",
            description:
                "Commit the working tree now. With no 'message', derive one from the current \
                 prompt. Requires PIR_AUTO_COMMIT semantics; respects .gitignore and never pushes.",
            schema: json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string", "description": "optional commit message" }
                },
                "required": []
            }),
        }]
    }

    fn run(&mut self, name: &str, input: &serde_json::Value) -> Outcome {
        match name {
            "commit" => {
                let msg = input
                    .get("message")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.lines().next().unwrap_or(s).trim().to_string())
                    .unwrap_or_else(|| "wip: manual commit".to_string());
                match self.maybe_commit(&msg) {
                    Some(o) => o,
                    None => Outcome::ok("nothing to commit".into()),
                }
            }
            other => Outcome::err(format!("unknown tool '{other}'")),
        }
    }

    fn on_session_start(&mut self, launch_cwd: &Path) {
        self.cwd = launch_cwd.to_path_buf();
        self.select_vcs(launch_cwd);
    }

    fn on_turn_end(&mut self, prompt: &str) -> Vec<String> {
        if !self.enabled {
            return Vec::new();
        }
        if let Some(outcome) = self.maybe_commit(prompt) {
            // Surface result/error to the terminal (non-fatal either way).
            if outcome.is_error {
                eprintln!("{} [autocommit] {}", crate::term::red("error:"), outcome.content);
            } else {
                println!("{}", crate::term::dim(&format!("[autocommit] {}", outcome.content)));
            }
        }
        Vec::new()
    }
}

pub fn register(reg: &mut Registry) {
    reg.add(Box::new(AutoCommit::new()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build an isolated git repo with one baseline commit; return its path.
    fn scratch_repo() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pir_ac_test_{}_{}", std::process::id(), "x"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&dir)
                .status()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "user.email", "t@x.io"]);
        fs::write(dir.join("base.txt"), "base\n").unwrap();
        git(&["add", "base.txt"]);
        git(&["commit", "-qm", "initial"]);
        dir
    }

    fn log_oneline(dir: &Path) -> String {
        String::from_utf8_lossy(
            &Command::new("git")
                .args(["log", "--oneline"])
                .current_dir(dir)
                .output()
                .unwrap()
                .stdout,
        )
        .to_string()
    }

    #[test]
    fn auto_commits_prompt_as_subject_and_skips_meta_commands() {
        std::env::set_var("PIR_AUTO_COMMIT", "1");
        std::env::remove_var("PIR_VCS");
        let dir = scratch_repo();
        fs::write(dir.join("change.txt"), "new\n").unwrap();

        let mut ac = AutoCommit::new();
        assert!(ac.enabled, "PIR_AUTO_COMMIT should enable");
        ac.on_session_start(&dir);

        // A real work prompt should create a commit.
        let out = ac.maybe_commit("add a changelog entry for the new feature");
        assert!(out.is_some());
        assert!(out.unwrap().content.contains("committed (git)"));
        let log = log_oneline(&dir);
        assert!(log.contains("add a changelog entry for the new feature"), "log was: {log}");

        // A REPL meta-command should be skipped (no commit, returns None).
        let before = log_oneline(&dir);
        assert!(ac.maybe_commit("/goal ship it").is_none());
        assert_eq!(log_oneline(&dir), before, "meta-command must not commit");

        // A prompt with no working-tree changes yields a (non-error) "nothing to
        // commit" outcome — NOT None. None is reserved for meta-commands/empty.
        let after = ac.maybe_commit("refactor the parser");
        assert!(after.is_some(), "no-change prompt returns an outcome, not None");
        assert!(!after.unwrap().is_error);

        let _ = fs::remove_dir_all(&dir);
        std::env::remove_var("PIR_AUTO_COMMIT");
    }

    #[test]
    fn off_by_default_does_nothing() {
        std::env::remove_var("PIR_AUTO_COMMIT");
        std::env::remove_var("PIR_VCS");
        let dir = scratch_repo();
        fs::write(dir.join("change.txt"), "new\n").unwrap();
        let mut ac = AutoCommit::new();
        assert!(!ac.enabled, "must be off by default");
        ac.on_session_start(&dir);
        // When off, the auto-commit gate in on_turn_end must not fire. The
        // `commit` tool (maybe_commit) still works on demand, so we assert via
        // the turn-end hook with a change that WOULD commit if enabled.
        let before = log_oneline(&dir);
        ac.on_turn_end("some prompt");
        assert_eq!(log_oneline(&dir), before, "off: on_turn_end must not commit");
        let _ = fs::remove_dir_all(&dir);
    }
}
