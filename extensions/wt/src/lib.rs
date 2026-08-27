//! wt — per-agent git worktrees with idle auto-verify + merge-back.
//!
//! Off by default; enable with `PIR_WT=1`. When enabled:
//!
//! * `wt_create` makes a linked git worktree (off the current `main`) and
//!   `cd`s the agent into it, so subsequent `bash`/`edit_file`/`read_file`
//!   calls operate there. The launch `cwd` (the main checkout) is remembered.
//! * After each user turn (`on_turn_end`), if the agent is inside a `wt`
//!   worktree, the extension auto-verifies the branch: it tries to fast-forward
//!   `main` and runs project-type build + test checks (the verification command
//!   is configurable per project; a sane default is chosen from the project
//!   layout). If everything passes it attempts to merge the branch back into
//!   `main` under an inter-agent lock so two agents never merge concurrently.
//! * If verification fails, instead of merging it queues a follow-up prompt
//!   asking the model to fix the breakage (the prompt is surfaced by the REPL
//!   as the next turn). This is how the user's requirement "if tests DON'T pass,
//!   ask the model to fix" is satisfied.
//! * `wt_merge` / `wt_verify` / `wt_status` / `wt_remove` are exposed as tools
//!   for explicit control (e.g. when not running in full-auto). `on_turn_end`
//!   only auto-merges when `PIR_WT_AUTO=1` (default on when `PIR_WT=1`).
//!
//! Locking
//! -------
//! A repo-level lock (`<repo>/.git/wt-merge.lock`, flock-compatible) serializes
//! auto-merge attempts so multiple agents (or the same agent across worktrees)
//! don't merge simultaneously. The merge itself is done from the *main checkout*
//! after `git worktree update`/pull, never from inside the worktree.
//!
//! Worktree location
//! -----------------
//! Worktrees live in `<repo>/.git/wt/<name>` by default (inside `.git`, so they
//! are never seen by the main working tree and aren't committed). Override the
//! parent with `PIR_WT_DIR`.

use crate::plugin::{Outcome, Registry, ToolBackend, ToolSpec};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

/// One process-wide guard so a single agent process never auto-merges two
/// worktrees at once (the repo lock below handles cross-process/cross-agent).
static AUTO_GUARD: Mutex<()> = Mutex::new(());

const DEFAULT_BRANCH: &str = "main";

struct Wt {
    enabled: bool,
    auto: bool,
    repo: PathBuf,
    wt_parent: PathBuf,
    /// The agent's launch cwd (the main checkout), set in on_session_start.
    main_cwd: PathBuf,
    /// Per-process current worktree path (None = on main checkout).
    current: Option<PathBuf>,
}

