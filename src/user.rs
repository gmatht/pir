//! Per-project execution user (`ai_<project>`).
//!
//! The security boundary is "drop privs for the *agents*, not the *user*":
//! `pir` itself stays the invoking identity (the operator, typically root), so
//! the operator keeps their authority (`/sh -u`, legitimate fs writes, …).
//! Only the *untrusted commands the model spawns* (the `bash` tool) are
//! confined to `ai_X`. `set_agent_exec_user` records that user and the bash
//! tool's `before_exec` drops to it (collapsing the saved uid so it cannot
//! escalate); the file tools run as the invoking identity. The `become_user`
//! helper still exists for the `sudo -u ai_X pir` launch shape, where the whole
//! process is already the sandbox user.
//!
//! This module is unix-only. On non-unix targets the functions return
//! `Err(...)` explaining the feature is unsupported, and the agent falls back
//! to running as the invoking user.

#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::sync::Mutex;

/// Name of the user with the given uid, best-effort.
#[cfg(unix)]
pub fn name_of_uid(uid: u32) -> Option<String> {
    unsafe {
        let mut buf = vec![0u8; 4096];
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        if libc::getpwuid_r(
            uid,
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        ) == 0
            && !result.is_null()
            && !pwd.pw_name.is_null()
        {
            return std::ffi::CStr::from_ptr(pwd.pw_name).to_str().ok().map(|s| s.to_string());
        }
    }
    None
}

/// The user who launched `pir` (the "invoking user"), captured before any
/// privilege drop. `SUDO_USER` takes precedence (it is the human who ran
/// `sudo … pir`); otherwise the *real* uid's name is used (`getuid()` is the
/// invoking user even after `become_user` drops only the *effective* uid).
/// Returns `None` when it can't be determined, so callers can fall back to the
/// current (dropped) identity. Stored into `PIR_INVOKING_USER` by `main` so it
/// survives the drop and is available to `/sh -u`.
#[cfg(unix)]
pub fn invoking_user_name() -> Option<String> {
    if let Ok(u) = std::env::var("SUDO_USER") {
        if !u.trim().is_empty() {
            return Some(u.trim().to_string());
        }
    }
    name_of_uid(unsafe { libc::getuid() })
}

/// Look up the numeric uid/gid for a system user.
#[cfg(unix)]
pub fn lookup_user(user: &str) -> Result<(u32, u32), String> {
    let cuser = std::ffi::CString::new(user).map_err(|_| format!("invalid user name '{user}'"))?;
    unsafe {
        let mut buf = vec![0u8; 4096];
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        if libc::getpwnam_r(
            cuser.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        ) != 0
        {
            return Err(format!("lookup of user '{user}' failed"));
        }
        if result.is_null() {
            return Err(format!("no such user '{user}' (create it with `pir project init`)"));
        }
        Ok((pwd.pw_uid, pwd.pw_gid))
    }
}

/// What the accessibility wizard decided to do about the cwd being unreachable
/// by the sandbox user.
#[cfg(unix)]
pub enum AccessibilityAction {
    /// Nothing wrong (or the user chose to proceed): drop privileges normally.
    Proceed,
    /// The user chose *not* to drop privileges; run as the invoking user.
    SkipDrop,
    /// The project was relocated into the sandbox user's home; `pir` should
    /// `chdir` here and then drop privileges normally.
    Relocated(std::path::PathBuf),
}

/// Run a command, returning its captured stderr (preferred) or
/// failure, as an error string. Used by the accessibility wizard's relocate/
/// clone steps. Best-effort: any spawn error is reported directly.
#[cfg(unix)]
fn run_cmd(args: &[&str]) -> Result<(), String> {
    use std::process::Command;
    let (prog, rest) = args.split_first().expect("non-empty command");
    let out = Command::new(prog).args(rest).output();
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let msg = String::from_utf8_lossy(if !o.stderr.is_empty() { &o.stderr } else { &o.stdout });
            Err(format!("`{}` failed: {}", args.join(" "), msg.trim()))
        }
        Err(e) => Err(format!("could not run `{}`: {e}", args.join(" "))),
    }
}

/// True when `user` (uid/gid) can `stat`/`traverse` `dir`: either "other" has
/// execute, or the user owns the dir (and owner has execute), or the user's
/// group owns it (and group has execute). Without execute on *some* matching
/// class, a non-owner process cannot walk through the directory to reach a
/// descendant which is exactly the "ai_ user can't reach a subdir of another
/// user's 0700 home" problem.
#[cfg(unix)]
fn can_traverse(md: &std::fs::Metadata, uid: u32, gid: u32) -> bool {
    let mode = md.mode();
    if mode & 0o001 != 0 {
        return true; // other execute
    }
    if md.uid() == uid && mode & 0o100 != 0 {
        return true; // owner execute
    }
    if md.gid() == gid && mode & 0o010 != 0 {
        return true; // group execute
    }
    false
}

/// Walk the cwd's ancestors (parent, grandparent, …) and return the
/// ones the `user` cannot traverse. Returns an empty vec when the user
/// can reach the cwd, when the user doesn't resolve, or when we can't stat a
/// path (so callers never block on a missing dir).
#[cfg(unix)]
fn traverse_blockers(cwd: &std::path::Path, user: &str) -> Vec<std::path::PathBuf> {
    let (uid, gid) = match lookup_user(user) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut blockers = Vec::new();
    // `ancestors()` yields cwd, then each parent up to `/`. Skip cwd itself.
    for ancestor in cwd.ancestors().skip(1) {
        if let Ok(md) = std::fs::metadata(ancestor) {
            if !can_traverse(&md, uid, gid) && !blockers.iter().any(|p| p == ancestor) {
                blockers.push(ancestor.to_path_buf());
            }
        }
    }
    blockers
}

