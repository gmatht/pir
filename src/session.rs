//! Unfinished-conversation tracking.
//!
//! Every interactive / one-shot / background session gets a sidecar
//! `<session>.status.json` written by the `Agent` as it runs:
//!
//!   * `active`      — a turn is currently in flight (records the live process
//!                     pid that owns the session);
//!   * `completed`   — a turn finished cleanly (the conversation was brought to
//!                     a natural stopping point);
//!   * `interrupted` — a turn ended early (user cancel, a network/provider
//!                     error, the token budget was hit, …).
//!
//! A conversation counts as *unfinished* precisely when **no live process is
//! driving it** but it isn't in a clean end-state:
//!
//!   * it was explicitly `interrupted`, or
//!   * its last recorded status is `active` yet the owning pid is no longer
//!     alive (the process crashed / was killed / the machine rebooted / a
//!     network failure dropped the connection mid-turn), or
//!   * it has a goal whose steps are still pending / in-progress.
//!
//! This lets a user come back later and resume exactly those threads, with the
//! guarantee that nothing is currently mutating them.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::term;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    #[default]
    Active,
    Completed,
    Interrupted,
}

impl SessionStatus {
    pub fn label(self) -> &'static str {
        match self {
            SessionStatus::Active => "active",
            SessionStatus::Completed => "completed",
            SessionStatus::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionMeta {
    #[serde(default)]
    pub status: SessionStatus,
    /// Pid of the process that last wrote this status. 0 when unknown.
    #[serde(default)]
    pub pid: u32,
    /// Unix epoch seconds at the time of the last write (best-effort).
    #[serde(default)]
    pub updated: u64,
    #[serde(default)]
    pub last_prompt: String,
    /// When true, a goal file exists for this session and it is not complete.
    #[serde(default)]
    pub goal_pending: bool,
    /// Why the turn ended early (only set when `status == interrupted`).
    #[serde(default)]
    pub reason: String,
}

pub fn status_path(log: &Path) -> PathBuf {
    log.with_extension("status.json")
}

/// Persist a status for `log`. A missing/empty log path is a no-op (one-shot
/// sessions that opted out of a transcript never get tracked).
pub fn write_status(
    log: &Path,
    status: SessionStatus,
    pid: u32,
    last_prompt: &str,
    goal_pending: bool,
    reason: &str,
) {
    if log.as_os_str().is_empty() {
        return;
    }
    let updated = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let meta = SessionMeta {
        status,
        pid,
        updated,
        last_prompt: last_prompt.to_string(),
        goal_pending,
        reason: reason.to_string(),
    };
    if let Ok(s) = serde_json::to_string_pretty(&meta) {
        let _ = fs::write(status_path(log), s);
    }
}

pub fn read_status(log: &Path) -> Option<SessionMeta> {
    let raw = fs::read_to_string(status_path(log)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Whether `pid` names a process that is currently alive. Used to decide if a
/// session still has an active client working on it.
#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    pid != 0 && unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
pub fn pid_alive(_pid: u32) -> bool {
    // Can't probe process liveness portably; treat any recorded pid as alive so
    // we don't falsely report sessions as unfinished.
    true
}

pub struct UnfinishedEntry {
    pub path: PathBuf,
    pub name: String,
    pub shell_pid: u32,
    pub preview: String,
    pub reason: String,
    pub mtime: SystemTime,
}

/// Scan the sessions directory and return conversations that are unfinished and
/// not currently being driven by a live process. Sorted newest-modified first.
pub fn scan_unfinished() -> Vec<UnfinishedEntry> {
    let dir = crate::config::pi_dir().join("agent").join("sessions");
    let Ok(entries) = fs::read_dir(&dir) else { return Vec::new() };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        let shell_pid = name
            .rsplit("sh")
            .next()
            .and_then(|s| s.trim_end_matches(".jsonl").trim().parse::<u32>().ok())
            .unwrap_or(0);
        let meta = match read_status(&path) {
            Some(m) => m,
            None => continue,
        };

        // A live client is actively driving this session right now if a process
        // with the recorded pid still exists.
        let live_client = meta.pid != 0 && pid_alive(meta.pid);

        let unfinished = !live_client
            && (meta.status == SessionStatus::Interrupted
                || meta.status == SessionStatus::Active
                || meta.goal_pending);
        if !unfinished {
            continue;
        }

        let reason = if meta.status == SessionStatus::Interrupted {
            if meta.reason.is_empty() {
                "interrupted".to_string()
            } else {
                meta.reason.clone()
            }
        } else if meta.status == SessionStatus::Active {
            "turn did not finish (crashed / killed / network failure)".to_string()
        } else {
            "goal still in progress".to_string()
        };

        let preview = first_user_line(&path);
        let mtime = e.metadata().and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH);
        out.push(UnfinishedEntry {
            path: path.clone(),
            name,
            shell_pid,
            preview,
            reason,
            mtime,
        });
    }
    out.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    out
}

fn first_user_line(path: &Path) -> String {
    if let Ok(f) = fs::File::open(path) {
        for line in std::io::BufReader::new(f).lines().flatten() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                if v.get("role").and_then(|r| r.as_str()) == Some("user") {
                    if let Some(txt) = v
                        .get("blocks")
                        .and_then(|b| b.as_array())
                        .and_then(|a| {
                            a.iter()
                                .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                        })
                        .and_then(|b| b.get("text").and_then(|t| t.as_str()))
                    {
                        let s = txt.lines().next().unwrap_or("").trim();
                        if !s.is_empty() {
                            return truncate(s, 80);
                        }
                    }
                }
            }
        }
    }
    String::new()
}