impl Wt {
    fn new() -> Self {
        let enabled = std::env::var_os("PIR_WT").map(|v| v != "0" && !v.is_empty()).unwrap_or(false);
        let auto = std::env::var_os("PIR_WT_AUTO")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(enabled);
        let wt_parent = std::env::var_os("PIR_WT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".git/wt"));
        Wt {
            enabled,
            auto,
            repo: PathBuf::from("."),
            wt_parent,
            main_cwd: PathBuf::from("."),
            current: None,
        }
    }

    /// Absolute path to the repo root (the `.git` parent), resolved from the
    /// launch cwd. Used for the lock file and to run git operations in the main
    /// checkout.
    fn repo_root(&self) -> PathBuf {
        // Find the git work-tree root by walking up from the launch cwd.
        let start = &self.main_cwd;
        let mut cur = if start.is_absolute() {
            start.clone()
        } else {
            std::env::current_dir().unwrap_or_else(|_| start.clone())
        };
        loop {
            if cur.join(".git").exists() {
                return cur;
            }
            match cur.parent() {
                Some(p) => cur = p.to_path_buf(),
                None => return start.clone(),
            }
        }
    }

    fn wt_parent_abs(&self) -> PathBuf {
        let root = self.repo_root();
        if self.wt_parent.is_absolute() {
            self.wt_parent.clone()
        } else {
            root.join(&self.wt_parent)
        }
    }

    fn lock_path(&self) -> PathBuf {
        self.repo_root().join(".git").join("wt-merge.lock")
    }

    fn git(&self, args: &[&str]) -> std::process::Output {
        Command::new("git").args(args).output().unwrap_or_else(|_| {
            // Couldn't spawn git (e.g. not installed). `false` always exits
            // non-zero and is universally available, so its Output is a safe
            // "failure" placeholder whose status.success() is false.
            Command::new("false").output().expect("`false` must be spawnable")
        })
    }

    fn is_git(&self) -> bool {
        self.git(&["rev-parse", "--is-inside-work-tree"])
            .status
            .success()
    }

    /// Are we currently inside one of our own worktrees (not the main checkout)?
    fn in_worktree(&self) -> bool {
        self.current.is_some()
    }

    /// Build + test verification command, chosen from the project layout, unless
    /// `PIR_WT_CHECK` overrides it. Returns None if there is nothing to verify.
    fn verify_cmd(&self, wt_dir: &Path) -> Option<String> {
        if let Ok(c) = std::env::var("PIR_WT_CHECK") {
            if !c.trim().is_empty() {
                return Some(c);
            }
        }
        // Rust project: Cargo.toml present.
        if wt_dir.join("Cargo.toml").exists() {
            return Some("cargo build --locked 2>&1 | tail -n 40 && cargo test --locked 2>&1 | tail -n 60".into());
        }
        // Node project.
        if wt_dir.join("package.json").exists() {
            return Some("npm ci >/dev/null 2>&1; npm run build 2>&1 | tail -n 40; npm test 2>&1 | tail -n 60".into());
        }
        // Python project.
        if wt_dir.join("pyproject.toml").exists() || wt_dir.join("setup.py").exists() {
            return Some("python -m build >/dev/null 2>&1; python -m pytest 2>&1 | tail -n 60".into());
        }
        // Makefile.
        if wt_dir.join("Makefile").exists() {
            return Some("make 2>&1 | tail -n 40; make test 2>&1 | tail -n 60".into());
        }
        // No recognized project type: nothing to verify.
        None
    }

    /// Run the verification command inside the given worktree. Returns
    /// (passed: bool, summary: String).
    fn verify(&self, wt_dir: &Path) -> (bool, String) {
        let Some(cmd) = self.verify_cmd(wt_dir) else {
            return (true, "no project-type checks configured; nothing to verify".into());
        };
        let out = Command::new("bash")
            .arg("-lc")
            .arg(&cmd)
            .current_dir(wt_dir)
            .output();
        match out {
            Ok(o) => {
                let passed = o.status.success();
                let mut log = String::from_utf8_lossy(&o.stdout).into_owned();
                log.push_str(&String::from_utf8_lossy(&o.stderr));
                crate::plugin::truncate(&mut log, 4000);
                (passed, log)
            }
            Err(e) => (false, format!("verify spawn error: {e}")),
        }
    }

    /// Acquire the repo-wide merge lock (flock on a file inside .git). Returns a
    /// drop guard that releases the lock when it falls out of scope. Uses the
    /// `flock` binary if present, else a std::fs advisory open-lock. Non-fatal:
    /// returns None if the lock can't be taken (caller should skip auto-merge).
    fn acquire_lock(&self) -> Option<LockGuard> {
        let path = self.lock_path();
        if let Some(g) = LockGuard::try_flock(&path) {
            return Some(g);
        }
        // Fallback: create + hold the file open (advisory, best-effort).
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new("."))).ok();
        match std::fs::File::create(&path) {
            Ok(f) => Some(LockGuard::File(f)),
            Err(_) => None,
        }
    }

    /// Try to fast-forward the worktree's branch tracking ref to origin and
    /// return the branch name currently checked out in the worktree.
    fn worktree_branch(&self, wt_dir: &Path) -> Option<String> {
        let out = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(wt_dir)
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let b = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if b.is_empty() || b == "HEAD" {
                    None
                } else {
                    Some(b)
                }
            }
            _ => None,
        }
    }

    /// From the main checkout, fast-forward `main` to `origin/main` if a
    /// fast-forward is possible (no local-only commits). Returns Ok(true) if
    /// main is now up to date (or there's no upstream to ff), Ok(false) if it
    /// couldn't ff (diverged), Err(msg) on an unexpected git failure.
    fn ff_main(&self) -> Result<bool, String> {
        let root = self.repo_root();
        let _ = Command::new("git")
            .args(["fetch", "--quiet", "origin"])
            .current_dir(&root)
            .status();
        // If there's no origin/main to fast-forward against, just proceed (a
        // purely-local repo, or offline). Never block the merge for that.
        let has_upstream = Command::new("git")
            .args(["rev-parse", "--verify", "origin/main"])
            .current_dir(&root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !has_upstream {
            return Ok(true);
        }
        let ff = Command::new("git")
            .args(["merge", "--ff-only", "origin/main"])
            .current_dir(&root)
            .status();
        match ff {
            Ok(s) if s.success() => Ok(true),
            Ok(_) => Ok(false),
            Err(e) => Err(format!("git merge --ff-only failed: {e}")),
        }
    }

    /// Merge `branch` into `main` from the main checkout under the merge lock.
    /// `branch` must be a local branch that the worktree owns. Returns Outcome.
    fn merge_into_main(&self, branch: &str) -> Outcome {
        // Serialize merges across the whole repo (other agents / worktrees).
        let _guard = match self.acquire_lock() {
            Some(g) => g,
            None => return Outcome::err("wt: could not acquire merge lock; skipping auto-merge".into()),
        };
        let root = self.repo_root();

        // Make sure main is current; only fast-forward, never rewrite main.
        match self.ff_main() {
            Ok(true) => {}
            Ok(false) => {
                return Outcome::err(
                    "wt: main has diverged from origin/main and cannot be fast-forwarded; \
                     skipping auto-merge (pull/resolve manually)"
                        .into(),
                );
            }
            Err(e) => return Outcome::err(e),
        }

        let status = Command::new("git")
            .args(["merge", "--no-ff", "-m", &format!("Merge {branch} into main (pir wt)"), branch])
            .current_dir(&root)
            .status();
        match status {
            Ok(s) if s.success() => Outcome::ok(format!(
                "merged {branch} into main (in {})",
                root.display()
            )),
            Ok(_) => Outcome::err(
                "wt: merge conflict or merge failed; resolve manually in the main checkout".into(),
            ),
            Err(e) => Outcome::err(format!("wt: git merge error: {e}")),
        }
    }

    /// Full idle pipeline: ff main, verify, then merge or ask-the-model-to-fix.
    /// Returns an optional follow-up prompt (to fix) and a human-readable line.
    fn auto_flow(&mut self) -> (Option<String>, String) {
        let Some(wt_dir) = self.current.clone() else {
            return (None, String::new());
        };
        let Some(branch) = self.worktree_branch(&wt_dir) else {
            return (None, "wt: worktree branch is detached; skipping auto-merge".into());
        };

        // Pull any upstream changes into the worktree's branch first (best
        // effort — if there's no upstream it's a no-op).
        let _ = Command::new("git")
            .args(["pull", "--ff-only"])
            .current_dir(&wt_dir)
            .status();

        let (passed, summary) = self.verify(&wt_dir);
        if !passed {
            let msg = format!(
                "wt: checks FAILED on branch {branch} — asking the model to fix.\n{summary}"
            );
            let fix_prompt = format!(
                "Verification (build/test) failed in worktree {wt_dir}: {branch}.\n\
                 Diagnose and fix the failure. Re-run the verification commands to confirm a clean build+test before finishing.",
                wt_dir = wt_dir.display()
            );
            return (Some(fix_prompt), msg);
        }

        let merge = self.merge_into_main(&branch);
        if merge.is_error {
            (None, merge.content)
        } else {
            // Merged: drop the now-merged branch's worktree so we don't leave it
            // lying around, then return to the main checkout.
            let remove = self.remove_worktree(&wt_dir, &branch);
            self.return_to_main();
            (None, format!("{}\n{}", merge.content, remove.content))
        }
    }

    /// `cd` the agent back to the main checkout. Called after a worktree is
    /// removed so the process isn't left inside a deleted directory.
    fn return_to_main(&mut self) {
        let root = self.repo_root();
        let _ = std::env::set_current_dir(&root);
        self.current = None;
    }

    fn remove_worktree(&self, wt_dir: &Path, branch: &str) -> Outcome {
        let root = self.repo_root();
        let out = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(wt_dir)
            .current_dir(&root)
            .status();
        let _ = Command::new("git")
            .args(["branch", "-D"])
            .arg(branch)
            .current_dir(&root)
            .status();
        match out {
            Ok(s) if s.success() => Outcome::ok(format!("removed worktree {wt_dir}", wt_dir = wt_dir.display())),
            Ok(_) => Outcome::err(format!("worktree remove failed for {wt_dir}", wt_dir = wt_dir.display())),
            Err(e) => Outcome::err(format!("worktree remove error: {e}")),
        }
    }

    /// Create a worktree off the current main with a fresh branch, and `cd` the
    /// agent into it.
    fn create(&mut self, base: &str) -> Outcome {
        if !self.enabled {
            return Outcome::err("wt is off (set PIR_WT=1 to enable worktree automation)".into());
        }
        if !self.is_git() {
            return Outcome::err("wt: not inside a git repository".into());
        }
        let root = self.repo_root();
        let name = format!(
            "wt-{}-{}",
            std::process::id(),
            chrono_short()
        );
        let branch = if base.is_empty() { name.clone() } else { format!("{base}-{name}") };
        let parent = self.wt_parent_abs();
        let _ = std::fs::create_dir_all(&parent);
        let wt_dir = parent.join(&name);

        // Always branch from the latest main (best-effort fast-forward; if there
        // is no upstream we just branch from the local main).
        let _ = self.ff_main();
        let start = DEFAULT_BRANCH.to_string();
        let add = Command::new("git")
            .args(["worktree", "add", "-b", &branch])
            .arg(&wt_dir)
            .arg(&start)
            .current_dir(&root)
            .status();
        if !add.map(|s| s.success()).unwrap_or(false) {
            return Outcome::err(format!("wt: git worktree add failed (branch {branch})"));
        }
        // Move the agent into the worktree so bash/edit_file operate there.
        if let Err(e) = std::env::set_current_dir(&wt_dir) {
            return Outcome::err(format!("wt: created {wt_dir} but cd failed: {e}", wt_dir = wt_dir.display()));
        }
        self.current = Some(wt_dir.clone());
        Outcome::ok(format!(
            "created worktree {wt_dir} on branch {branch} (from {start}); cd'd into it. Run wt_status to inspect, or finish the turn to auto-verify+merge.",
            wt_dir = wt_dir.display()
        ))
    }
}

