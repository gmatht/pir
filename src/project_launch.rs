//! Opening a project directory in a new pir process — the one implementation
//! behind the GUI's "New Project" button, its Session menu item and
//! `/new-project`, and the terminal `/new-project` command.
//!
//! # Why a new process rather than a tab that points elsewhere
//!
//! `Agent::new`, `session::session_cwd()`, the `Registry` cwd, the session-log
//! location and `workspace_label()` all read the **process** working directory.
//! Repointing one GUI tab at another directory would therefore repoint every
//! other tab too (and the terminal REPL). A fresh process with that directory as
//! its cwd makes every one of those values correct by construction, and leaves
//! the current window/session untouched.
//!
//! # Detaching
//!
//! The child is started detached so it outlives (and never shares a terminal
//! with) the parent: `setsid` on unix — never `process_group(0)`, which makes
//! the child a group leader and can leave it able to grab pir's terminal — and
//! `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` on Windows. Its stdio is
//! nulled, and it is reaped on a short worker thread so the caller neither
//! blocks nor leaves a zombie.

use std::path::PathBuf;

/// Resolve the directory a project request should open.
///
/// * an existing directory → `Ok(that path)`, canonicalized so the new
///   process's workspace label and session grouping agree with every other view
///   of the same project;
/// * a missing path with `create = false` → `Err`, naming the path (the caller
///   turns this into an explicit "create it?" prompt, never a silent mkdir);
/// * a missing path with `create = true` → created with `create_dir_all` (so
///   multi-level paths work), then `Ok`;
/// * an existing *file* → `Err` — a file is never a project.
///
/// `~` is expanded so a typed `~/src/thing` behaves like a shell would.
pub fn target(input: &str, create: bool) -> Result<PathBuf, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("no directory given".into());
    }
    let expanded: PathBuf = if raw == "~" {
        PathBuf::from(std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("~")))
    } else if let Some(rest) = raw.strip_prefix("~/") {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("~"));
        home.join(rest)
    } else {
        PathBuf::from(raw)
    };
    match std::fs::metadata(&expanded) {
        Ok(m) if m.is_dir() => Ok(crate::security::canonicalize_lenient(&expanded)),
        Ok(_) => Err(format!("{} is a file, not a directory", expanded.display())),
        Err(_) if create => std::fs::create_dir_all(&expanded)
            .map(|_| crate::security::canonicalize_lenient(&expanded))
            .map_err(|e| format!("could not create {}: {e}", expanded.display())),
        Err(_) => Err(format!("{} does not exist", expanded.display())),
    }
}

/// Launch pir with `dir` as its working directory, detached. Returns the child
/// pid. `gui` selects the graphical front end (`--gui`); `full_auto` forwards
/// the current session's posture.
pub fn launch(dir: &PathBuf, full_auto: bool) -> Result<u32, String> {
    let gui = std::env::args().any(|a| a == "--gui");
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate the pir binary: {e}"))?;
    let mut cmd = std::process::Command::new(exe);
    if gui {
        cmd.arg("--gui");
    }
    if full_auto {
        cmd.arg("--full-auto");
    }
    cmd.current_dir(dir);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` is async-signal-safe and is the only thing that runs
        // in the child before exec.
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: no console, own group.
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    let child = cmd.spawn().map_err(|e| format!("could not start pir in {}: {e}", dir.display()))?;
    let pid = child.id();
    // Reap on a worker: detaching must not leave a zombie, and `wait` must not
    // run on the GUI's main thread.
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "pir-proj-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn existing_directory_resolves_and_is_canonicalized() {
        let p = tmp("exists");
        std::fs::create_dir_all(&p).unwrap();
        let got = target(p.to_str().unwrap(), false).expect("an existing dir must resolve");
        assert!(got.is_dir());
        assert_eq!(got, crate::security::canonicalize_lenient(&p));
        let _ = std::fs::remove_dir_all(&p);
    }

    #[test]
    fn missing_directory_is_an_error_unless_creation_is_allowed() {
        let p = tmp("missing");
        // Not created: an error that names the path, so the caller can prompt.
        let err = target(p.to_str().unwrap(), false).expect_err("a missing dir must not silently resolve");
        assert!(err.contains(&p.display().to_string()), "the error must name the path: {err}");
        assert!(!p.exists(), "target() must not create anything when create=false");
        // Explicitly allowed: created (multi-level, so parents come too).
        let nested = p.join("a").join("b");
        let got = target(nested.to_str().unwrap(), true).expect("create=true must create it");
        assert!(got.is_dir());
        let _ = std::fs::remove_dir_all(&p);
    }

    #[test]
    fn a_file_is_never_a_project() {
        let p = tmp("file");
        std::fs::create_dir_all(&p).unwrap();
        let f = p.join("notes.txt");
        std::fs::write(&f, "x").unwrap();
        let err = target(f.to_str().unwrap(), true).expect_err("a file must not resolve as a project");
        assert!(err.contains("file, not a directory"), "got: {err}");
        let _ = std::fs::remove_dir_all(&p);
    }

    #[test]
    fn empty_input_is_rejected() {
        assert!(target("   ", true).is_err());
    }
}