/// Human-readable listing of unfinished sessions, newest first. Returns an
/// empty string (no trailing newline) when there are none, so callers can print
/// it directly (or skip when empty).
pub fn list_unfinished() -> String {
    let entries = scan_unfinished();
    if entries.is_empty() {
        return term::dim("(no unfinished sessions — nothing crashed or left a goal in progress)");
    }
    let mut out = String::new();
    out.push_str(&term::bold("unfinished sessions (no live process driving them)\n"));
    for (i, e) in entries.iter().enumerate() {
        out.push_str(&format!(
            "  #{:<3} [{}] {}   {}\n       {}\n",
            i,
            term::cyan(&e.reason),
            e.name,
            term::dim(&format!("sh{}", e.shell_pid)),
            truncate(&e.preview, 80),
        ));
    }
    out.push_str(&term::dim("resume with: /resume <index|path-fragment>"));
    out
}

/// Resolve a user token (index like `0`, or a path/fragment substring) to a
/// session log path among the unfinished entries. Returns None if nothing
/// matches.
pub fn resolve_unfinished(token: &str) -> Option<PathBuf> {
    let entries = scan_unfinished();
    let t = token.trim();
    if t.is_empty() {
        return None;
    }
    // Numeric index (0 = newest, matching the listing order).
    if let Ok(idx) = t.parse::<usize>() {
        return entries.get(idx).map(|e| e.path.clone());
    }
    // Otherwise treat as a case-insensitive substring of the session name.
    let lower = t.to_lowercase();
    entries
        .into_iter()
        .find(|e| e.name.to_lowercase().contains(&lower))
        .map(|e| e.path)
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_log(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pir_status_tests_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir.join(format!("{name}.jsonl"))
    }

    #[test]
    fn status_roundtrips_and_reports_reason() {
        let log = tmp_log("sess");
        write_status(&log, SessionStatus::Interrupted, 1234, "fix the parser", false, "network failure");
        let m = read_status(&log).expect("status should be readable");
        assert_eq!(m.status, SessionStatus::Interrupted);
        assert_eq!(m.pid, 1234);
        assert_eq!(m.last_prompt, "fix the parser");
        assert_eq!(m.reason, "network failure");

        write_status(&log, SessionStatus::Completed, 1234, "fix the parser", false, "");
        let m = read_status(&log).unwrap();
        assert_eq!(m.status, SessionStatus::Completed);
        assert!(m.reason.is_empty());

        let _ = std::fs::remove_file(status_path(&log));
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn empty_log_path_is_noop() {
        // Should not panic; nothing to read back.
        write_status(Path::new(""), SessionStatus::Active, 1, "", false, "");
        assert!(read_status(Path::new("")).is_none());
    }
}