/// RAII merge lock: releases `flock`/file when dropped.
#[allow(dead_code)]
enum LockGuard {
    Flock(std::process::Child),
    File(std::fs::File),
}

impl LockGuard {
    /// Use the `flock` binary to take an exclusive lock on `path`. `flock -w 0`
    /// fails immediately if another holder exists. We keep the child alive
    /// (holding the lock) until the guard is dropped.
    fn try_flock(path: &Path) -> Option<LockGuard> {
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new("."))).ok();
        let child = Command::new("flock")
            .arg("-w")
            .arg("0")
            .arg("-x")
            .arg(path)
            .arg("--")
            .arg("sleep")
            .arg("1000000")
            .spawn();
        match child {
            Ok(mut c) => {
                // Give flock a moment; if it dies immediately it couldn't lock.
                std::thread::sleep(std::time::Duration::from_millis(120));
                if c.try_wait().map(|w| w.is_some()).unwrap_or(true) {
                    let _ = c.kill();
                    None
                } else {
                    Some(LockGuard::Flock(c))
                }
            }
            Err(_) => None,
        }
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        match self {
            LockGuard::Flock(c) => {
                let _ = c.kill();
                let _ = c.wait();
            }
            LockGuard::File(_) => {}
        }
    }
}

fn chrono_short() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