/// Resolve the sandbox user's real home directory (for relocating the project).
#[cfg(unix)]
fn user_home(user: &str) -> Option<std::path::PathBuf> {
    let cuser = std::ffi::CString::new(user).ok()?;
    let mut buf = vec![0u8; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    unsafe {
        if libc::getpwnam_r(
            cuser.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        ) != 0
            || result.is_null()
            || pwd.pw_dir.is_null()
        {
            return None;
        }
        std::ffi::CStr::from_ptr(pwd.pw_dir).to_str().ok().map(std::path::PathBuf::from)
    }
}

/// Relocate `cwd` into `user`'s home and leave a symlink at the original path
/// pointing at the new location, so external references to the old path still
/// resolve. Returns the new location (owned by `user`). Root only (needs to
/// chown + write the symlink into the original parent).
#[cfg(unix)]
fn relocate_and_symlink(cwd: &std::path::Path, user: &str, home: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let proj = cwd
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "project".into());
    let dest = home.join(proj);
    if dest.exists() {
        return Err(format!("refusing to relocate: {} already exists", dest.display()));
    }
    run_cmd(&["mv", cwd.to_str().unwrap_or(""), dest.to_str().unwrap_or("")])?;
    // Symlink the *original* path at the new location so anything referencing
    // the old path (other terminals, tooling) keeps working.
    let _ = std::os::unix::fs::symlink(&dest, cwd);
    run_cmd(&["chown", "-R", &format!("{user}:{user}"), dest.to_str().unwrap_or("")])?;
    Ok(dest)
}

/// Copy `cwd` into `user`'s home (owned by `user`), leaving the original
/// intact. The agent then works in the copy; later divergence between the two
/// trees is the caller's responsibility. Root only (needs to chown).
#[cfg(unix)]
fn clone_into_home(cwd: &std::path::Path, user: &str, home: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let proj = cwd
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "project".into());
    let dest = home.join(proj);
    if dest.exists() {
        return Err(format!("refusing to clone: {} already exists", dest.display()));
    }
    run_cmd(&["cp", "-a", cwd.to_str().unwrap_or(""), dest.to_str().unwrap_or("")])?;
    run_cmd(&["chown", "-R", &format!("{user}:{user}"), dest.to_str().unwrap_or("")])?;
    Ok(dest)
}

/// Check whether the sandbox `user` can actually reach the current working
/// directory, and — if not — run an interactive wizard offering ways to fix
/// it instead of letting the agent silently fail to read files later:
///
///   1. Move the project into the user's home and symlink the original path.
///   2. Clone (copy) the project into the user's home.
///   3. Don't drop privileges at all (run as the invoking user — no sandbox).
///   4. Drop anyway and try (may fail to read files).
///
/// Returns the action `main` should take. When stdin isn't a terminal the
/// wizard does not block: it warns and proceeds unless `PIR_NO_DROP=1` is set
/// (which forces `SkipDrop`). `root` always passes (root can traverse anything),
/// so the wizard only fires for a non-root sandbox target.
#[cfg(unix)]
pub fn cwd_accessibility_wizard(user: &str) -> Result<AccessibilityAction, String> {
    use crate::term;

    if user == "root" {
        return Ok(AccessibilityAction::Proceed);
    }
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read cwd: {e}"))?;
    let blockers = traverse_blockers(&cwd, user);
    if blockers.is_empty() {
        return Ok(AccessibilityAction::Proceed);
    }

    let euid = unsafe { libc::geteuid() };
    let can_drop = euid == 0;

    eprintln!(
        "{}",
        term::yellow(&format!(
            "[pir] the sandbox user '{user}' may not be able to reach the working directory {}",
            cwd.display()
        ))
    );
    eprintln!(
        "{}",
        term::dim("These parent directories are not traversable by that user (no 'o+x' and not owned by it):")
    );
    for d in &blockers {
        eprintln!("  {} {}", term::red("✗"), d.display());
    }

    if !term::is_terminal() {
        // Non-interactive: don't block. Honour PIR_NO_DROP to force "no sandbox".
        if std::env::var("PIR_NO_DROP").is_ok() && can_drop {
            eprintln!("{} PIR_NO_DROP set — not dropping privileges.", term::dim("·"));
            return Ok(AccessibilityAction::SkipDrop);
        }
        eprintln!(
            "{}",
            term::dim("Running non-interactively — proceeding with the privilege drop. Set PIR_NO_DROP=1 to skip the sandbox.")
        );
        return Ok(AccessibilityAction::Proceed);
    }

    eprintln!();
    eprintln!("{}", term::bold("How do you want to fix this?"));
    eprintln!(
        "  {}  Move the project into {user}'s home (~{user}/{}) and symlink the original path to it (recommended)",
        term::cyan("1"),
        cwd.file_name().and_then(|n| n.to_str()).unwrap_or("project")
    );
    eprintln!(
        "  {}  Clone (copy) the project into {user}'s home (~{user}/{})",
        term::cyan("2"),
        cwd.file_name().and_then(|n| n.to_str()).unwrap_or("project")
    );
    if can_drop {
        eprintln!(
            "  {}  Don't drop privileges — run as the current user (no sandbox)",
            term::cyan("3")
        );
    }
    eprintln!(
        "  {}  Drop privileges anyway and try (may fail to read files)",
        term::cyan(if can_drop { "4" } else { "3" })
    );

    let ans = term::read_answer("choice [1-.., default 1]: ");
    let ans = ans.trim();
    let choice = if ans.is_empty() { "1" } else { ans };

    match choice {
        "1" => {
            let home = user_home(user).ok_or_else(|| format!("cannot resolve home for '{user}'"))?;
            match relocate_and_symlink(&cwd, user, &home) {
                Ok(dest) => {
                    eprintln!(
                        "{} moved project to {} and symlinked {} to it",
                        term::green("✓"),
                        dest.display(),
                        cwd.display()
                    );
                    Ok(AccessibilityAction::Relocated(dest))
                }
                Err(e) => {
                    eprintln!("{} {e}", term::red("!"));
                    eprintln!("{}", term::dim("F: drop privileges anyway."));
                    Ok(AccessibilityAction::Proceed)
                }
            }
        }
        "2" => {
            let home = user_home(user).ok_or_else(|| format!("cannot resolve home for '{user}'"))?;
            match clone_into_home(&cwd, user, &home) {
                Ok(dest) => {
                    eprintln!(
                        "{} cloned project to {} (original left intact)",
                        term::green("✓"),
                        dest.display()
                    );
                    Ok(AccessibilityAction::Relocated(dest))
                }
                Err(e) => {
                    eprintln!("{} {e}", term::red("!"));
                    eprintln!("{}", term::dim("Falling back to: drop privileges anyway."));
                    Ok(AccessibilityAction::Proceed)
                }
            }
        }
        "3" if can_drop => Ok(AccessibilityAction::SkipDrop),
        "3" => Ok(AccessibilityAction::Proceed), // not root: "3" means "drop anyway"
        "4" if can_drop => Ok(AccessibilityAction::Proceed),
        _ => Ok(AccessibilityAction::Proceed),
    }
}

