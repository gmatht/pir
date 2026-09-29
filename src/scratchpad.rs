//! Per-session scratch directory.
//!
//! Every session gets a private, writable working area separate from the
//! project tree — a place for the agent to dump intermediate files, generated
//! fixtures, build logs, PDFs it extracts, sub-agent reports, plan drafts and
//! anything else that must not pollute the repo (and must not be committed).
//!
//! Layout: `<PIR_SCRATCHPAD_DIR or $TMPDIR/pir>/<session-key>/` where
//! `session-key` is the session transcript's file stem (`pir-20260924-1200-sh123`)
//! or, for one-shot runs with no transcript, `pid-<pid>`. The directory is
//! created at session start and exported to every child process (bash tools,
//! `!`/`/sh`, sub-agents) through `$PIR_SCRATCHPAD`, so an agent can shell out
//! to `$PIR_SCRATCHPAD/out.json` without knowing the path.
//!
//! Cleanup is deliberately *lazy*: nothing is removed while the session runs.
//! On startup, [`sweep_old`] deletes scratch dirs whose last write is older than
//! [`TTL_DAYS`] days, so a crashed session's leftovers don't accumulate
//! forever — but a recent one survives, because the operator may still want to
//! look at what the agent produced. Set `PIR_SCRATCHPAD_KEEP=1` to disable the
//! sweep entirely, or `PIR_SCRATCHPAD_TTL_DAYS=N` to change the window.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Default lifetime of an abandoned scratch directory.
pub const TTL_DAYS: u64 = 30;

/// Environment variable every child process receives with the scratch path.
pub const ENV_VAR: &str = "PIR_SCRATCHPAD";

/// Root under which per-session scratch dirs live.
///
/// `PIR_SCRATCHPAD_DIR` overrides everything (tests and CI use it to avoid
/// touching the real `/tmp`); otherwise the OS temp dir is used, *not* the
/// project tree — a scratch dir inside the repo would show up in `git status`
/// and in every `glob`/`grep` the agent runs.
pub fn root() -> PathBuf {
    if let Some(d) = std::env::var_os("PIR_SCRATCHPAD_DIR") {
        let p = PathBuf::from(d);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    std::env::temp_dir().join("pir").join("scratch")
}

/// Derive a filesystem-safe key for a session. Uses the transcript stem when
/// there is one so the scratch dir is trivially correlatable with
/// `~/.pi/agent/sessions/<same-name>.jsonl`; falls back to the pid for
/// transcript-less one-shot runs.
pub fn key_for(log_path: Option<&Path>) -> String {
    let raw = log_path
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("pid-{}", std::process::id()));
    sanitize(&raw)
}

/// Keep only characters that are safe in a path component on every platform.
fn sanitize(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect();
    if out.is_empty() {
        out = format!("pid-{}", std::process::id());
    }
    out
}

/// Create (and return) the scratch directory for `log_path`, exporting it as
/// `$PIR_SCRATCHPAD`. Idempotent: a resumed session reuses the directory it had
/// (its previous contents are still there, which is exactly what makes a
/// resumed session able to pick up a half-finished job).
///
/// Returns `None` — never an error — when the directory can't be created; a
/// missing scratch dir must degrade to "no scratchpad" rather than aborting the
/// session.
pub fn init(log_path: Option<&Path>) -> Option<PathBuf> {
    let dir = root().join(key_for(log_path));
    if std::fs::create_dir_all(&dir).is_err() {
        // One retry: the root itself may not exist yet (create_dir_all handles
        // that, so this only fires on a genuine permission problem).
        if std::fs::create_dir_all(root().join(key_for(log_path))).is_err() {
            return None;
        }
    }
    // SAFETY: edition 2024 marks env mutation unsafe; pir confines env writes
    // to startup config and explicit session toggles (this is startup).
    unsafe { std::env::set_var(ENV_VAR, &dir); }
    Some(dir)
}

/// The current session's scratch directory, if one was initialised (or if the
/// env var is set by a parent).
pub fn current() -> Option<PathBuf> {
    std::env::var_os(ENV_VAR).map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
}

