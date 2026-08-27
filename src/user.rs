//! Per-project execution user (`ai_<project>`).
//!
//! Every tool — `bash` and the file tools — runs under the *current process
//! identity*, so the simplest correct sandbox is to make the whole `pir`
//! process become `ai_X` (via setuid/setgid) after config is loaded. The tool
//! layer needs no changes; `spawn_shell` and all `fs` calls then run as
//! `ai_X`.
//!
//! This module is unix-only. On non-unix targets the functions return
//! `Err(...)` explaining the feature is unsupported, and the agent falls back
//! to running as the invoking user.

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

/// Drop privileges to the given user (unix only). Call *after* config and
/// providers are loaded but *before* the agent is built and any tool runs.
#[cfg(unix)]
pub fn become_user(user: &str) -> Result<(), String> {
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        return Err(format!(
            "pir must run as root to switch to user '{user}'. Re-run as root, or use \
             `sudo -u {user} pir ...`"
        ));
    }
    let (uid, gid) = lookup_user(user)?;
    unsafe {
        // Drop supplementary groups first, then gid, then uid (order matters:
        // once uid is dropped we can no longer setgid).
        if libc::setgroups(0, std::ptr::null()) != 0 {
            return Err(format!("failed to clear groups for '{user}'"));
        }
        if libc::setgid(gid) != 0 {
            return Err(format!("failed to setgid to '{user}'"));
        }
        if libc::setuid(uid) != 0 {
            return Err(format!("failed to setuid to '{user}'"));
        }
    }
    Ok(())
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

    // 1. Create the system user (non-login) if missing.
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
    Ok(format!("project '{project}' -> user '{user}' (run as root, or `sudo -u {user} pir ...`)"))
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