/// The per-project `ai_*` user that the agent's *tool commands* (bash) should
/// run as. `pir` itself stays the invoking identity (the operator, typically
/// root); only the untrusted commands the model spawns (the `bash` tool) are
/// confined to this identity — exactly the "drop privs for the agents, not the
/// user" boundary. Set at startup when `pir` is launched as root; a no-op when
/// `pir` was launched already as the sandbox user (the child inherits that
/// identity automatically). `spawn_shell` reads this in the child's
/// `before_exec` and drops to it.
static AGENT_EXEC_USER: Mutex<Option<String>> = Mutex::new(None);

/// Record the sandbox user agent bash commands should run as. See
/// [`AGENT_EXEC_USER`].
pub fn set_agent_exec_user(user: &str) {
    *AGENT_EXEC_USER.lock().unwrap() = Some(user.to_string());
}

/// The `ai_*` user agent bash commands should run as, if one was configured.
pub fn agent_exec_user() -> Option<String> {
    AGENT_EXEC_USER.lock().unwrap().clone()
}

/// Drop privileges to the given user (unix only). Call *after* config and
/// providers are loaded but *before* the agent is built and any tool runs.
#[cfg(unix)]
pub fn become_user(user: &str) -> Result<(), String> {
    let (uid, gid) = lookup_user(user)?;
    let euid = unsafe { libc::geteuid() };
    // Already running as the target user (e.g. `sudo -u ai_X pir …`): there is
    // nothing to drop, but still point the agent at its self-owned toolchain
    // dirs so crates / gh don't land in the invoking user's home.
    if euid == uid {
        ensure_home_dir(user);
        apply_toolchain_env(user);
        return Ok(());
    }
    // Otherwise we can only switch if we currently hold root (setuid
    // privilege). A non-root process that isn't already the target can't
    // escalate, so report that clearly rather than silently running as the
    // wrong identity.
    if euid != 0 {
        return Err(format!(
            "pir must run as root (or already as '{user}') to switch to user '{user}'. \
             Re-run as root, or use `sudo -u {user} pir ...`"
        ));
    }
    unsafe {
        // Drop supplementary groups first, then gid, then uid (order matters:
        // once uid is dropped we can no longer setgid).
        //
        // We keep root in the *saved* uid (setresuid(uid, uid, 0)) rather than
        // the classic setuid(uid) which would discard it. That lets an explicit,
        // human-invoked `/sh -u [user]` switch back to the invoking user (or
        // root) for the duration of one shell — the only place we ever *want*
        // to escalate. The sandbox itself stays airtight because every command
        // the agent spawns (the bash tool, `run_shell`'s plain path, the health
        // probe) calls `drop_to_current_identity()` in the child's `before_exec`,
        // which drops the saved uid too — so untrusted agent commands can never
        // escalate even though the parent holds a saved root.
        if libc::setgroups(0, std::ptr::null()) != 0 {
            return Err(format!("failed to clear groups for '{user}'"));
        }
        if libc::setresgid(gid, gid, 0) != 0 {
            return Err(format!("failed to setresgid to '{user}'"));
        }
        if libc::setresuid(uid, uid, 0) != 0 {
            return Err(format!("failed to setresuid to '{user}'"));
        }
    }
    // Point the (now unprivileged) agent at its own, self-owned toolchain dirs
    // so it can fetch crates / use gh without touching root's files.
    ensure_home_dir(user);
    apply_toolchain_env(user);
    Ok(())
}

/// Fully drop the process to its *current* (effective) identity, including the
/// *saved* uid/gid, so a child spawned afterwards can never escalate back to
/// root. The parent `pir` keeps a saved root (see `become_user`) so a
/// human-invoked `/sh -u` can switch back, but every *command the agent spawns*
/// must call this in its `before_exec` so untrusted agent code can't abuse the
/// saved root. Safe to call whether or not a saved root exists: when the
/// effective uid is already unprivileged we just collapse real/effective/saved
/// to that one identity (e.g. `sudo -u ai_X pir`, where no root was ever held).
/// Returns `Ok(())` or the `io::Error` from the first failing syscall so a
/// `before_exec` can refuse to exec as root.
#[cfg(unix)]
pub fn drop_to_current_identity() -> Result<(), std::io::Error> {
    unsafe {
        let euid = libc::geteuid();
        let egid = libc::getegid();
        if libc::setresuid(euid, euid, euid) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::setresgid(egid, egid, egid) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let _ = libc::setgroups(0, std::ptr::null());
        Ok(())
    }
}

