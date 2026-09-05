//! Cross-platform "move to trash / Recycle Bin" used by the mitigation engine
//! to make `rm`/`del` deletions *reversible* (docs/MITIGATION_LEVEL_SECURITY.md
//! §4: on Windows a Recycle-Bin wrapper, on Linux `trash-put`). The file stays
//! recoverable until the operator empties the bin, so a "delete" is genuinely
//! mitigated rather than destroyed.
//!
//! The module keeps a process-wide counter of how many files *this agent* has
//! moved to trash, so the operator can be told "N agent files are now in the
//! trash" (the user explicitly asked for this).

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Process-wide count of files this agent has moved to the OS trash/Recycle
/// Bin during the session. Informs the operator how much is still recoverable.
static TRASH_MOVED: AtomicUsize = AtomicUsize::new(0);

/// Record that `n` more files were moved to trash this session.
pub fn record_trash(n: usize) {
    if n > 0 {
        TRASH_MOVED.fetch_add(n, Ordering::SeqCst);
    }
}

/// How many files this agent has moved to trash this session.
pub fn trash_moved_count() -> usize {
    TRASH_MOVED.load(Ordering::SeqCst)
}

/// Build the OS command string that moves `paths` to trash. Returns a plain
/// shell command (the mitigation engine runs it through the normal `bash`/
/// `cmd` path, pure). `recursive` adds the flag needed to
/// move directories.
pub fn trash_command_for(paths: &[String], recursive: bool) -> String {
    if paths.is_empty() {
        return String::new();
    }
    if cfg!(windows) {
        // Windows: move each target to the Recycle Bin via PowerShell's
        // `-RecycleBin` switch (available on Windows 10/11 PowerShell 5.1+).
        // Single quotes are escaped by doubling, which is the PowerShell rule.
        let mut inner = String::new();
        for p in paths {
            inner.push_str(&format!(
                "Remove-Item -LiteralPath '{}' -RecycleBin -Force{}; ",
                p.replace('\'', "''"),
                if recursive { " -Recurse" } else { "" }
            ));
        }
        format!("powershell -NoProfile -Command \"{}\"", inner.trim_end())
    } else {
        // Linux/macOS: `trash-put` (freedesktop spec). `trash-put` accepts a
        // `-r`/`-R` flag for directories.
        let mut c = String::from("trash-put");
        if recursive {
            c.push_str(" -r");
        }
        for p in paths {
            c.push(' ');
            c.push_str(p);
        }
        c
    }
}

/// Actually move `path` to the OS trash/Recycle Bin directly (not via a shell
/// string). Returns an error string on failure — the caller must then surface
/// it and must NOT claim the deletion was mitigated. Used by direct callers
/// and as the authoritative implementation behind [`trash_command_for`].
pub fn move_to_trash(path: &Path) -> Result<(), String> {
    let ok = if cfg!(windows) {
        let p = path.to_string_lossy().replace('\'', "''");
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("Remove-Item -LiteralPath '{}' -RecycleBin -Force", p),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    } else {
        matches!(Command::new("trash-put").arg(path).status(), Ok(s) if s.success())
    };
    if ok {
        record_trash(1);
        Ok(())
    } else {
        Err(format!("could not move {} to trash", path.display()))
    }
}
