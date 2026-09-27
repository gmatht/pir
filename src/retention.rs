//! Session-transcript retention.
//!
//! The transcript is the one file pir writes continuously for the whole life
//! of a session, append-only and never rotated or pruned. A long-lived agent
//! loop that starts pir many times a day leaves one transcript per start,
//! plus a sidecar per session (`.status.json`, `.checkpoints.json`, `.model`,
//! `.goal.json`, `.history`, `.incmd`, `.susec`, `.plan`, `.thinking`).
//!
//! On a machine that was running seven agent loops concurrently this reached
//! 1.1 GB under `~/.pi/agent/sessions`, with a single 76 MB transcript — and
//! that was a secondary, not a primary, cause of the IO pressure: the
//! transcripts are pure buffered append, so they sit in the page cache as
//! dirty pages and are written back by the same single virtio queue that the
//! build trees were saturating.
//!
//! What this module does, at startup:
//!   * deletes a *whole* session (transcript + sidecars) when it is older than
//!     [`TTL_DAYS`];
//!   * truncates the single largest transcript to its last [`MAX_BYTES`] once it
//!     exceeds that, preserving a `.head` file so the beginning of the session
//!     is not silently lost.
//!
//! Deliberately NOT done: rotating the live transcript while a session is
//! running. Rotation needs the writer to cooperate (reopen after rename), and a
//! transcript that is being appended to by the live session is the one file an
//! operator most wants intact. The cap therefore applies to *stale* sessions
//! at startup, and a session that genuinely exceeds it while running is
//! reported rather than mangled.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Default age at which a whole session is removed.
pub const TTL_DAYS: u64 = 30;

/// Default per-transcript cap. A 76 MB transcript is not useful to read and is
/// pure write amplification; 16 MB keeps a long session's tail readable.
pub const MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Sidecar suffixes that belong to a session and must be removed with it.
/// Without these, deleting a transcript would leave every status/goal/undo
/// sidecar behind and the directory would still grow.
const SIDECAR_SUFFIXES: &[&str] = &[
    "status.json",
    "checkpoints.json",
    "model",
    "goal.json",
    "history",
    "incmd",
    "susec",
    "plan",
    "thinking",
];

/// Summary of one sweep, for the startup line and for tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    /// Whole sessions (transcript + sidecars) removed for age.
    pub removed_sessions: usize,
    /// Transcripts truncated to their tail.
    pub truncated: usize,
    /// Bytes reclaimed (approximate: the removed size of deleted/truncated files).
    pub freed_bytes: u64,
}

/// Run the retention sweep over the global sessions directory.
/// Best-effort and total: a file that cannot be removed is skipped and the
/// sweep continues, because retention must never fail a session.
///
/// `keep` is the current session's own transcript: it must survive untouched
/// even if something upstream gives it a stale mtime or an odd size.
pub fn sweep_global(keep: Option<&Path>) -> SweepReport {
    let dir = crate::config::pi_dir().join("agent").join("sessions");
    sweep_dir(&dir, keep)
}

/// Run the retention sweep over one project's `.pir/sessions`.
pub fn sweep_project(dir: &Path, keep: Option<&Path>) -> SweepReport {
    sweep_dir(dir, keep)
}