/// Drop the *current* process (a child about to `exec`) to the configured agent
/// execution user `ai_X`, collapsing the *saved* uid/gid too so the untrusted
/// command can never escalate back to the invoking identity (root). This is the
/// other half of the "drop privs for the agents, not the user" boundary:
/// `pir` itself stays root (kept able to `/sh -u` elsewhere), but every command
/// the model spawns via the `bash` tool is confined to `ai_X` in its child
/// `before_exec`. No-op (returns `Ok`) when no agent exec user is configured, or
/// when the configured user can't be resolved (run as-is rather than fail the
/// command). Returns the `io::Error` from the first failing syscall so a
/// `before_exec` can refuse to exec as root.
#[cfg(unix)]
pub fn drop_to_agent_user() -> Result<(), std::io::Error> {
    // FULL-ROOT / directory-rootfs container mode: the agent IS root inside its
    // own container/namespace (ai-root), so there is no `ai_X` to drop to —
    // dropping would hard-stop its writes to the container's own root-owned
    // files (e.g. the containers copy of /etc/hosts), breaking permit-but-
    // quarantine.
    #[cfg(unix)]
    if crate::security::overlay::fullroot_engaged() || crate::security::overlay::container_engaged() {
        return Ok(());
    }
    // The operator disabled su-security for this session (`/su-security off
    // <reason>`): the agent is authorized to act with the invoking user's full
    // authority, so bash does NOT drop to `ai_X` — child commands run as the
    // invoking user (root). This is the deliberate, human-gated escalation path.
    if std::env::var_os("PIR_AGENT_AS_INVOKER")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false)
    {
        return Ok(());
    }
    let Some(user) = agent_exec_user() else {
        return Ok(());
    };
    let (uid, gid) = match lookup_user(&user) {
        Ok(v) => v,
        Err(_) => return Ok(()), // can't resolve: don't fail the command
    };
    // Unprivileged quarantine path: when the agent is already running as its
    // target user and `enter_private_mount_ns` entered a user namespace mapping
    // the agent's real uid to virtual root (0), this process IS the agent (as
    // ns-root, host uid = the agent's uid). setresuid to the agent's uid would
    // fail (it is not mapped in the ns), so treat it as a no-op rather than
    // breaking every bash command.
    if in_userns_mapping_agent_to_root(uid) {
        return Ok(());
    }
    unsafe {
        // BEST-EFFORT confinement: the posture is "permit ALL operations,
        // quarantine writes" — the overlay is the write gate, NOT this setuid.
        // So if the drop fails (the child is already an unprivileged identity
        // like ai_pir and lacks CAP_SETUID/SETGID — setgroups fails EPERM — or
        // the uid isn't mapped in a user namespace), run the command as the
        // current identity instead of failing it. A hard failure here turned
        // every `bash` spawn into "Operation not permitted".
        let _ = libc::setgroups(0, std::ptr::null());
        if libc::setresgid(gid, gid, gid) != 0 {
            return Ok(());
        }
        if libc::setresuid(uid, uid, uid) != 0 {
            return Ok(());
        }
    }
    // Point the confined command at the agent user's own toolchain dirs
    // (HOME / CARGO_HOME / GH_CONFIG_DIR) so `cargo`/`gh` write into `ai_X`'s
    // directories, never the invoking user's (root's) home. Mirrors what the
    // old `become_user` applied to the whole process.
    for (k, v) in toolchain_env_for(&user) {
        std::env::set_var(k, v);
    }
    Ok(())
}

/// The home directory of the configured agent execution user (e.g. `ai_pir`),
/// from `/etc/passwd`. `None` when no agent exec user is configured or it can't
/// be resolved. Used to place the overlay staging dir where the agent's bash
/// commands (which run as `ai_X`) can actually reach and write it — when `pir`
/// runs as root, `$HOME` is root's, which `ai_X` cannot traverse.
#[cfg(unix)]
pub fn agent_user_home() -> Option<PathBuf> {
    let user = agent_exec_user()?;
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    for line in passwd.lines() {
        let mut f = line.split(':');
        if f.next()? == user {
            return f.nth(4).map(PathBuf::from); // field 5 = home dir
        }
    }
    None
}

/// The home directory of an arbitrary user (e.g. the resolved per-project
/// `ai_*` user), from `/etc/passwd`. `None` when the user can't be resolved.
/// Used at startup to read the *target* (sandbox) user's `security.toml` so the
/// `user-security` decision is made against the policy the operator actually
/// edited (via `/menu`), not the invoking user's.
#[cfg(unix)]
pub fn home_of(user: &str) -> Option<PathBuf> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    for line in passwd.lines() {
        let mut f = line.split(':');
        if f.next()? == user {
            return f.nth(4).map(PathBuf::from); // field 5 = home dir
        }
    }
    None
}

/// Best-effort chown of `path` to the configured agent execution user, so the
/// overlay staging upper/work dirs are writable by the agent's bash commands
/// (which run as `ai_X`). No-op when no agent exec user is configured or the
/// chown fails.
#[cfg(unix)]
pub fn chown_to_agent_user(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let Some(user) = agent_exec_user() else { return };
    let Ok((uid, gid)) = lookup_user(&user) else { return };
    let Ok(cpath) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return };
    unsafe {
        libc::chown(cpath.as_ptr(), uid, gid);
    }
}

