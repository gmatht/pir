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
    let (uid, gid) = lookup_user(user)?;
    let euid = unsafe { libc::geteuid() };
    // Already running as the target user (e.g. `sudo -u ai_X pir …`): there is
    // nothing to drop, but still point the agent at its self-owned toolchain
    // dirs so crates / gh don't land in the invoking user's home.
    if euid == uid {
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
    // Point the (now unprivileged) agent at its own, self-owned toolchain dirs
    // so it can fetch crates / use gh without touching root's files.
    apply_toolchain_env(user);
    Ok(())
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

    // 4. Give the agent user its own network-capable toolchain dirs.
    //    `ai_*` users run as themselves (non-root) but $HOME is usually still
    //    inherited from root, so the default /root/.cargo and /root/.config/gh
    //    are unwritable. Create self-owned CARGO_HOME and GH_CONFIG_DIR so the
    //    agent can fetch crates and use gh without touching root's files.
    //    `toolchain_env_for` exposes these so a launch as this user picks them
    //    up.
    setup_agent_toolchain(user)?;

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