/// `keep` names a transcript path that must survive untouched even if it looks
/// stale (the current session's own transcript).
pub fn sweep_dir(dir: &Path, keep: Option<&Path>) -> SweepReport {
    let mut rep = SweepReport::default();
    if std::env::var_os("PIR_SESSION_KEEP").is_some() {
        return rep;
    }
    let ttl_days = env_num("PIR_SESSION_TTL_DAYS", TTL_DAYS);
    let max_bytes = env_num("PIR_SESSION_MAX_BYTES", MAX_BYTES);
    let cutoff = Duration::from_secs(ttl_days.saturating_mul(24 * 3600));
    let now = SystemTime::now();

    // The directory holds per-project subdirectories (named `--home-foo-bar--`)
    // as well as loose transcripts. Sweep both levels.
    let mut roots: Vec<PathBuf> = vec![dir.to_path_buf()];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                roots.push(p);
            }
        }
    }

    for root in roots {
        let Ok(rd) = std::fs::read_dir(&root) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_file() {
                continue;
            }
            let is_transcript = p.extension().and_then(|x| x.to_str()) == Some("jsonl");
            if !is_transcript && !is_sidecar(&p) {
                continue;
            }
            if let Some(k) = keep
                && (p == k || p.starts_with(k))
            {
                continue;
            }
            let Ok(md) = p.metadata() else { continue };
            let size = md.len();
            let Ok(modified) = md.modified() else { continue };
            let age = match now.duration_since(modified) {
                Ok(a) => a,
                // A future mtime (clock skew) is treated as fresh.
                Err(_) => continue,
            };

            if age > cutoff {
                // Age applies to the whole session: a transcript past the TTL
                // takes its sidecars with it. A sidecar is only removed when
                // its own base transcript is also gone, so a single stale
                // sidecar cannot orphan-delete a live session's data.
                if is_transcript {
                    if remove(&p) {
                        rep.removed_sessions += 1;
                        rep.freed_bytes += size;
                        for s in sidecars_of(&p) {
                            if remove(&s) {
                                rep.freed_bytes += s.metadata().map(|m| m.len()).unwrap_or(0);
                            }
                        }
                    }
                }
                continue;
            }

            // Fresh enough to keep, but a single transcript may still be
            // enormous. Truncate the tail, keeping the head in a `.head`
            // sidecar so the start of the session survives.
            if is_transcript && size > max_bytes {
                if truncate_keeping_tail(&p, max_bytes) {
                    rep.truncated += 1;
                    rep.freed_bytes += size.saturating_sub(max_bytes);
                }
            }
        }
    }
    rep
}

/// Is this file one of a transcript's sidecars?
fn is_sidecar(p: &Path) -> bool {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    // `pir-2026...-sh1.status.json` etc. Match on the suffix following the
    // transcript stem: a sidecar is any file whose name contains a dot after
    // the `pir-` stem. Restrict to the known list so unrelated files in the
    // sessions dir are never touched.
    SIDECAR_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// The sidecar paths belonging to transcript `p`.
fn sidecars_of(p: &Path) -> Vec<PathBuf> {
    let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
        return Vec::new();
    };
    let Some(dir) = p.parent() else { return Vec::new() };
    let mut out = Vec::new();
    for s in SIDECAR_SUFFIXES {
        // `with_extension` cannot express `.status.json` (it would replace
        // `.json`), so build the name explicitly.
        out.push(dir.join(format!("{stem}.{s}")));
    }
    out
}

/// Rewrite `p` so it holds at most `max_bytes` of its TAIL, preserving the
/// head in `<stem>.head`. The tail is kept because it is the part an operator
/// resuming a session actually reads; the head is kept because it holds the
/// original goal and the early turns, which the tail does not.
///
/// Returns true when the file was rewritten. Any error leaves the original
/// file untouched — a failed truncation must never destroy a transcript.
fn truncate_keeping_tail(p: &Path, max_bytes: u64) -> bool {
    let Ok(data) = std::fs::read(p) else { return false };
    if data.len() as u64 <= max_bytes {
        return false;
    }
    let start = data.len() - max_bytes as usize;
    // Snap to the next newline so the first retained line is a whole JSON
    // record: a partially-cut line would make the transcript unparseable.
    let start = match data[start..].iter().position(|&b| b == b'\n') {
        Some(off) => start + off + 1,
        None => start,
    };
    // Preserve the head (everything dropped) as a sidecar before rewriting.
    let head_path = p.with_extension("head");
    if std::fs::write(&head_path, &data[..start]).is_err() {
        // Cannot preserve the head: do NOT truncate. Losing the start of a
        // session silently is worse than leaving a large file in place.
        return false;
    }
    // Skip a partial line at the very start of the tail too.
    let tail = &data[start..];
    let tail = match tail.iter().position(|&b| b == b'\n') {
        Some(0) => &tail[1..],
        _ => tail,
    };
    std::fs::write(p, tail).is_ok()
}