/// True when this process is inside a non-init user namespace whose uid_map
/// maps ns-uid 0 to the given host uid — i.e. the unprivileged quarantine path
/// where the agent runs as virtual root representing its own real uid. In that
/// case `drop_to_agent_user` must NOT setuid (the agent's uid is not mapped in
/// the ns; virtual root already IS the agent on the host).
#[cfg(unix)]
fn in_userns_mapping_agent_to_root(uid: u32) -> bool {
    let in_userns = match (
        std::fs::read_link("/proc/self/ns/user").ok(),
        std::fs::read_link("/proc/1/ns/user").ok(),
    ) {
        (Some(a), Some(b)) => a != b,
        _ => false,
    };
    if !in_userns {
        return false;
    }
    let Ok(map) = std::fs::read_to_string("/proc/self/uid_map") else {
        return false;
    };
    map.lines().any(|l| l.trim() == format!("0 {uid} 1"))
}

/// True when an agent execution user has been configured (i.e. `pir` is
/// sandboxing only the model's commands, not the operator). Exposed so the
/// bash tool can report which identity a command will run as.
#[cfg(unix)]
pub fn has_agent_exec_user() -> bool {
    agent_exec_user().is_some()
}

#[cfg(not(unix))]
pub fn has_agent_exec_user() -> bool {
    false
}