/// A one-line description used by `/scratchpad` and the startup banner.
pub fn describe() -> String {
    match current() {
        Some(p) => {
            let (files, bytes) = usage(&p);
            format!(
                "scratchpad {} ({} file{}, {}; $PIR_SCRATCHPAD)",
                p.display(),
                files,
                if files == 1 { "" } else { "s" },
                human_bytes(bytes)
            )
        }
        None => "no scratchpad for this session (set PIR_SCRATCHPAD_DIR or check /tmp permissions)".to_string(),
    }
}

/// Count files (recursively) and total bytes under `dir`.
pub fn usage(dir: &Path) -> (usize, u64) {
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                files += 1;
                bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    (files, bytes)
}

/// Delete scratch directories (and scratch *files*) older than the TTL.
///
/// **Do not call this on the startup path** — use [`spawn_sweep`], which does
/// the same work on a background thread. [`is_stale`] takes the max mtime over
/// the whole tree of every candidate (a directory's own mtime only changes when
/// its top-level entries change, so a dir the agent kept writing into would
/// otherwise look ancient), which means a sweep is O(every file in every
/// scratchpad ever created). With a few thousand stale session dirs that is
/// seconds of blocking `stat` calls *before the REPL paints* — measured at 5.2s
/// on a host with 1633 scratch dirs, where the resume itself took 108ms. The
/// sweep is pure housekeeping that nothing in the resume path depends on, so it
/// has no business sitting in front of the first prompt.
///
/// Best-effort: a directory that can't be removed (still in use by a live
/// session, permission denied) is left alone and the sweep continues — this
/// must never fail a session.
pub fn sweep_old() -> usize {
    if std::env::var_os("PIR_SCRATCHPAD_KEEP").is_some() {
        return 0;
    }
    let ttl_days = std::env::var("PIR_SCRATCHPAD_TTL_DAYS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&d| d > 0)
        .unwrap_or(TTL_DAYS);
    let cutoff = Duration::from_secs(ttl_days * 24 * 3600);
    let root = root();
    let Ok(rd) = std::fs::read_dir(&root) else { return 0 };
    let mut removed = 0usize;
    for e in rd.flatten() {
        let p = e.path();
        // Never sweep the *current* session's directory, even if something
        // upstream gave it a stale mtime.
        if current().as_deref() == Some(p.as_path()) {
            continue;
        }
        if !is_stale(&p, cutoff) {
            continue;
        }
        let ok = if p.is_dir() { std::fs::remove_dir_all(&p).is_ok() } else { std::fs::remove_file(&p).is_ok() };
        if ok {
            removed += 1;
        }
    }
    // Also tidy the legacy flat files directly under the root.
    if let Ok(rd) = std::fs::read_dir(&root) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                continue;
            }
            if is_stale(&p, cutoff) && std::fs::remove_file(&p).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// Start [`sweep_old`] on a background thread, so startup never waits for it.
///
/// Returns the [`JoinHandle`]. Production **drops it** (detached) — dropping a
/// handle does not cancel the thread, so the work still completes, and nothing
/// in resume needs its result. It is returned so a *test* can join it: the
/// scratch root comes from process-global env, so an unjoined thread could
/// still be reading `PIR_SCRATCHPAD_DIR` after a test restored it and then
/// sweep a sibling test's directory.
///
/// Deliberately *not* spawned through the smol reactor: the sweep is plain
/// blocking filesystem I/O, and putting it on that single worker would let it
/// occupy the executor while the REPL also wants it.
pub fn spawn_sweep() -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("scratch-sweep".into())
        .spawn(|| {
            let removed = sweep_old();
            if removed > 0 {
                eprintln!("[pir] scratchpad sweep: removed {removed} stale item(s)");
            }
        })
        // A thread that cannot be spawned must not fail startup: the sweep is
        // housekeeping, and `sweep_old` is still callable directly.
        .unwrap_or_else(|_| std::thread::spawn(|| {}))
}

/// Whether `p` (or the newest file inside it) is older than `cutoff`. A
/// directory's own mtime only changes when entries are added/removed at its top
/// level, so we take the max mtime over the tree — otherwise a scratch dir that
/// the agent kept *writing into* would look ancient.
fn is_stale(p: &Path, cutoff: Duration) -> bool {
    let newest = newest_mtime(p);
    match SystemTime::now().duration_since(newest) {
        Ok(age) => age > cutoff,
        // A future mtime (clock skew) is treated as fresh, never deleted.
        Err(_) => false,
    }
}

