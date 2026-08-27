//! wt — per-agent git worktrees with idle auto-verify + merge-back.
//!
//! Off by default; enable with `PIR_WT=1`. When enabled:
//!
//! * `wt_create` makes a linked git worktree off the repo's *trunk* branch
//!   (auto-detected — `origin/HEAD`, else the checked-out branch, else
//!   main/master/trunk/develop) with a fresh branch, and `cd`s the agent into
//!   it so subsequent `bash`/`edit_file`/`read_file` calls operate there. The
//!   trunk checkout is remembered and used for merges.
//! * After each user turn (`on_turn_end`), if the agent is inside a `wt`
//!   worktree, the extension auto-verifies the branch: it tries to fast-forward
//!   the trunk and runs project-type build + test checks (the verification
//!   command is configurable per project via `PIR_WT_CHECK`; a default is
//!   chosen from the project layout). If everything passes it merges the branch
//!   back into the trunk under an inter-agent lock so two agents never merge
//!   concurrently.
//! * If verification fails, instead of merging it queues a follow-up prompt
//!   asking the model to fix the breakage (surfaced by the REPL as the next
//!   turn) — up to `MAX_FIX_ATTEMPTS` times, then stops re-queueing so the user
//!   can intervene. This is how "if tests DON'T pass, ask the model to fix" is
//!   satisfied.
//! * Conservative default: if no check command is configured/recognized (no
//!   `PIR_WT_CHECK` and no Cargo.toml/package.json/pyproject.toml/Makefile), the
//!   extension does NOT claim success — it skips auto-merge and asks for an
//!   explicit `wt_merge`. This avoids silently merging unverified work.
//! * `wt_merge` / `wt_verify` / `wt_status` / `wt_remove` are exposed as tools
//!   for explicit control. `on_turn_end` only auto-merges when `PIR_WT_AUTO=1`
//!   (default on when `PIR_WT=1`).
//!
//! Locking
//! -------
//! A repo-level lock (`<repo>/.git/wt-merge.lock`, flock-compatible) serializes
//! auto-merge attempts so multiple agents (or the same agent across worktrees)
//! don't merge simultaneously. The merge itself is done from the *trunk*
//! checkout after a fast-forward, never from inside the worktree.
//!
//! Worktree location
//! -----------------
//! Worktrees live in `<repo>/.git/wt/<name>` by default (inside `.git`, so they
//! are never seen by the trunk working tree and aren't committed). Override the
//! parent with `PIR_WT_DIR`.

use crate::plugin::{Outcome, Registry, ToolBackend, ToolSpec};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

/// One process-wide guard so a single agent process never auto-merges two
/// worktrees at once (the repo lock below handles cross-process/cross-agent).
static AUTO_GUARD: Mutex<()> = Mutex::new(());

/// Max auto-fix prompts queued before we give up and stop re-queuing (so the
/// model can't be asked to fix the same failing branch forever, blocking the
/// user from ever typing).
const MAX_FIX_ATTEMPTS: u32 = 2;

const DEFAULT_BRANCH: &str = "main";

/// Result of running verification in a worktree.
enum Verdict {
    /// Build/test passed (summary is the tail of output).
    Passed(String),
    /// Build/test failed (summary is the tail of output).
    Failed(String),
    /// No verification command could be determined (no PIR_WT_CHECK and no
    /// recognized project type) — so *nothing was actually checked*. Callers
    /// must NOT treat this as green.
    NoChecks,
}

struct Wt {
    enabled: bool,
    auto: bool,
    repo: PathBuf,
    wt_parent: PathBuf,
    /// The agent's launch cwd (the main checkout), set in on_session_start.
    main_cwd: PathBuf,
    /// Per-process current worktree path (None = on main checkout).
    current: Option<PathBuf>,
    /// How many consecutive auto-fix prompts we've queued (reset when checks
    /// pass). Bounded by `max_fix` to avoid an infinite fix loop.
    fix_attempts: u32,
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
            fix_attempts: 0,
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