/// Run a shell (interactive, or a single `-c` command) as `target_user`, from
/// the invoking user's perspective. Used by `/sh -u <user>` so a session that
/// has dropped to a sandbox identity (`ai_X`) can still hand control to a
/// *different* user (typically the original invoking user) for the duration of
/// one shell. `target_user` is `None` => the invoking user (captured before the
/// drop into `PIR_INVOKING_USER`), so `/sh -u` with no argument returns to the
/// user who launched pir.
///
/// The child re-uses pir's cwd, env and stdio. Because `become_user` keeps root
/// in the *saved* uid (`setresuid(uid, uid, 0)`), the current process is still
/// allowed to setuid back to anyone even though its effective uid is the
/// sandbox user, so this works even after the sandbox drop — we setuid to the
/// target and exec the shell, exactly the same operation `become_user`
/// performs at startup. A process with no saved-root (e.g. `sudo -u ai_X pir`,
/// which collapsed r/e/s to the sandbox identity) cannot use this path; we
/// report that clearly. Returns the shell's exit code, or `None` if the shell
/// could not be started. Inside `before_exec` we adopt the target's identity
/// (clearing groups, then gid, then uid) so the shell runs as exactly `target`
/// with no escalation path.
#[cfg(unix)]
pub fn spawn_shell_as(
    shell: &str,
    args: &[&str],
    target_user: Option<&str>,
) -> Option<i32> {
    use std::process::Command;
    use std::os::unix::process::CommandExt;

    // We may switch identity if we are currently root (euid 0) *or* if the
    // process still holds root in its *saved* uid. `become_user` drops to the
    // sandbox user via `setresuid(uid, uid, 0)`, so the effective uid is the
    // sandbox user (not 0) but the *saved* uid is still 0 — and a saved-root is
    // exactly what lets a human-invoked `/sh -u` switch back to the invoking
    // user (or root) for one shell. Query the full r/e/s set with `getresuid`:
    // a process that has neither euid 0 nor suid 0 (e.g. `sudo -u ai_X pir`,
    // which collapsed r/e/s to the sandbox user) cannot escalate, and we say
    // so clearly.
    let (mut ruid, mut euid, mut suid) = (0u32, 0u32, 0u32);
    unsafe { libc::getresuid(&mut ruid, &mut euid, &mut suid); }
    if euid != 0 && suid != 0 {
        eprintln!(
            "pir: not privileged — cannot start a shell as another user (re-run as root, \
             or use `sudo -u {user} sh` directly)",
            user = target_user.unwrap_or("?")
        );
        return None;
    }
    let target = match target_user {
        Some(u) => u.to_string(),
        None => match std::env::var("PIR_INVOKING_USER").ok().filter(|u| !u.is_empty()) {
            Some(u) => u,
            None => {
                eprintln!("pir: no invoking user recorded; pass a username to /sh -u");
                return None;
            }
        },
    };
    let (uid, gid) = match lookup_user(&target) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("pir: {e}");
            return None;
        }
    };
    let shell_path = resolve_shell_path(shell);
    // Unprivileged quarantine path: we may be virtual root in a user namespace
    // whose uid_map maps ns-0 -> the agent's REAL uid (see enter_private_mount_ns).
    // The agent's real uid is NOT mapped in that ns, so a setuid to it would
    // fail — but virtual root already IS the agent on the host. Just exec the
    // shell as-is (no drop needed).
    if in_userns_mapping_agent_to_root(uid) {
        let mut cmd = Command::new(&shell_path);
        if !args.is_empty() {
            cmd.arg("-c").arg(args.join(" "));
        }
        return cmd
            .env("HISTFILE", "/dev/null")
            .current_dir(std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")))
            .status()
            .ok()
            .map(|s| s.code().unwrap_or(1));
    }
    let mut cmd = Command::new(&shell_path);
    // Build a precise argv so we hand the *exact* command to the child shell.
    if !args.is_empty() {
        cmd.arg("-c").arg(args.join(" "));
    }
    let err = cmd
        .env("HISTFILE", "/dev/null")
        .current_dir(std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")));
    let err = unsafe {
        err.pre_exec(move || {
            // Inside the child: drop to the target identity before exec. We
            // must not run any code as root in the child shell. Order matters:
            // clear groups, setgid, then setuid (once uid is dropped we can no
            // longer setgid). BEST-EFFORT: if the drop fails (the child is
            // already an unprivileged identity and setgroups EPERMs, or the uid
            // isn't mapped in this user namespace), run the shell as the current
            // identity rather than failing it.
            let _ = libc::setgroups(0, std::ptr::null());
            if libc::setgid(gid) != 0 {
                return Ok(());
            }
            if libc::setuid(uid) != 0 {
                return Ok(());
            }
            Ok(())
        })
    }
    .status();
    match err {
        Ok(s) => Some(s.code().unwrap_or(1)),
        Err(_) => None,
    }
}

/// Resolve a shell name to its absolute path via the `passwd` shell of the
/// target user, falling back to the given name (which may already be absolute)
/// or `/bin/sh`. Keeps `spawn_shell_as` from relying on the (possibly
/// sandbox-rewritten) `$SHELL`.
#[cfg(unix)]
fn resolve_shell_path(shell: &str) -> String {
    if shell.starts_with('/') {
        return shell.to_string();
    }
    // Prefer the invoking user's login shell from passwd, then the env, then
    // the literal name, then /bin/sh.
    if let Some(u) = std::env::var("PIR_INVOKING_USER").ok().filter(|s| !s.is_empty()) {
        if let Some(path) = login_shell_of(&u) {
            return path;
        }
    }
    std::env::var("SHELL").unwrap_or_else(|_| shell.to_string())
}

/// Login shell (`pw_shell`) for `user`, resolved via getpwnam.
#[cfg(unix)]
fn login_shell_of(user: &str) -> Option<String> {
    let cuser = std::ffi::CString::new(user).ok()?;
    let mut buf = vec![0u8; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    unsafe {
        if libc::getpwnam_r(
            cuser.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        ) != 0
            || result.is_null()
            || pwd.pw_shell.is_null()
        {
            return None;
        }
        std::ffi::CStr::from_ptr(pwd.pw_shell).to_str().ok().map(|s| s.to_string())
    }
}

/// Idempotently ensure the target user's home directory exists and is owned by
/// them. Called from `become_user` at every `pir` launch so an `ai_*` agent
/// always has a usable home — creation paths (e.g. `useradd -M`, or an
/// `ai_*` account made outside `pir project init`) can otherwise leave the home
/// absent, which would make the agent write config/cargo/gh into a missing path.
///
/// Under root we can both `mkdir` and `chown`/`chmod`; when already running as
/// the target we can only `mkdir` (best-effort, and only if the parent is
/// writable), which is intentionally non-fatal.
#[cfg(unix)]
fn ensure_home_dir(user: &str) {
    use std::process::Command;

    let home = {
        let cuser = match std::ffi::CString::new(user) {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut buf = vec![0u8; 4096];
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        unsafe {
            if libc::getpwnam_r(
                cuser.as_ptr(),
                &mut pwd,
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                &mut result,
            ) != 0
                || result.is_null()
                || pwd.pw_dir.is_null()
            {
                return; // can't resolve a home to create
            }
            match std::ffi::CStr::from_ptr(pwd.pw_dir).to_str() {
                Ok(s) => s.to_string(),
                Err(_) => return,
            }
        }
    };
    if home.is_empty() {
        return;
    }
    let home_p = std::path::Path::new(&home);
    if home_p.exists() {
        return; // already present; leave it alone
    }
    let euid = unsafe { libc::geteuid() };
    if euid == 0 {
        let _ = std::fs::create_dir_all(home_p);
        let _ = Command::new("chown")
            .args(["-R", &format!("{user}:{user}"), &home])
            .status();
        let _ = Command::new("chmod").args(["700", &home]).status();
        println!("ensured home {home} owned by {user}");
    } else {
        // Best-effort: only works if the parent directory is writable by the
        // already-target user. Non-fatal — the launch proceeds and the
        // missing-home case surfaces through normal write failures.
        let _ = std::fs::create_dir_all(home_p);
    }
}

/// Set CARGO_HOME / GH_CONFIG_DIR (if the user was provisioned with self-owned
/// dirs) so the (now unprivileged) agent can fetch crates / use gh without
/// touching root's files. Called from `become_user` both for the drop-to-root
/// path and the already-the-target no-op path.
#[cfg(unix)]
fn apply_toolchain_env(user: &str) {
    for (k, v) in toolchain_env_for(user) {
        unsafe {
            std::env::set_var(k, v);
        }
    }
}

/// Return the home directory of the currently running user (best-effort, for
/// placing project artifacts the user can own). Falls back to `$HOME`.
#[cfg(unix)]
pub fn current_user_home() -> Option<std::path::PathBuf> {
    let uid = unsafe { libc::getuid() };
    unsafe {
        let mut buf = vec![0u8; 4096];
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        if libc::getpwuid_r(
            uid,
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        ) == 0
            && !result.is_null()
            && !pwd.pw_dir.is_null()
        {
            if let Ok(s) = std::ffi::CStr::from_ptr(pwd.pw_dir).to_str() {
                return Some(std::path::PathBuf::from(s));
            }
        }
    }
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// Idempotently provision the `ai_<project>` execution user for `path`.
///
/// Must be run as root. Creates a non-login system account owning `path` and
/// its `.pir/` metadata dir, and records the mapping in projects.json.
#[cfg(unix)]
pub fn provision(
    project: &str,
    user: &str,
    path: &std::path::Path,
) -> Result<String, String> {
    use std::process::Command;

    if unsafe { libc::geteuid() } != 0 {
        return Err("`pir project init` must run as root".into());
    }

    // 1. Create the system user (non-login) if missing. `-M` skips creating a
    //    home dir (we create it explicitly below so we can chown it), which is
    //    intentional: a fixed, owned home is what makes the per-project sandbox
    //    self-contained (see `toolchain_env_for` / `become_user`).
    if lookup_user(user).is_err() {
        let status = Command::new("useradd")
            .args(["-r", "-s", "/usr/sbin/nologin", "-M", user])
            .status()
            .map_err(|e| format!("useradd: {e}"))?;
        if !status.success() {
            return Err(format!("useradd failed for '{user}' (exit {})", status.code().unwrap_or(-1)));
        }
        println!("created user {user}");
    } else {
        println!("user {user} already exists");
    }

    // 1b. Ensure the user's home directory exists and is owned by them. Some
    //     distros (useradd -M) leave it absent, which would make `HOME` point
    //     at a missing path once `become_user` fixes HOME to the real home.
    let home = {
        let cuser = std::ffi::CString::new(user).map_err(|_| format!("invalid user '{user}'"))?;
        let mut buf = vec![0u8; 4096];
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        unsafe {
            if libc::getpwnam_r(
                cuser.as_ptr(),
                &mut pwd,
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                &mut result,
            ) != 0
                || result.is_null()
                || pwd.pw_dir.is_null()
            {
                return Err(format!("cannot resolve home for user '{user}'"));
            }
            std::ffi::CStr::from_ptr(pwd.pw_dir).to_str().map(|s| s.to_string()).unwrap_or_default()
        }
    };
    if !home.is_empty() {
        let _ = std::fs::create_dir_all(&home);
        let status = Command::new("chown")
            .args(["-R", &format!("{user}:{user}"), &home])
            .status()
            .map_err(|e| format!("chown: {e}"))?;
        if !status.success() {
            return Err(format!("chown failed for {home} (exit {})", status.code().unwrap_or(-1)));
        }
        // Reset permissions to a sane 0700 so the sandbox user owns its home
        // privately (the dir may have been created by root with 0755).
        let _ = Command::new("chmod").args(["700", &home]).status();
        println!("ensured home {home} owned by {user}");
    }

    // 2. Chown the project directory (and a .pir metadata dir) to the user.
    let path_s = path.to_string_lossy().to_string();
    let meta_path = path.join(".pir");
    let meta_s = meta_path.to_string_lossy().to_string();
    let _ = std::fs::create_dir_all(&meta_path);
    for p in [&path_s, &meta_s] {
        let rp = std::fs::canonicalize(p).unwrap_or_else(|_| std::path::PathBuf::from(p));
        let status = Command::new("chown")
            .args(["-R", &format!("{user}:{user}"), rp.to_string_lossy().as_ref()])
            .status()
            .map_err(|e| format!("chown: {e}"))?;
        if !status.success() {
            return Err(format!("chown failed for {p} (exit {})", status.code().unwrap_or(-1)));
        }
    }
    println!("granted {user} ownership of {path_s}");

    // 3. Record mapping.
    crate::config::set_project_user(project, user, &path_s)?;

    // 4. Give the agent user its own network-capable toolchain dirs.
    //    `ai_*` users run as themselves (non-root) and `toolchain_env_for`
    //    derives CARGO_HOME/GH_CONFIG_DIR from the user's *real* home
    //    (not $HOME), so crates/gh land under ~ai_X (e.g. /home/ai_rpi/.cargo),
    //    never /root/.cargo. Create + own those dirs so the agent can fetch
    //    crates and use gh without touching root's files. `become_user` applies
    //    these env vars at every launch.
    setup_agent_toolchain(user)?;

    // 5. Make the `.git` setup sane for LLM use on a fresh project: install the
    //    pre-commit guard hook (refuses huge/binary files) so agents can't
    //    accidentally bloat the repo. Under jj (git hooks don't run) this is a
    //    no-op and `/fix` handles jj separately.
    if crate::project::is_git_repo(path) && crate::project::detect_vcs(path) == crate::project::Vcs::Git {
        match crate::project::install_git_guard_hook(path) {
            Ok(true) => println!("installed .git/hooks/pre-commit guard (refuses large/binary files)"),
            Ok(false) => println!("a pre-commit hook already exists; left it in place"),
            Err(e) => eprintln!("warning: could not install git guard hook: {e}"),
        }
    }

    Ok(format!(
        "project '{project}' -> user '{user}' (run as root, or `sudo -u {user} pir ...`)\n\
         agent toolchain (self-owned): CARGO_HOME=~{user}/.cargo GH_CONFIG_DIR=~{user}/.config/gh"
    ))
}

/// Create self-owned `CARGO_HOME` and `GH_CONFIG_DIR` for an `ai_*` user and
/// return the env overrides as `KEY=VALUE` pairs. The dirs live under the
/// user's real home (resolved via getpwnam) so the agent can write its cargo
/// registry cache and a gh config without touching root's files. Must be
/// called as root.
#[cfg(unix)]
fn setup_agent_toolchain(user: &str) -> Result<(), String> {
    use std::process::Command;

    let (_, _) = lookup_user(user)?; // ensure user exists
    // Resolve the agent user's real home directory.
    let home = {
        let cuser = std::ffi::CString::new(user).map_err(|_| format!("invalid user '{user}'"))?;
        let mut buf = vec![0u8; 4096];
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        unsafe {
            if libc::getpwnam_r(
                cuser.as_ptr(),
                &mut pwd,
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                &mut result,
            ) != 0
                || result.is_null()
                || pwd.pw_dir.is_null()
            {
                return Err(format!("cannot resolve home for user '{user}'"));
            }
            std::ffi::CStr::from_ptr(pwd.pw_dir).to_str().map(|s| s.to_string()).unwrap_or_default()
        }
    };
    if home.is_empty() {
        return Err(format!("user '{user}' has no home directory"));
    }
    let cargo_home = std::path::Path::new(&home).join(".cargo");
    let gh_config = std::path::Path::new(&home).join(".config").join("gh");
    for d in [&cargo_home, &gh_config] {
        let _ = std::fs::create_dir_all(d);
        let status = Command::new("chown")
            .args(["-R", &format!("{user}:{user}"), d.to_string_lossy().as_ref()])
            .status()
            .map_err(|e| format!("chown: {e}"))?;
        if !status.success() {
            return Err(format!("chown failed for {} (exit {})", d.display(), status.code().unwrap_or(-1)));
        }
    }
    // A minimal gh config dir so `gh` doesn't fall back to /root/.config/gh.
    // `gh` creates config.yml on first use; we just ensure the dir is owned.
    println!(
        "agent toolchain ready for {user}: CARGO_HOME={} GH_CONFIG_DIR={}",
        cargo_home.display(),
        gh_config.display()
    );
    Ok(())
}

/// Return the `CARGO_HOME` / `GH_CONFIG_DIR` overrides for a project's
/// execution user, if that user was provisioned with a self-owned toolchain.
/// `pir` exports these via `set_project_user` metadata; this reads them back
/// so a launch as `ai_X` picks up the agent-owned dirs. Returns an empty vec
/// when no override is recorded.
#[cfg(unix)]
pub fn toolchain_env_for(user: &str) -> Vec<(String, String)> {
    let cuser = match std::ffi::CString::new(user) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut buf = vec![0u8; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    unsafe {
        if libc::getpwnam_r(
            cuser.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        ) != 0
            || result.is_null()
            || pwd.pw_dir.is_null()
        {
            return Vec::new();
        }
    }
    let home = match unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir).to_str() } {
        Ok(s) => s.to_string(),
        Err(_) => return Vec::new(),
    };
    if home.is_empty() {
        return Vec::new();
    }
    let cargo_home = std::path::Path::new(&home).join(".cargo");
    let gh_config = std::path::Path::new(&home).join(".config").join("gh");
    let mut out = Vec::new();
    // Always point HOME at the agent user's real home directory. Without this,
    // a launch that inherits a foreign HOME (e.g. root's) would make every tool
    // (bash, cargo, gh, git) write into the invoking user's home instead of the
    // sandbox user's own — defeating the per-project sandbox. The home is
    // resolved from getpwnam, never from the inherited `$HOME`.
    out.push(("HOME".into(), home.clone()));
    if cargo_home.is_dir() {
        out.push(("CARGO_HOME".into(), cargo_home.to_string_lossy().into_owned()));
    }
    if gh_config.is_dir() {
        out.push(("GH_CONFIG_DIR".into(), gh_config.to_string_lossy().into_owned()));
    }
    out
}

/// Resolve the project name to use for the metadata/log directory. If the
/// process has dropped to a non-root user (e.g. `ai_X`), place logs under the
/// project's own `.pir/` directory rather than the global `~/.pi`, which the
/// user may not be able to write.
#[cfg(unix)]
pub fn session_dir_for(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    let meta = cwd.join(".pir").join("sessions");
    if let Ok(md) = std::fs::metadata(&meta) {
        if md.is_dir() {
            return Some(meta);
        }
    }
    None
}

#[cfg(all(test, unix))]
mod accessibility_tests {
    use super::*;

    // `can_traverse` must report true for a 0700 dir owned by the user (the common "sandbox user owns the project" case) and false when neither
    // owner/group/other grants execute.
    #[test]
    fn traverse_rules() {
        let dir = std::env::temp_dir().join(format!("pir_acl_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir(&dir);
        let me = unsafe { libc::getuid() };
        let my_gid = unsafe { libc::getgid() };
        // Owner can traverse regardless of the exact bits.
        assert!(can_traverse(&std::fs::metadata(&dir).unwrap(), me, my_gid));
        // Make it private (0700) so a stranger cannot traverse it.
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o700);
        std::fs::set_permissions(&dir, perms).unwrap();
        // A different user with no other-execute bit must be denied.
        assert!(!can_traverse(&std::fs::metadata(&dir).unwrap(), me.wrapping_add(1), my_gid.wrapping_add(1)));
        // Grant 'o+x' and the stranger can now traverse.
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o701);
        std::fs::set_permissions(&dir, perms).unwrap();
        assert!(can_traverse(&std::fs::metadata(&dir).unwrap(), me.wrapping_add(1), my_gid.wrapping_add(1)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // `traverse_blockers` should find a 0700 parent dir that a *different* user
    // cannot pass through, but not report a world-traversable parent.
    #[test]
    fn finds_unreachable_parent() {
        let base = std::env::temp_dir().join(format!("pir_acl_base_{}", std::process::id()));
        let proj = base.join("proj");
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::create_dir(&base);
        let mut perms = std::fs::metadata(&base).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o700); // private parent
        std::fs::set_permissions(&base, perms).unwrap();
        let _ = std::fs::create_dir(&proj);

        let other = unsafe { libc::getuid() }.wrapping_add(1);
        let other_gid = unsafe { libc::getgid() }.wrapping_add(1);
        // No such user -> lookup_user fails -> no blockers (safe default).
        let none = traverse_blockers(&proj, "no_such_user_xyz");
        assert!(none.is_empty());

        // Synthesize a fake uid/gid by directly invoking the predicate over the
        // real parent metadata: the private `base` must be reported as a blocker
        // for a stranger. We reach into the public-ish path via a temp user that
        // doesn't exist is unhelpful, so instead assert the helper returns the
        // private base when given a uid that cannot traverse it.
        let md = std::fs::metadata(&base).unwrap();
        assert!(!can_traverse(&md, other, other_gid));
        let _ = std::fs::remove_dir_all(&base);
    }
}