fn newest_mtime(p: &Path) -> SystemTime {
    let mut newest = std::fs::metadata(p).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
    if p.is_dir() {
        let mut stack = vec![p.to_path_buf()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                if let Ok(md) = e.metadata() {
                    if md.is_dir() {
                        stack.push(e.path());
                    }
                    if let Ok(t) = md.modified()
                        && t > newest
                    {
                        newest = t;
                    }
                }
            }
        }
    }
    newest
}

fn human_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else if n < 1024 * 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", n as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All scratch tests run under a throwaway root so the real `/tmp/pir` is
    /// never touched, and restore `PIR_SCRATCHPAD_DIR` afterwards.
    ///
    /// Serialized: `PIR_SCRATCHPAD_DIR`/`$PIR_SCRATCHPAD` are process-global, so
    /// two tests running concurrently would clobber each other's root (observed
    /// as `sweep` seeing `init`'s directory and vice versa). Every helper takes
    /// the same lock the config tests use.
    fn with_root<T>(tag: &str, f: impl FnOnce(&Path) -> T) -> T {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("pir_scratch_test_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let old_dir = std::env::var_os("PIR_SCRATCHPAD_DIR");
        let old_env = std::env::var_os(ENV_VAR);
        // SAFETY: test-local env mutation, serialized by the module's tests
        // running in one thread under `--test-threads=1`-style discipline; the
        // values are restored below.
        unsafe { std::env::set_var("PIR_SCRATCHPAD_DIR", &root); }
        let out = f(&root);
        match old_dir {
            Some(v) => unsafe { std::env::set_var("PIR_SCRATCHPAD_DIR", v) },
            None => unsafe { std::env::remove_var("PIR_SCRATCHPAD_DIR") },
        }
        match old_env {
            Some(v) => unsafe { std::env::set_var(ENV_VAR, v) },
            None => unsafe { std::env::remove_var(ENV_VAR) },
        }
        let _ = std::fs::remove_dir_all(&root);
        out
    }

    #[test]
    fn key_is_derived_from_session_stem_and_sanitized() {
        assert_eq!(key_for(Some(Path::new("/a/b/pir-20260101-1200-sh9.jsonl"))), "pir-20260101-1200-sh9");
        assert_eq!(key_for(Some(Path::new("/a/b/we ird/na me.jsonl"))), "na_me");
        // No transcript -> a pid-scoped key, never empty.
        let k = key_for(None);
        assert!(k.starts_with("pid-"), "got {k}");
    }

    #[test]
    fn init_creates_exports_and_is_idempotent() {
        with_root("init", |root| {
            let log = Path::new("/x/pir-20260101-1200-sh1.jsonl");
            let dir = init(Some(log)).expect("scratch dir");
            assert!(dir.is_dir(), "scratch dir must exist");
            assert_eq!(current().as_deref(), Some(dir.as_path()));
            assert_eq!(dir, root.join("pir-20260101-1200-sh1"));
            // A file written there survives a re-init (resume semantics).
            std::fs::write(dir.join("note.txt"), "hello").unwrap();
            let again = init(Some(log)).expect("re-init");
            assert_eq!(again, dir);
            assert_eq!(std::fs::read_to_string(dir.join("note.txt")).unwrap(), "hello");
        });
    }

    #[test]
    fn usage_counts_files_and_bytes() {
        with_root("usage", |_root| {
            let dir = init(Some(Path::new("/x/pir-usage.jsonl"))).unwrap();
            assert_eq!(usage(&dir), (0, 0));
            std::fs::write(dir.join("a.txt"), "12345").unwrap();
            std::fs::create_dir_all(dir.join("sub")).unwrap();
            std::fs::write(dir.join("sub/b.txt"), "123").unwrap();
            assert_eq!(usage(&dir), (2, 8));
            assert!(describe().contains("2 files"), "describe: {}", describe());
        });
    }

    #[test]
    fn sweep_removes_stale_and_keeps_fresh() {
        with_root("sweep", |root| {
            // A stale dir: mtime forced 40 days into the past.
            let stale = root.join("old-session");
            std::fs::create_dir_all(&stale).unwrap();
            std::fs::write(stale.join("f"), "x").unwrap();
            let old = SystemTime::now() - Duration::from_secs(40 * 24 * 3600);
            set_mtime(&stale.join("f"), old);
            set_mtime(&stale, old);
            // A fresh dir.
            let fresh = root.join("new-session");
            std::fs::create_dir_all(&fresh).unwrap();
            std::fs::write(fresh.join("f"), "x").unwrap();

            let removed = sweep_old();
            assert_eq!(removed, 1, "exactly the stale dir must go");
            assert!(!stale.exists(), "stale scratch dir must be removed");
            assert!(fresh.exists(), "fresh scratch dir must survive");
        });
    }

    #[test]
    fn sweep_keeps_the_current_sessions_dir_even_if_stale() {
        with_root("keepcur", |_root| {
            let log = Path::new("/x/pir-current.jsonl");
            let dir = init(Some(log)).unwrap();
            std::fs::write(dir.join("f"), "x").unwrap();
            let old = SystemTime::now() - Duration::from_secs(400 * 24 * 3600);
            set_mtime(&dir.join("f"), old);
            set_mtime(&dir, old);
            let _ = sweep_old();
            assert!(dir.exists(), "the live session's scratch must never be swept");
        });
    }

    #[test]
    fn keep_env_disables_the_sweep() {
        with_root("keepenv", |root| {
            let stale = root.join("old");
            std::fs::create_dir_all(&stale).unwrap();
            let old = SystemTime::now() - Duration::from_secs(400 * 24 * 3600);
            set_mtime(&stale, old);
            unsafe { std::env::set_var("PIR_SCRATCHPAD_KEEP", "1"); }
            let removed = sweep_old();
            unsafe { std::env::remove_var("PIR_SCRATCHPAD_KEEP"); }
            assert_eq!(removed, 0);
            assert!(stale.exists());
        });
    }

    /// The sweep must not sit in front of the first prompt. It walks the newest
    /// mtime of every leftover session's whole tree, so on a host with a few
    /// thousand scratch dirs it blocked startup for seconds (5.2s measured with
    /// 1633 dirs, against 108ms to parse a 2.2MB transcript). Assert the
    /// contract: `spawn_sweep` returns promptly with the work unfinished, and
    /// that work still completes once joined.
    #[test]
    fn the_sweep_runs_off_the_calling_thread_and_still_completes() {
        with_root("async", |root| {
            // Enough real work that a synchronous sweep *cannot* return inside
            // the budget: 400 dirs × 40 files, each needing a `stat` (the sweep
            // takes the newest mtime across the whole tree). An earlier version
            // used 64 one-file dirs, which completed in ~80ms and therefore
            // "passed" even with the sweep restored to the startup path — the
            // assertion was vacuous.
            for i in 0..400 {
                let d = root.join(format!("stale-{i}"));
                std::fs::create_dir_all(&d).unwrap();
                for f in 0..40 {
                    std::fs::write(d.join(format!("f{f}")), "x").unwrap();
                }
                let old = SystemTime::now() - Duration::from_secs(400 * 24 * 3600);
                for f in 0..40 {
                    set_mtime(&d.join(format!("f{f}")), old);
                }
                set_mtime(&d, old);
            }

            let start = std::time::Instant::now();
            let handle = spawn_sweep();
            // Returning inside the budget is the assertion: a synchronous sweep
            // does the whole walk before returning from this call.
            let spawn_elapsed = start.elapsed();
            assert!(
                spawn_elapsed < Duration::from_millis(250),
                "spawn_sweep must return immediately, took {spawn_elapsed:?} — \
                 the sweep is back on the startup path"
            );

            // The work is genuinely asynchronous, and genuinely still done.
            handle.join().expect("sweep thread must not panic");
            let left = std::fs::read_dir(root).unwrap().flatten().count();
            assert_eq!(left, 0, "the background sweep must still remove every stale dir");
        });
    }

    /// Force a path's mtime, so the TTL logic can be tested without waiting 30
    /// days. Unix-only (`utimensat` via libc); other platforms skip the test.
    #[cfg(unix)]
    fn set_mtime(p: &Path, t: SystemTime) {
        use std::os::unix::ffi::OsStrExt;
        let secs = t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let times = [
            libc::timespec { tv_sec: secs, tv_nsec: 0 },
            libc::timespec { tv_sec: secs, tv_nsec: 0 },
        ];
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        unsafe {
            libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0);
        }
    }
    #[cfg(not(unix))]
    fn set_mtime(_p: &Path, _t: SystemTime) {}
}