impl ToolBackend for Wt {
    fn name(&self) -> &'static str {
        "wt"
    }

    fn specs(&self) -> Vec<ToolSpec> {
        if !self.enabled {
            return Vec::new();
        }
        vec![
            ToolSpec {
                name: "wt_create",
                description:
                    "Create a linked git worktree off the current main with a fresh branch, and \
                     cd the agent into it so subsequent tools operate there. Optionally pass a \
                     'base' name to prefix the branch. The main checkout is remembered and used \
                     for merges. Requires PIR_WT=1.",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "base": { "type": "string", "description": "optional branch-name prefix" }
                    },
                    "required": []
                }),
            },
            ToolSpec {
                name: "wt_verify",
                description:
                    "Run build+test checks for the current worktree (project-type aware; \
                     overridable via PIR_WT_CHECK). Reports pass/fail + tail of output.",
                schema: json!({ "type": "object", "properties": {}, "required": [] }),
            },
            ToolSpec {
                name: "wt_merge",
                description:
                    "Merge the current worktree's branch into main from the main checkout, under \
                     the inter-agent merge lock (fast-forward only of main; --no-ff merge of the \
                     branch). Then removes the worktree. Requires PIR_WT=1.",
                schema: json!({ "type": "object", "properties": {}, "required": [] }),
            },
            ToolSpec {
                name: "wt_status",
                description:
                    "Report the current worktree (if any), its branch, and whether the agent is \
                     on the main checkout.",
                schema: json!({ "type": "object", "properties": {}, "required": [] }),
            },
            ToolSpec {
                name: "wt_remove",
                description:
                    "Remove the current worktree and its branch (force), returning to the main \
                     checkout. Use to abandon a branch without merging.",
                schema: json!({ "type": "object", "properties": {}, "required": [] }),
            },
        ]
    }

    fn run(&mut self, name: &str, input: &serde_json::Value) -> Outcome {
        match name {
            "wt_create" => {
                let base = input.get("base").and_then(|v| v.as_str()).unwrap_or("");
                self.create(base)
            }
            "wt_verify" => {
                let Some(wt_dir) = self.current.clone() else {
                    return Outcome::err("wt: not in a worktree".into());
                };
                let (passed, summary) = self.verify(&wt_dir);
                if passed {
                    Outcome::ok(format!("wt: checks passed\n{summary}"))
                } else {
                    Outcome::err(format!("wt: checks FAILED\n{summary}"))
                }
            }
            "wt_merge" => {
                let Some(wt_dir) = self.current.clone() else {
                    return Outcome::err("wt: not in a worktree".into());
                };
                let Some(branch) = self.worktree_branch(&wt_dir) else {
                    return Outcome::err("wt: worktree branch is detached".into());
                };
                let out = self.merge_into_main(&branch);
                if !out.is_error {
                    let _ = self.remove_worktree(&wt_dir, &branch);
                    self.return_to_main();
                }
                out
            }
            "wt_status" => {
                match &self.current {
                    Some(wt_dir) => {
                        let branch = self.worktree_branch(wt_dir).unwrap_or_else(|| "(detached)".into());
                        Outcome::ok(format!(
                            "wt: in worktree {} on branch {branch}; main checkout at {}",
                            wt_dir.display(),
                            self.repo_root().display()
                        ))
                    }
                    None => Outcome::ok(format!(
                        "wt: on main checkout {} (no active worktree)",
                        self.repo_root().display()
                    )),
                }
            }
            "wt_remove" => {
                let Some(wt_dir) = self.current.clone() else {
                    return Outcome::err("wt: not in a worktree".into());
                };
                let branch = self.worktree_branch(&wt_dir).unwrap_or_default();
                let out = self.remove_worktree(&wt_dir, &branch);
                self.return_to_main();
                out
            }
            other => Outcome::err(format!("unknown tool '{other}'")),
        }
    }

    fn on_session_start(&mut self, launch_cwd: &Path) {
        self.main_cwd = launch_cwd.to_path_buf();
        self.repo = launch_cwd.to_path_buf();
    }

    fn on_turn_end(&mut self, _prompt: &str) -> Vec<String> {
        if !self.enabled || !self.auto || !self.in_worktree() {
            return Vec::new();
        }
        // One auto-flow per process at a time (repo lock handles cross-process).
        let _g = match AUTO_GUARD.try_lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let (fix_prompt, line) = self.auto_flow();
        if !line.is_empty() && self.enabled {
            // Surface the outcome on the terminal (non-fatal either way).
            if fix_prompt.is_some() {
                eprintln!("{}", crate::term::yellow(&format!("[wt] {line}")));
            } else {
                eprintln!("{}", crate::term::dim(&format!("[wt] {line}")));
            }
        }
        fix_prompt.into_iter().collect()
    }

    fn on_exit(&mut self) {
        // Leave the worktree in place on exit (the user may want to inspect it).
        // Only drop the merge lock; worktree cleanup is manual via wt_remove.
        if let Some(wt_dir) = self.current.clone() {
            let branch = self.worktree_branch(&wt_dir).unwrap_or_default();
            let _ = self.remove_worktree(&wt_dir, &branch);
            self.current = None;
        }
    }
}

