//! Privilege-escalation contract.
//!
//! The single audited boundary where any *granted* host-root escalation is
//! exercised. In the security model, the agent never holds host root: a
//! `BecomeRoot` op is always denied by the policy layer, and the only path to
//! elevated writes is the human-gated `ai-apt-install` sudoers wrapper (see
//! `docs/SECURITY_MODEL.md` §8.3). This module centralises the reaping of
//! host-root-owned files back to the project owner, so an extension that is
//! ever granted a narrow write cannot leave the tree owned by `root`.
//!
//! On a non-root per-project `ai_*` setup (the default), the whole process
//! already runs unprivileged, so these helpers are no-ops. They exist so the
//! contract is explicit and the reaping happens in exactly one place.

use std::path::Path;

/// Reap a path (and nothing outside it) back to `owner_uid:owner_gid` if the
/// current effective uid is 0. Best-effort: any failure is logged, never fatal.
/// Used after a tool that was granted a narrow write, so files never end up
/// host-root-owned and unreachable by the project's `ai_*` identity.
#[cfg(unix)]
pub fn reap_to_owner_if_root(path: &Path, owner_uid: u32, owner_gid: u32) {
    use std::os::unix::fs::chown;
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        return;
    }
    // Only chown files we can see; never recurse into the repo's `.git` internals
    // or traverse symlinks outside `path`. Conservative: chown the single path.
    if let Err(e) = chown(path, Some(owner_uid), Some(owner_gid)) {
        eprintln!(
            "[pir] privilege: could not reap {} to {}/{}: {}",
            path.display(),
            owner_uid,
            owner_gid,
            e
        );
    }
}

/// Non-unix: nothing to reap (no host-root concept here).
#[cfg(not(unix))]
pub fn reap_to_owner_if_root(_path: &Path, _owner_uid: u32, _owner_gid: u32) {}

/// True when the process is currently running as host root (euid 0). Used by
/// the launcher to decide whether the `ai-apt-install` sudoers path is even
/// reachable, and to decide whether reaping is necessary after a granted write.
#[cfg(unix)]
pub fn is_effective_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
pub fn is_effective_root() -> bool {
    false
}
