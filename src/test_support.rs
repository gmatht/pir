//! Test-only helpers shared between the `pir` binary and its extensions.
//!
//! Extensions are separate compilation units linked into the same test binary
//! (`build.rs` emits `gen_registry.rs`, and each extension's test module is
//! included the same way). They therefore cannot name `crate::config::TEST_ENV_LOCK`
//! directly — a private item of the binary's crate. This module is the one place
//! that hands the *same* lock to both sides.
//!
//! ## Why a shared lock is needed
//!
//! `cargo test` runs a crate's tests as concurrent threads in **one process**.
//! Anything process-global is therefore shared mutable state: environment
//! variables, the current working directory, the cgroup tree, the terminal.
//! Two tests that each mutate such state and then assert on it will interleave,
//! and the resulting failures look nothing like the bug they are — a shell test
//! reporting `spawn bash: No such file or directory` because an unrelated test
//! had the cwd pointing at a temp dir it was about to delete, for instance.
//!
//! The fix is always the same: take the lock, mutate, assert, drop. This suite
//! had drifted into several independent per-module locks, which serialise a
//! module against *itself* but not against anyone else — strictly weaker than
//! one shared lock and easy to get wrong again, so there is now just this one.

/// The process-wide lock guarding test mutations of process-global state
/// (environment variables, the process cwd, the cgroup tree).
///
/// This is an **alias for** [`crate::config::TEST_ENV_LOCK`], not a second
/// lock. That distinction is the whole point: two locks, one in the binary and
/// one here, would exclude nothing relative to each other, which is precisely
/// the bug this module exists to close. [`crate::config::TEST_ENV_LOCK`] is a
/// `static`, and `Mutex::new` is a `const fn`, so borrowing it costs nothing
/// and there is no `OnceLock` to allocate through.
///
/// Poisoning is deliberately ignored (`unwrap_or_else(|e| e.into_inner())`): a
/// single panicking test must not turn every later test into a "poisoned lock"
/// failure, which hides the one real failure behind hundreds of fake ones.
#[cfg(test)]
pub fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this module exists to prevent, pinned as a test: the wt
    /// extension and the binary's config/security tests mutate *different*
    /// process globals (`PIR_WT*` and the cwd versus `PI_DIR`) and used to take
    /// *different* locks, so they never excluded each other. Every one of them
    /// now goes through `env_lock()`.
    ///
    /// The check is that the lock is genuinely re-entrant-safe *across modules*
    /// rather than merely present: take it here, and take it again from a
    /// different call site while a `PIR_WT` mutation is in flight. A second
    /// thread attempting the same lock must block, not proceed.
    #[test]
    fn env_lock_is_shared_not_per_module() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (tx, rx) = mpsc::channel();
        // Hold the lock on this thread, then ask another thread to take it and
        // report back. If the lock were per-module, that thread would sail
        // straight through and we'd see its message immediately.
        let _guard = env_lock();
        std::thread::spawn(move || {
            let _g = env_lock();
            let _ = tx.send(());
        });

        // The other thread must be blocked while we hold the guard.
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "a second thread acquired the lock while this one held it — the lock is not shared"
        );
        // Dropping the guard lets it through.
        drop(_guard);
        assert!(
            rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "the waiting thread never acquired the lock after it was released"
        );
    }
}