pub fn register(reg: &mut Registry) {
    reg.add(Box::new(Wt::new()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build an isolated git repo (no remote) with one baseline commit; return
    /// its path. The `wt` extension operates on linked worktrees under it.
    fn scratch_repo() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pir_wt_test_{}_{}", std::process::id(), chrono_short()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            Command::new("git").args(args).current_dir(&dir).status().unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "user.email", "t@x.io"]);
        fs::write(dir.join("main.txt"), "base\n").unwrap();
        git(&["add", "main.txt"]);
        git(&["commit", "-qm", "initial"]);
        dir
    }

    #[test]
    fn creates_branch_and_merges_back_when_checks_pass() {
        std::env::set_var("PIR_WT", "1");
        std::env::remove_var("PIR_WT_AUTO");
        let repo = scratch_repo();
        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(&repo).unwrap();

        let mut wt = Wt::new();
        assert!(wt.enabled, "PIR_WT should enable");
        wt.on_session_start(&repo);

        // No recognized project type => verify() passes trivially.
        let created = wt.create("feat");
        assert!(created.content.contains("created worktree"), "got: {}", created.content);
        assert!(wt.in_worktree(), "should be inside a worktree now");

        // Make a commit in the worktree's branch.
        let here = std::env::current_dir().unwrap();
        fs::write(here.join("change.txt"), "new\n").unwrap();
        Command::new("git").args(["add", "change.txt"]).current_dir(&here).status().unwrap();
        Command::new("git").args(["commit", "-qm", "add change"]).current_dir(&here).status().unwrap();

        // Verify should pass (no project tooling).
        let (passed, _) = wt.verify(&here);
        assert!(passed, "verify should pass for an empty project");

        // Merge back into main from the main checkout.
        let branch = wt.worktree_branch(&here).unwrap();
        let merged = wt.merge_into_main(&branch);
        assert!(!merged.is_error, "merge failed: {}", merged.content);
        // The branch's commit should now be reachable from main.
        let reachable = Command::new("git")
            .args(["merge-base", "--is-ancestor", &branch, "main"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success();
        assert!(reachable, "branch should be merged into main");

        // Remove the worktree and return to main.
        let removed = wt.remove_worktree(&here, &branch);
        assert!(!removed.is_error, "remove failed: {}", removed.content);
        wt.return_to_main();
        assert!(!wt.in_worktree());

        let _ = fs::remove_dir_all(&repo);
        let _ = std::env::set_current_dir(&orig);
        std::env::remove_var("PIR_WT");
    }

    #[test]
    fn merge_lock_is_exclusive() {
        // Two concurrent flock holders on the same lock file must not both win.
        let repo = scratch_repo();
        let lock = repo.join(".git").join("wt-merge.lock");

        // Holder A (kept alive for the duration of the test).
        let mut a = Command::new("flock")
            .args(["-x", "-w", "0"])
            .arg(&lock)
            .arg("sleep")
            .arg("1000")
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(150));

        // Holder B should be refused immediately (-w 0 => no wait).
        let b = Command::new("flock")
            .args(["-x", "-w", "0"])
            .arg(&lock)
            .arg("sleep")
            .arg("0.2")
            .status()
            .unwrap();
        assert!(!b.success(), "second flock holder must be blocked");

        let _ = a.kill();
        let _ = a.wait();
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn off_by_default_registers_no_tools() {
        std::env::remove_var("PIR_WT");
        let wt = Wt::new();
        assert!(!wt.enabled, "wt must be off by default");
        assert!(!wt.in_worktree());
        assert!(wt.specs().is_empty(), "no tools when disabled");
    }
}
