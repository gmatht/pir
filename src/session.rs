//! Unfinished-conversation tracking.
//!
//! Every interactive / one-shot / background session gets a sidecar
//! `<session>.status.json` written by the `Agent` as it runs:
//!
//!   * `active`      — a turn is currently in flight (records the live process
//!     pid that owns the session);
//!   * `completed`   — a turn finished cleanly (the conversation was brought to
//!     a natural stopping point);
//!   * `interrupted` — a turn ended early (user cancel, a network/provider
//!     error, the token budget was hit, …).
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
    /// The user has explicitly marked the conversation finished (via `/finished`
    /// or `f`/`F` in the `pir -r` picker). A session stays *unfinished* — shown
    /// in `/unfinished` and the resume picker — until this is set, even after a
    /// turn ends cleanly. Finished sessions are excluded from the unfinished
    /// list; they're still reopenable with `/fg` or `pir -r <token>` if wanted.
    Finished,
}

impl SessionStatus {
    pub fn label(self) -> &'static str {
        match self {
            SessionStatus::Active => "active",
            SessionStatus::Completed => "completed",
            SessionStatus::Interrupted => "interrupted",
            SessionStatus::Finished => "finished",
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
    /// Build stamp of the `pir` binary that wrote this status (e.g.
    /// `0.1.0-<short-sha>`). Lets `scan_unfinished` distinguish statuses
    /// written by this run from stale ones left by an older build (which is
    /// why a pile of `pid=0`/`interrupted` files from a previous binary used
    /// to flood `/unfinished`). Empty when unknown. A missing/old stamp is not
    /// fatal — a session is still resumable; the stamp only informs display.
    #[serde(default)]
    pub built: String,
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

/// Path of the conversation-title sidecar (`<log>.title`). The throttled
/// "light model" writes a short, human-readable name for the conversation
/// here so it can be shown in `/sessions` / `/unfinished` without re-reading
/// and summarizing the whole transcript (and without ever touching stdout —
/// the light model runs silently in the background).
pub fn title_path(log: &Path) -> PathBuf {
    log.with_extension("title")
}

/// Write a generated conversation title to the `.title` sidecar. Best-effort:
/// a missing/empty log path (one-shot sessions that opted out of a transcript)
/// is a no-op. The title is the bare text the light model produced — callers
/// are responsible for stripping quotes/leading "Title:" prefixes before
/// passing it here.
pub fn write_title(log: &Path, title: &str) {
    if log.as_os_str().is_empty() {
        return;
    }
    let title = title.trim().to_string();
    if title.is_empty() {
        return;
    }
    let _ = fs::write(title_path(log), title);
}

/// Read the cached conversation title (if any) from the `.title` sidecar.
/// Returns `None` when no title has been generated yet or the file is absent.
pub fn read_title(log: &Path) -> Option<String> {
    let raw = fs::read_to_string(title_path(log)).ok()?;
    let t = raw.trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}

/// Path of the turn-outcome verdict sidecar (`<log>.verdict`). The throttled
/// "light" model writes a short outcome label (`complete` / `waiting` /
/// `retry` / `blocked` / `error`) here after each turn, so `/sessions` and the
/// resume picker can show at a glance how a conversation ended — without
/// re-summarizing the whole transcript, and without ever touching stdout.
pub fn verdict_path(log: &Path) -> PathBuf {
    log.with_extension("verdict")
}

/// Write a turn-outcome verdict to the `.verdict` sidecar. Best-effort: a
/// missing/empty log path or empty verdict is a no-op.
pub fn write_verdict(log: &Path, verdict: &str) {
    if log.as_os_str().is_empty() {
        return;
    }
    let verdict = verdict.trim().to_string();
    if verdict.is_empty() {
        return;
    }
    let _ = fs::write(verdict_path(log), verdict);
}

/// Read the cached turn-outcome verdict (if any) from the `.verdict` sidecar.
/// Returns `None` when no verdict has been generated yet or the file is absent.
pub fn read_verdict(log: &Path) -> Option<String> {
    let raw = fs::read_to_string(verdict_path(log)).ok()?;
    let v = raw.trim().to_string();
    if v.is_empty() { None } else { Some(v) }
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
        built: build_stamp(),
        last_prompt: last_prompt.to_string(),
        goal_pending,
        reason: reason.to_string(),
    };
    if let Ok(s) = serde_json::to_string_pretty(&meta) {
        let _ = fs::write(status_path(log), s);
    }
}

/// Best-effort build stamp of the currently-running binary, e.g.
/// `0.1.0-abc1234`. Used to tag status sidecars so stale statuses left by an
/// older build can be recognized. Reads the `PIR_BUILD_STAMP` env var (set by
/// `deploy.sh`/cargo build wrapper when available), else the crate version
/// from `CARGO_PKG_VERSION`. Never fails — falls back to an empty string.
pub fn build_stamp() -> String {
    if let Ok(s) = std::env::var("PIR_BUILD_STAMP") {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    env!("CARGO_PKG_VERSION").to_string()
}

pub fn read_status(log: &Path) -> Option<SessionMeta> {
    let raw = fs::read_to_string(status_path(log)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Mark `log` as explicitly finished. Used by `/finished` and the `f`/`F` key in
/// the `pir -r` picker. Sets status to `Finished` (clearing any pending-goal
/// flag) so the session drops out of `/unfinished` and the resume picker. A
/// missing/empty log path is a no-op.
pub fn mark_finished(log: &Path) {
    if log.as_os_str().is_empty() {
        return;
    }
    write_status(log, SessionStatus::Finished, std::process::id(), "", false, "");
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

        // A session stays "unfinished" — shown in `/unfinished` and the `pir -r`
        // picker — until the user *explicitly* marks it finished (via `/finished`
        // or `f`/`F` in the picker). A clean `Completed` end-state no longer
        // removes it: only an explicit `Finished` status does. This way a
        // conversation that ended at a natural stop is still reopenable/a resumable
        // until the user decides it's done.
        let unfinished = !live_client && meta.status != SessionStatus::Finished;
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
        } else if meta.goal_pending {
            "goal still in progress".to_string()
        } else {
            "completed — mark finished with /finished or `f` in pir -r".to_string()
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
    out.sort_by_key(|a| std::cmp::Reverse(a.mtime));
    out
}

fn first_user_line(path: &Path) -> String {
    if let Ok(f) = fs::File::open(path) {
        for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
                && v.get("role").and_then(|r| r.as_str()) == Some("user")
                    && let Some(txt) = v
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
        out.push('\n');
    }
    out.push_str(&term::dim("resume with: /resume <index|path-fragment>  ·  mark one done with /finished or `f` in pir -r"));
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

/// A compact preview of a session's conversation, used by the interactive
/// `pir -r` session picker (the arrow-key picker) to show, for the highlighted
/// session, the first user prompt, the last user prompt, and the tail of the
/// model's last thinking + response. Cheap: a single linear scan of the
/// (typically small) session log.
pub struct SessionPreview {
    pub turns: usize,
    pub first_prompt: String,
    pub last_prompt: String,
    pub last_thinking: String,
    pub last_output: String,
}

/// Produce a [`SessionPreview`] for `path` by scanning its JSONL transcript.
/// Missing/empty logs yield an all-empty preview (the picker still shows the
/// session's name + tag). Tolerant of malformed lines.
///
/// This is a **hot path** — the picker calls it for every row the user arrows
/// onto — so a large transcript (multi-MB, thousands of lines) must not cost a
/// full `serde_json` parse of every byte.
///
/// The whole cost is avoided by a **byte-level prefilter**: a line is only fed
/// to `serde_json` if its raw bytes look like a turn that actually carries
/// `text`/`thinking`. In real transcripts the bulk of the bytes live in
/// `tool_result` / `tool_use` payloads (measured: 89% of a 5.4 MB log), which
/// never contribute to the preview — skipping them by substring test costs a
/// SIMD-accelerated scan instead of an allocation-heavy parse.
///
/// Deliberately a *single* pass over the file. A tail-window shortcut for the
/// `last_*` fields is tempting but **unsound**: a session can end with a
/// megabyte of tool chatter after its final prompt (measured: 0.97 MB in one
/// corro transcript), so any fixed window silently reports the wrong "last
/// prompt". The prefilter already makes the full scan cheap enough that the
/// extra pass is not worth the wrong answers.
pub fn read_preview(path: &Path) -> SessionPreview {
    let mut turns = 0usize;
    let mut first_prompt = String::new();
    let mut last_prompt = String::new();
    let mut last_thinking = String::new();
    let mut last_output = String::new();

    if let Ok(f) = fs::File::open(path) {
        // One large buffer: transcripts have multi-hundred-KB lines, and the
        // default 8 KB BufReader would re-`read` for each chunk of a long line.
        for line in std::io::BufReader::with_capacity(256 * 1024, f)
            .lines()
            .map_while(Result::ok)
        {
            if !worth_parsing(&line) {
                continue;
            }
            let Some(v) = parse_line(&line) else { continue };
            match v.get("role").and_then(|r| r.as_str()) {
                Some("user") => {
                    let text = user_text(&v);
                    if !text.is_empty() {
                        turns += 1;
                        if first_prompt.is_empty() {
                            first_prompt = text.clone();
                        }
                        last_prompt = text;
                    }
                }
                Some("assistant") => {
                    let (thinking, text) = assistant_text(&v);
                    if !thinking.is_empty() {
                        last_thinking = thinking;
                    }
                    if !text.is_empty() {
                        last_output = text;
                    }
                }
                _ => {}
            }
        }
    }

    // A transcript whose last user entry carried no text (e.g. it ended on a
    // tool round-trip) still needs a prompt in the pane.
    if last_prompt.is_empty() {
        last_prompt = first_prompt.clone();
    }

    SessionPreview {
        turns,
        first_prompt,
        last_prompt,
        last_thinking,
        last_output,
    }
}

/// Cheap prefilter: does this raw JSONL line possibly contribute to a preview?
///
/// The preview only reads `role` + `blocks[].type == text|thinking`. A line with
/// no `role` field, or one that is purely `tool_result`/`tool_use` payload (the
/// majority of the bytes in a long transcript), can be skipped without a parse.
/// The test is byte-level so it costs a fast substring scan rather than
/// building a `serde_json::Value`.
fn worth_parsing(line: &str) -> bool {
    if line.is_empty() {
        return false;
    }
    let b = line.as_bytes();
    // Every transcript entry we care about has a `role` key.
    if !contains(b, b"\"role\"") {
        return false;
    }
    // A preview only ever surfaces text or thinking blocks; a line carrying
    // neither (a bare tool_result / tool_use record) is skipped. Note
    // `"type":"text"` also matches inside `"type":"text_delta"`-style payloads,
    // which is fine — those are still cheap to parse and rare.
    contains(b, b"\"text\"") || contains(b, b"\"thinking\"")
}

/// Substring search over raw bytes (no UTF-8 decoding, no allocation).
///
/// Deliberately not `windows(..).any(..)`: that is a naive O(n·m) scan which
/// dominates the profile on multi-KB `tool_result` lines. `str::find` on a
/// byte-slice-as-str is a memchr-accelerated (Two-Way / SIMD) search, so the
/// prefilter stays a small fraction of the old parse cost. Bytes are compared
/// as `u8` so this is valid for non-UTF-8 content too.
/// Substring search over raw bytes, with no UTF-8 assumption and no allocation.
///
/// Anchors on the first byte with [`slice::iter().position()`], which compiles
/// to a `memchr`-style scan, then compares the remainder with `==` (a
/// length-known slice compare, autovectorized). A naive
/// `hay.windows(n).any(|w| w == needle)` does the same comparisons but through
/// a per-window closure; this form keeps the scan branch-light, which matters
/// because it runs over every byte of a multi-MB transcript.
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if hay.len() < needle.len() {
        return false;
    }
    let first = needle[0];
    let n = needle.len();
    let mut i = 0;
    while i + n <= hay.len() {
        match hay[i..hay.len() - n + 1].iter().position(|&b| b == first) {
            Some(off) => {
                let start = i + off;
                if &hay[start..start + n] == needle {
                    return true;
                }
                i = start + 1;
            }
            None => return false,
        }
    }
    false
}

fn parse_line(line: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(line).ok()
}

/// The joined `text` blocks of a user entry, trimmed.
fn user_text(v: &serde_json::Value) -> String {
    let Some(arr) = v.get("blocks").and_then(|b| b.as_array()) else {
        return String::new();
    };
    arr.iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<&str>>()
        .join("\n")
        .trim()
        .to_string()
}

/// The joined `thinking` and `text` blocks of an assistant entry, trimmed.
fn assistant_text(v: &serde_json::Value) -> (String, String) {
    let mut thinking = String::new();
    let mut text = String::new();
    let Some(arr) = v.get("blocks").and_then(|b| b.as_array()) else {
        return (String::new(), String::new());
    };
    for b in arr {
        match b.get("type").and_then(|t| t.as_str()) {
            Some("thinking") => {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    thinking.push_str(t);
                    thinking.push('\n');
                }
            }
            Some("text") => {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    text.push_str(t);
                    text.push('\n');
                }
            }
            _ => {}
        }
    }
    (thinking.trim().to_string(), text.trim().to_string())
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
    fn finished_session_drops_out_via_status() {
        let dir = std::env::temp_dir().join(format!("pir_fin_tests_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let log = dir.join("fin.jsonl");
        std::fs::write(&log, "{\"role\":\"user\",\"blocks\":[{\"type\":\"text\",\"text\":\"hi\"}]}\n").unwrap();
        // A completed session still carries a non-finished status.
        write_status(&log, SessionStatus::Completed, 1234, "hi", false, "");
        assert_ne!(read_status(&log).unwrap().status, SessionStatus::Finished);
        // An explicit finished mark flips the status so downstream scans exclude it.
        mark_finished(&log);
        assert_eq!(read_status(&log).unwrap().status, SessionStatus::Finished);

        let _ = std::fs::remove_file(status_path(&log));
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_dir(&dir);
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    fn write(dir: &Path, name: &str, entries: &[serde_json::Value]) -> PathBuf {
        let p = dir.join(name);
        let mut s = String::new();
        for e in entries {
            s.push_str(&serde_json::to_string(e).unwrap());
            s.push('\n');
        }
        std::fs::write(&p, s).unwrap();
        p
    }

    fn entry(role: &str, blocks: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({ "role": role, "blocks": blocks })
    }
    fn text(t: &str) -> serde_json::Value {
        serde_json::json!({ "type": "text", "text": t })
    }
    fn thinking(t: &str) -> serde_json::Value {
        serde_json::json!({ "type": "thinking", "text": t })
    }
    fn tool_result(t: &str) -> serde_json::Value {
        serde_json::json!({ "type": "tool_result", "content": t })
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pir_preview_{}_{}", tag, std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    #[test]
    fn preview_reads_basic_fields() {
        let d = tmpdir("basic");
        let p = write(&d, "a.jsonl", &[
            entry("user", vec![text("first question")]),
            entry("assistant", vec![thinking("hmm"), text("first answer")]),
            entry("user", vec![text("second question")]),
            entry("assistant", vec![thinking("thinking two"), text("second answer")]),
        ]);
        let pr = read_preview(&p);
        assert_eq!(pr.turns, 2);
        assert_eq!(pr.first_prompt, "first question");
        assert_eq!(pr.last_prompt, "second question");
        assert_eq!(pr.last_thinking, "thinking two");
        assert_eq!(pr.last_output, "second answer");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn preview_skips_tool_result_payloads() {
        // A tool_result carries "text" only inside content, never a `text`
        // block, so it must not be mistaken for a prompt/output.
        let d = tmpdir("tools");
        let p = write(&d, "b.jsonl", &[
            entry("user", vec![text("q1")]),
            entry("assistant", vec![
                serde_json::json!({"type":"tool_use","id":"t1","name":"bash","input":{}}),
                tool_result("{\"role\":\"user\",\"blocks\":[{\"type\":\"text\",\"text\":\"DECOY\"}]}"),
            ]),
            entry("assistant", vec![text("a1")]),
        ]);
        let pr = read_preview(&p);
        assert_eq!(pr.turns, 1, "tool_result must not inflate the turn count");
        assert_eq!(pr.first_prompt, "q1");
        assert_eq!(pr.last_output, "a1");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn preview_finds_last_turn_in_large_log_with_trailing_tool_chatter() {
        // Regression: a real transcript can end with ~1 MB of tool chatter
        // *after* its final prompt, so any fixed tail window reports the wrong
        // "last prompt". The prefiltered full scan must stay exact.
        let d = tmpdir("tail");
        let big = "x".repeat(4096);
        let mut entries = vec![entry("user", vec![text("the very first prompt")])];
        entries.push(entry("user", vec![text("last prompt here")]));
        entries.push(entry("assistant", vec![thinking("late thinking"), text("late answer")]));
        // ~1 MB of tool round-trips *after* that final exchange.
        for i in 0..250 {
            entries.push(entry("assistant", vec![
                serde_json::json!({"type":"tool_use","id":format!("t{i}"),"name":"bash","input":{}}),
            ]));
            entries.push(entry("user", vec![tool_result(&big)]));
        }
        let p = write(&d, "c.jsonl", &entries);
        let bytes = std::fs::metadata(&p).unwrap().len() as usize;
        assert!(bytes > 1_000_000, "fixture must be large ({bytes})");

        let pr = read_preview(&p);
        assert_eq!(pr.first_prompt, "the very first prompt");
        assert_eq!(pr.last_prompt, "last prompt here");
        assert_eq!(pr.last_thinking, "late thinking");
        assert_eq!(pr.last_output, "late answer");
        assert_eq!(pr.turns, 2, "tool_result-only user lines must not count as turns");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn preview_handles_empty_and_malformed() {
        let d = tmpdir("empty");
        let p = d.join("empty.jsonl");
        std::fs::write(&p, "").unwrap();
        let pr = read_preview(&p);
        assert_eq!(pr.turns, 0);
        assert!(pr.first_prompt.is_empty());

        let p2 = d.join("malformed.jsonl");
        std::fs::write(&p2, "not json\n{\"role\":\"user\"\n{\"role\":\"user\",\"blocks\":[{\"type\":\"text\",\"text\":\"ok\"}]}\n").unwrap();
        let pr2 = read_preview(&p2);
        assert_eq!(pr2.turns, 1);
        assert_eq!(pr2.first_prompt, "ok");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&p2);
    }

    #[test]
    fn prefilter_accepts_and_rejects() {
        assert!(worth_parsing("{\"role\":\"user\",\"blocks\":[{\"type\":\"text\",\"text\":\"hi\"}]}"));
        assert!(worth_parsing("{\"role\":\"assistant\",\"blocks\":[{\"type\":\"thinking\",\"text\":\"h\"}]}"));
        assert!(!worth_parsing("{\"role\":\"assistant\",\"blocks\":[{\"type\":\"tool_result\",\"content\":\"z\"}]}"));
        assert!(!worth_parsing(""));
        assert!(!worth_parsing("garbage"));
    }
}


#[cfg(test)]
mod preview_differential_tests {
    //! Self-contained differential test: the optimised `read_preview` (byte
    //! prefilter, single pass) must agree exactly with a deliberately naive
    //! reference that parses every line. Generated from a seeded PRNG so the
    //! case mix (tool chatter, huge lines, trailing tool rounds, malformed
    //! lines) is stable across runs.
    use super::*;

    /// The original, unoptimised semantics — parse every line, no prefilter.
    fn reference(path: &Path) -> SessionPreview {
        let mut turns = 0usize;
        let mut first_prompt = String::new();
        let mut last_prompt = String::new();
        let mut last_thinking = String::new();
        let mut last_output = String::new();
        if let Ok(f) = fs::File::open(path) {
            for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                let role = v.get("role").and_then(|r| r.as_str()).unwrap_or("");
                let blocks = v.get("blocks").and_then(|b| b.as_array());
                if role == "user" {
                    if let Some(arr) = blocks {
                        let text = arr
                            .iter()
                            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<&str>>()
                            .join("\n")
                            .trim()
                            .to_string();
                        if !text.is_empty() {
                            if first_prompt.is_empty() {
                                first_prompt = text.clone();
                            }
                            last_prompt = text;
                            turns += 1;
                        }
                    }
                } else if role == "assistant"
                    && let Some(arr) = blocks
                {
                    let mut thinking = String::new();
                    let mut text = String::new();
                    for b in arr {
                        match b.get("type").and_then(|t| t.as_str()) {
                            Some("thinking") => {
                                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                                    thinking.push_str(t);
                                    thinking.push('\n');
                                }
                            }
                            Some("text") => {
                                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                                    text.push_str(t);
                                    text.push('\n');
                                }
                            }
                            _ => {}
                        }
                    }
                    let thinking = thinking.trim().to_string();
                    let text = text.trim().to_string();
                    if !thinking.is_empty() {
                        last_thinking = thinking;
                    }
                    if !text.is_empty() {
                        last_output = text;
                    }
                }
            }
        }
        if last_prompt.is_empty() {
            last_prompt = first_prompt.clone();
        }
        SessionPreview { turns, first_prompt, last_prompt, last_thinking, last_output }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            // xorshift64*: deterministic across platforms/runs.
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    fn synth(rng: &mut Rng, seed_case: usize) -> String {
        let mut out = String::new();
        // A realistic sprinkle of turns; some sessions end on tool rounds,
        // some have multi-hundred-KB lines, some have malformed lines.
        let n_turns = 1 + rng.below(6);
        for t in 0..n_turns {
            let prompt = format!("prompt {seed_case}-{t}");
            out.push_str(&serde_json::json!({
                "role": "user",
                "blocks": [{"type": "text", "text": prompt}]
            }).to_string());
            out.push('\n');

            // assistant turn: thinking + text + a tool_use
            out.push_str(&serde_json::json!({
                "role": "assistant",
                "blocks": [
                    {"type": "thinking", "text": format!("think {seed_case}-{t}")},
                    {"type": "text", "text": format!("answer {seed_case}-{t}")},
                    {"type": "tool_use", "id": format!("c{t}"), "name": "bash", "input": {}}
                ]
            }).to_string());
            out.push('\n');

            // tool result: occasionally enormous, and its `content` may itself
            // contain decoy JSON that looks like a user text entry.
            let big = "y".repeat(rng.below(300 * 1024));
            let decoy = if rng.below(4) == 0 {
                "{\\\"role\\\":\\\"user\\\",\\\"blocks\\\":[{\\\"type\\\":\\\"text\\\",\\\"text\\\":\\\"DECOY\\\"}]}"
            } else {
                ""
            };
            out.push_str(&serde_json::json!({
                "role": "user",
                "blocks": [{"type": "tool_result", "tool_use_id": format!("c{t}"), "content": format!("{big}{decoy}"), "is_error": false}]
            }).to_string());
            out.push('\n');

            if rng.below(6) == 0 {
                out.push_str("{not valid json\n");
            }
        }
        out
    }

    #[test]
    fn optimised_matches_reference_on_synthetic_transcripts() {
        let d = std::env::temp_dir().join(format!("pir_diff_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        for case in 0..24usize {
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (case as u64 + 1));
            let body = synth(&mut rng, case);
            let p = d.join(format!("t{case}.jsonl"));
            std::fs::write(&p, &body).unwrap();

            let want = reference(&p);
            let got = read_preview(&p);

            assert_eq!(got.turns, want.turns, "turns mismatch (case {case})");
            assert_eq!(got.first_prompt, want.first_prompt, "first_prompt mismatch (case {case})");
            assert_eq!(got.last_prompt, want.last_prompt, "last_prompt mismatch (case {case})");
            assert_eq!(got.last_thinking, want.last_thinking, "last_thinking mismatch (case {case})");
            assert_eq!(got.last_output, want.last_output, "last_output mismatch (case {case})");
            let _ = std::fs::remove_file(&p);
        }
        let _ = std::fs::remove_dir(&d);
    }

    #[test]
    fn contains_finds_and_misses() {
        assert!(contains(b"hello world", b"world"));
        assert!(contains(b"world", b"world"));
        assert!(contains(b"aaa", b"a"));
        assert!(contains(b"x", b""));
        assert!(!contains(b"hello", b"world"));
        assert!(!contains(b"hi", b"hello"));
        assert!(!contains(b"", b"x"));
        // Non-UTF-8 bytes must not panic or mis-match.
        assert!(contains(&[0xff, 0x00, 0x41, 0x42], &[0xff, 0x00]));
        assert!(!contains(&[0xff, 0x00, 0x41], &[0xfe, 0x00]));
    }
}