fn remove(p: &Path) -> bool {
    std::fs::remove_file(p).is_ok()
}

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok()).filter(|&n| n > 0).unwrap_or(default)
}

/// A one-line summary for the startup banner.
pub fn describe(rep: &SweepReport) -> String {
    if rep.removed_sessions == 0 && rep.truncated == 0 {
        return String::new();
    }
    format!(
        "session retention: {} session(s) removed, {} transcript(s) truncated, {} freed",
        rep.removed_sessions,
        rep.truncated,
        human_bytes(rep.freed_bytes)
    )
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

    /// Build a temp sessions dir with a few synthetic transcripts.
    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("pir_retention_test_{tag}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Fixture { root }
        }

        /// A transcript whose content is `lines` complete newline-terminated
        /// records, then aged by `days`.
        fn transcript(&self, name: &str, lines: usize, days: u64) -> PathBuf {
            let p = self.root.join(name);
            let body: String = (0..lines).map(|i| format!("{{\"n\":{i}}}\n")).collect();
            std::fs::write(&p, body).unwrap();
            age(&p, days);
            p
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Backdate a path's mtime (and the mtime of the files inside a dir).
    fn age(p: &Path, days: u64) {
        let t = SystemTime::now() - Duration::from_secs(days * 24 * 3600 + 60);
        set_mtime(p, t);
        if p.is_dir() && let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                age(&e.path(), days);
            }
        }
    }

    #[cfg(unix)]
    fn set_mtime(p: &Path, t: SystemTime) {
        use std::os::unix::ffi::OsStrExt;
        let secs = t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let times = [libc::timespec { tv_sec: secs, tv_nsec: 0 }, libc::timespec { tv_sec: secs, tv_nsec: 0 }];
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        unsafe {
            libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0);
        }
    }
    #[cfg(not(unix))]
    fn set_mtime(_p: &Path, _t: SystemTime) {}

    #[test]
    fn removes_a_session_older_than_the_ttl_with_its_sidecars() {
        let f = Fixture::new("ttl");
        let t = f.transcript("pir-20260101-1200-sh1.jsonl", 3, 90);
        // Sidecars belonging to that session.
        for ext in [".status.json", ".goal.json", ".model", ".checkpoints.json"] {
            let s = f.root.join(format!("pir-20260101-1200-sh1{ext}"));
            std::fs::write(&s, b"x").unwrap();
            age(&s, 90);
        }
        // A fresh session that must survive.
        let keep = f.transcript("pir-20260101-1300-sh2.jsonl", 3, 1);

        let rep = sweep_dir(&f.root, None);
        assert_eq!(rep.removed_sessions, 1, "exactly the stale session goes");
        assert!(!t.exists(), "stale transcript must be gone");
        assert!(!f.root.join("pir-20260101-1200-sh1.status.json").exists(), "sidecar must go too");
        assert!(!f.root.join("pir-20260101-1200-sh1.goal.json").exists(), "sidecar must go too");
        assert!(keep.exists(), "fresh session must survive");
    }

    #[test]
    fn keeps_a_stale_sidecar_when_its_transcript_is_fresh() {
        let f = Fixture::new("orphan");
        // Fresh transcript, stale sidecar: the sidecar must NOT be deleted,
        // or a live session loses its status/goal data.
        let t = f.transcript("pir-live.jsonl", 3, 1);
        let s = f.root.join("pir-live.status.json");
        std::fs::write(&s, b"{}").unwrap();
        age(&s, 90);

        let rep = sweep_dir(&f.root, None);
        assert_eq!(rep.removed_sessions, 0);
        assert!(t.exists(), "live transcript survives");
        assert!(s.exists(), "sidecar of a live session survives even if stale");
    }

    #[test]
    fn truncates_an_oversized_transcript_keeping_head_and_tail() {
        let f = Fixture::new("trunc");
        // 400 records; cap forces truncation. Age it 0 days so the TTL branch
        // does not fire and only the size cap applies.
        let p = f.transcript("pir-big.jsonl", 400, 0);
        let before = std::fs::metadata(&p).unwrap().len();
        let cap: u64 = 200; // deliberately tiny, well under the body size

        let rep = sweep_dir_cap(&f.root, None, cap);
        assert_eq!(rep.truncated, 1, "oversized transcript must be truncated");

        let after_body = std::fs::read_to_string(&p).unwrap();
        assert!(after_body.len() < before as usize, "body must have shrunk");
        // The tail survives: the last record is still present.
        assert!(after_body.contains("\"n\":399}"), "the tail must be preserved");
        // The head survives, in the .head sidecar.
        let head = std::fs::read_to_string(f.root.join("pir-big.head")).unwrap();
        assert!(head.contains("\"n\":0}"), "the head must be preserved in .head");
    }

    #[test]
    fn truncated_transcript_stays_line_aligned() {
        let f = Fixture::new("align");
        let p = f.transcript("pir-align.jsonl", 200, 0);
        let cap: u64 = 137; // not a multiple of the record size on purpose
        let _ = sweep_dir_cap(&f.root, None, cap);
        let body = std::fs::read_to_string(&p).unwrap();
        // Every retained line must be a whole JSON record — a partially cut
        // line would make the transcript unparseable.
        for line in body.lines() {
            assert!(line.starts_with('{') && line.ends_with('}'), "partial line: {line:?}");
        }
    }

    #[test]
    fn the_current_session_transcript_is_never_swept() {
        let f = Fixture::new("keepcurrent");
        let t = f.transcript("pir-current.jsonl", 3, 90); // very stale
        let rep = sweep_dir(&f.root, Some(&t));
        assert_eq!(rep.removed_sessions, 0);
        assert!(t.exists(), "the live session's transcript must never be swept");
    }

    #[test]
    fn keep_env_disables_the_sweep() {
        let f = Fixture::new("keepenv");
        let t = f.transcript("pir-old.jsonl", 3, 90);
        // SAFETY: test-local env mutation.
        unsafe { std::env::set_var("PIR_SESSION_KEEP", "1") };
        let rep = sweep_dir(&f.root, None);
        unsafe { std::env::remove_var("PIR_SESSION_KEEP") };
        assert_eq!(rep.removed_sessions, 0);
        assert!(t.exists());
    }

    /// `sweep_dir` with an explicit cap, so the size branch can be tested
    /// without writing a 16 MB file.
    fn sweep_dir_cap(dir: &Path, keep: Option<&Path>, cap: u64) -> SweepReport {
        // SAFETY: test-local env mutation, restored immediately.
        unsafe { std::env::set_var("PIR_SESSION_MAX_BYTES", cap.to_string()) };
        let rep = sweep_dir(dir, keep);
        unsafe { std::env::remove_var("PIR_SESSION_MAX_BYTES") };
        rep
    }

    #[test]
    fn unrelated_files_in_the_sessions_dir_are_never_touched() {
        let f = Fixture::new("unrelated");
        let p = f.root.join("notes.txt");
        std::fs::write(&p, b"keep me").unwrap();
        age(&p, 90);
        let q = f.root.join("README");
        std::fs::write(&q, b"keep me too").unwrap();
        age(&q, 90);

        let rep = sweep_dir(&f.root, None);
        assert_eq!(rep.removed_sessions, 0);
        assert!(p.exists(), "a plain .txt must never be swept");
        assert!(q.exists(), "a plain README must never be swept");
    }
}