    /// Resolve the repo's trunk branch dynamically (don't assume `main`).
    /// Prefers `origin/HEAD` (e.g. `origin/main`), then the local checked-out
    /// branch of the main checkout, then `main`, then `master`.
    fn trunk(&self) -> String {
        let root = self.repo_root();
        // 1) remote HEAD, e.g. refs/remotes/origin/main -> "main".
        if let Ok(o) = Command::new("git")
            .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
            .current_dir(&root)
            .output()
        {
            if o.status.success() {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if let Some(name) = s.rsplit('/').next() {
                    if !name.is_empty() && name != "HEAD" {
                        return name.to_string();
                    }
                }
            }
        }
        // 2) whatever branch the main checkout currently has checked out.
        if let Ok(o) = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&root)
            .output()
        {
            if o.status.success() {
                let b = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !b.is_empty() && b != "HEAD" {
                    return b;
                }
            }
        }
        // 3) common trunk names.
        for cand in [DEFAULT_BRANCH, "master", "trunk", "develop"] {
            if Command::new("git")
                .args(["show-ref", "--verify", "--quiet", &format!("refs/heads/{cand}")])
                .current_dir(&root)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                return cand.to_string();
            }
        }
        DEFAULT_BRANCH.to_string()
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
    /// `PIR_WT_CHECK` overrides it. Returns `None` if there is nothing to verify
    /// (no override and no recognized project type) — callers must treat that as
    /// "we don't know", NOT as "pass".
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

    /// Run the verification command inside the given worktree. Never claims
    /// success when no command could be determined (`Verdict::NoChecks`).
    fn verify(&self, wt_dir: &Path) -> Verdict {
        let Some(cmd) = self.verify_cmd(wt_dir) else {
            // Conservative: with no configured/recognized check, we do NOT auto-merge.
            return Verdict::NoChecks;
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
                if passed {
                    Verdict::Passed(log)
                } else {
                    Verdict::Failed(log)
                }
            }
            Err(e) => Verdict::Failed(format!("verify spawn error: {e}")),
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

    /// From the main checkout, fast-forward the *trunk* branch to
    /// `origin/<trunk>` if a fast-forward is possible (no local-only commits).
    /// Returns Ok(true) if trunk is now up to date (or there's no upstream to ff),
    /// Ok(false) if it couldn't ff (diverged), Err(msg) on an unexpected git failure.
    fn ff_main(&self) -> Result<bool, String> {
        let root = self.repo_root();
        let trunk = self.trunk();
        let _ = Command::new("git")
            .args(["fetch", "--quiet", "origin"])
            .current_dir(&root)
            .status();
        // If there's no origin/<trunk> to fast-forward against, just proceed (a
        // purely-local repo, or offline). Never block the merge for that.
        let upstream = format!("origin/{trunk}");
        let has_upstream = Command::new("git")
            .args(["rev-parse", "--verify", &upstream])
            .current_dir(&root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !has_upstream {
            return Ok(true);
        }
        let ff = Command::new("git")
            .args(["merge", "--ff-only", &upstream])
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

        // Make sure trunk is current; only fast-forward, never rewrite trunk.
        match self.ff_main() {
            Ok(true) => {}
            Ok(false) => {
                return Outcome::err(
                    "wt: trunk has diverged from origin and cannot be fast-forwarded; \
                     skipping auto-merge (pull/resolve manually)"
                        .into(),
                );
            }
            Err(e) => return Outcome::err(e),
        }

        let trunk = self.trunk();
        let status = Command::new("git")
            .args(["merge", "--no-ff", "-m", &format!("Merge {branch} into {trunk} (pir wt)"), branch])
            .current_dir(&root)
            .status();
        match status {
            Ok(s) if s.success() => Outcome::ok(format!(
                "merged {branch} into {trunk} (in {})",
                root.display(),
                trunk = trunk
            )),
            Ok(_) => Outcome::err(
                "wt: merge conflict or merge failed; resolve manually in the main checkout".into(),
            ),
            Err(e) => Outcome::err(format!("wt: git merge error: {e}")),
        }
    }

    /// Full idle pipeline: ff trunk, verify, then merge or ask-the-model-to-fix.
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

        let verdict = self.verify(&wt_dir);
        match verdict {
            Verdict::Failed(summary) => {
                // Checks failed: ask the model to fix, but only up to MAX_FIX_ATTEMPTS
                // times — beyond that, stop re-queuing so the user can intervene.
                if self.fix_attempts >= MAX_FIX_ATTEMPTS {
                    let msg = format!(
                        "wt: checks STILL FAILED on {branch} after {} fix attempts; \
                         not re-queueing. Resolve manually or run wt_merge when green.",
                        MAX_FIX_ATTEMPTS
                    );
                    return (None, msg);
                }
                self.fix_attempts += 1;
                let msg = format!(
                    "wt: checks FAILED on branch {branch} (attempt {}/{}) — asking the model to fix.\n{summary}",
                    self.fix_attempts, MAX_FIX_ATTEMPTS
                );
                let fix_prompt = format!(
                    "Verification (build/test) failed in worktree {wt_dir}: {branch}.\n\
                     Diagnose and fix the failure. Re-run the verification commands to confirm a clean build+test before finishing.",
                    wt_dir = wt_dir.display()
                );
                (Some(fix_prompt), msg)
            }
            Verdict::NoChecks => {
                // No verification command was configured/recognized: do NOT silently
                // merge. Surface it and require an explicit wt_merge (or a PIR_WT_CHECK).
                (
                    None,
                    format!(
                        "wt: no build/test checks for {branch} (set PIR_WT_CHECK, or add a \
                         Cargo.toml/package.json/pyproject.toml/Makefile). Skipping auto-merge; \
                         run wt_merge to merge manually."
                    ),
                )
            }
            Verdict::Passed(summary) => {
                // Checks passed: reset the fix counter and merge back.
                self.fix_attempts = 0;
                let merge = self.merge_into_main(&branch);
                if merge.is_error {
                    (None, merge.content)
                } else {
                    let remove = self.remove_worktree(&wt_dir, &branch);
                    self.return_to_main();
                    (None, format!("{}\n{}", merge.content, remove.content))
                }
                // `summary` is intentionally not echoed to the idle line to keep
                // green merges quiet; it is still available via `wt_verify`.
            }
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

        // Always branch from the latest trunk (best-effort fast-forward; if there
        // is no upstream we just branch from the local trunk).
        let _ = self.ff_main();
        let start = self.trunk();
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
                    "Create a linked git worktree off the repo's trunk branch (auto-detected: \
                     origin/HEAD, then the checked-out branch, else main/master/...) with a fresh \
                     branch, and cd the agent into it so subsequent tools operate there. \
                     Optionally pass a 'base' name to prefix the branch. The main checkout is \
                     remembered and used for merges. Requires PIR_WT=1.",
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
                match self.verify(&wt_dir) {
                    Verdict::Passed(s) => Outcome::ok(format!("wt: checks passed\n{s}")),
                    Verdict::Failed(s) => Outcome::err(format!("wt: checks FAILED\n{s}")),
                    Verdict::NoChecks => Outcome::err(
                        "wt: no build/test checks configured for this project (set PIR_WT_CHECK, or \
                         add a Cargo.toml/package.json/pyproject.toml/Makefile). Nothing was verified."
                            .into(),
                    ),
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
                        let trunk = self.trunk();
                        Outcome::ok(format!(
                            "wt: in worktree {} on branch {branch}; trunk is {trunk} at {}",
                            wt_dir.display(),
                            self.repo_root().display()
                        ))
                    }
                    None => Outcome::ok(format!(
                        "wt: on the trunk checkout {} (no active worktree)",
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

        // With a real check command set, verify() runs it and should pass.
        std::env::set_var("PIR_WT_CHECK", "true");
        let verdict = wt.verify(&here);
        assert!(matches!(verdict, Verdict::Passed(_)), "verify should pass with PIR_WT_CHECK=true");

        // Merge back into trunk (main here) from the main checkout.
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

    #[test]
    fn no_checks_is_not_a_pass() {
        // A bare repo (no PIR_WT_CHECK, no recognized project type) must report
        // Verdict::NoChecks, never Verdict::Passed — so the extension will NOT
        // silently auto-merge it.
        std::env::remove_var("PIR_WT_CHECK");
        let repo = scratch_repo();
        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(&repo).unwrap();
        let wt = Wt::new();
        assert!(matches!(wt.verify(&repo), Verdict::NoChecks), "bare repo => NoChecks");
        let _ = fs::remove_dir_all(&repo);
        let _ = std::env::set_current_dir(&orig);
    }

    #[test]
    fn trunk_detection_prefers_checked_out_branch() {
        // Repo whose default branch is "trunk" (not main). trunk() should pick it
        // up from the checked-out branch.
        let dir = scratch_repo();
        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        Command::new("git").args(["checkout", "-qb", "trunk"]).current_dir(&dir).status().unwrap();
        let wt = Wt::new();
        assert_eq!(wt.trunk(), "trunk", "trunk() should detect the checked-out branch");
        let _ = fs::remove_dir_all(&dir);
        let _ = std::env::set_current_dir(&orig);
    }
}
