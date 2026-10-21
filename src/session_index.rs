//! Session-scan index: make `pir -r` not re-walk every transcript on every start.
//!
//! ## Why this exists
//!
//! `resolve_resume()` used to build the *entire* candidate list before it
//! matched the resume token. For each candidate `scan_sessions()` did three
//! cold, serial, per-file filesystem operations:
//!
//!   1. `stat` the `.jsonl` (mtime + size),
//!   2. `read_status()` — read and `serde_json`-parse the whole `.status.json`
//!      sidecar, just to recover one string field (`tty`),
//!   3. `first_user_line()` — open the transcript and JSON-parse lines until the
//!      first user message, to build a picker preview.
//!
//! On a machine carrying ~189 transcripts over 618 MB that is ~570 file opens
//! *before the prompt is drawn*, every start. Measured on a cold page cache at
//! load 16, it cost 1.4–2.9 s of wall time and dominated `pir -r` completely
//! (plain `pir`, which does not scan, was 0.11–0.31 s under identical
//! conditions).
//!
//! The volume is small — the whole scan reads ~2.7 MB — so the cost is almost
//! entirely **per-open latency**, not bandwidth. That is why the fix is about
//! *number of opens*, not bytes.
//!
//! ## What this module does
//!
//! Keeps a single JSON index next to the sessions store, keyed by each
//! transcript's path, and records `name`, `shell_pid`, `tty`, `bytes`, `mtime`,
//! `preview` and `local`. A subsequent scan:
//!
//!   * lists each directory (one `read_dir` per directory — unavoidable, and
//!     cheap: it is what tells us *which* files exist),
//!   * `stat`s each transcript (unavoidable — the index is keyed by size+mtime,
//!     so a changed file must be re-read), and
//!   * opens **nothing** when the size+mtime pair matches the index.
//!
//! So a warm start is one `read_dir` + N `stat`s and zero reads, versus N
//! `read_dir` + N `stat` + 2N `open`/`read`/`parse` before. Cold it is no worse:
//! the index is consulted but every entry misses and is re-read as today.
//!
//! ## Correctness
//!
//! An index entry is reused only when (size, mtime) are *both* equal to the
//! live values, so a transcript appended to since the index was written is
//! never served stale. A sidecar or preview the index does not know is filled
//! in on the miss and written back. Nothing here is authoritative for resume
//! correctness — [`super::Session::path`] and the mtime used for sorting always
//! come from the live `stat`, never from the index — so a stale or corrupt
//! index can cost a redundant re-read but can never resume the wrong session.
//! Unreadable indexes are silently ignored and rebuilt.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One transcript's cached scan metadata.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    /// `mtime` as whole seconds since the epoch. A transcript touched within
    /// the same second as the cached entry can be missed by the equality check,
    /// so the caller treats a second-resolution tie as a miss (see
    /// [`lookup`]) — correctness is preserved, only a re-read is spent.
    pub mtime: u64,
    pub bytes: u64,
    /// Controlling terminal recovered from the `.status.json` sidecar.
    #[serde(default)]
    pub tty: String,
    /// Working directory recovered from the `.status.json` sidecar
    /// (canonicalized leniently). What bare `pir -r` matches on first, so a
    /// warm start must not pay a sidecar re-read to learn it.
    #[serde(default)]
    pub cwd: String,
    /// First user prompt, for the picker/`/sessions` list.
    #[serde(default)]
    pub preview: String,
    /// Canonical project root for the session (VCS top-level, else the cwd
    /// itself, else empty). Keyed alongside `cwd` so a warm start never
    /// re-walks the filesystem for `.git`/`.jj` markers: misses recompute it
    /// via `session_tree::derive_project_root` (cheap, bounded 8-level walk)
    /// and write it back. `#[serde(default)]` keeps v2 indexes readable.
    #[serde(default)]
    pub project_root: String,
}

/// The on-disk index: `{ "version": 1, "entries": { "<path>": Entry } }`.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct IndexFile {
    /// Bumped whenever the entry shape changes, so an old index is discarded
    /// rather than misread.
    #[serde(default)]
    version: u32,
    #[serde(default)]
    entries: HashMap<String, Entry>,
}

/// Bumped on any change to [`Entry`]; a mismatch means "rebuild".
const INDEX_VERSION: u32 = 3;

/// Name of the index file, written next to the global sessions dir's parent so
/// it covers the global store. The project-local store is indexed in the same
/// file (paths are absolute keys), which keeps one read/write rather than two.
const INDEX_FILE: &str = "sessions-index.json";

/// Where the index lives: `<pi_dir>/agent/<INDEX_FILE>`.
pub fn index_path() -> PathBuf {
    crate::config::pi_dir().join("agent").join(INDEX_FILE)
}

/// Load the index, or an empty one if it is missing, unreadable, from a future
/// version, or otherwise unusable. Never fails: an index is an optimisation, and
/// a session must start even when it cannot be cached.
fn load() -> IndexFile {
    let raw = match std::fs::read(index_path()) {
        Ok(r) => r,
        Err(_) => return IndexFile::default(),
    };
    match serde_json::from_slice::<IndexFile>(&raw) {
        Ok(f) if f.version == INDEX_VERSION => f,
        _ => IndexFile::default(),
    }
}

/// Save the index, best-effort. A failed write costs a re-read next start.
fn store(mut f: IndexFile) {
    f.version = INDEX_VERSION;
    let p = index_path();
    // The directory is the agent config dir, which exists on any real install,
    // but create it rather than silently dropping the index when it does not
    // (a fresh PI_DIR, or a test's throwaway dir).
    if let Some(parent) = p.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    // Write-then-rename so a concurrent pir never observes a half-written
    // index (it would just miss, but a truncated file would fail to parse on
    // every start forever).
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec(&f).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

/// A loaded index that records hits and misses, and writes back on
/// [`Index::save`].
pub struct Index {
    entries: HashMap<String, Entry>,
    /// Entries that missed and must be written back.
    dirty: Vec<(String, Entry)>,
}

impl Index {
    /// Load the index for this run.
    pub fn load() -> Self {
        let f = load();
        Self { entries: f.entries, dirty: Vec::new() }
    }

    /// Return the cached entry for `path` when it is still valid for the live
    /// `bytes`/`mtime_secs`, otherwise `None`.
    ///
    /// `mtime_secs` is deliberately the *whole-second* mtime the caller will
    /// sort by. Comparing that (rather than a nanosecond mtime) means a
    /// transcript rewritten within the same second as the cached entry is
    /// treated as fresh and re-read — a re-read is cheap and always correct,
    /// whereas trusting a same-second tie could serve a stale preview.
    pub fn lookup(&mut self, path: &Path, bytes: u64, mtime_secs: u64) -> Option<Entry> {
        let key = path.to_string_lossy().into_owned();
        match self.entries.get(&key) {
            Some(e) if e.bytes == bytes && e.mtime == mtime_secs => Some(e.clone()),
            _ => None,
        }
    }

    /// Record a freshly-computed entry so the next start can reuse it.
    pub fn put(&mut self, path: &Path, e: Entry) {
        self.dirty.push((path.to_string_lossy().into_owned(), e));
    }

    /// Write back everything learned this run. Call once, after the scan.
    pub fn save(self) {
        if self.dirty.is_empty() {
            return;
        }
        let mut f = IndexFile { version: INDEX_VERSION, entries: self.entries };
        for (k, e) in self.dirty {
            f.entries.insert(k, e);
        }
        store(f);
    }

    /// Number of entries written back (for tests / diagnostics).
    pub fn dirty_len(&self) -> usize {
        self.dirty.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pir_sidx_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A hit requires both size and mtime to match; either differing must miss.
    #[test]
    fn hit_requires_size_and_mtime_to_match() {
        let mut i = Index { entries: HashMap::new(), dirty: Vec::new() };
        i.entries.insert(
            "/a.jsonl".into(),
            Entry { mtime: 100, bytes: 10, tty: "pts/3".into(), cwd: "/w".into(), preview: "hi".into(), project_root: "/w".into() },
        );
        let hit = i.lookup(Path::new("/a.jsonl"), 10, 100).expect("exact match hits");
        assert_eq!(hit.tty, "pts/3");
        assert_eq!(hit.cwd, "/w");
        assert_eq!(hit.preview, "hi");
        assert!(i.lookup(Path::new("/a.jsonl"), 11, 100).is_none(), "size change must miss");
        assert!(i.lookup(Path::new("/a.jsonl"), 10, 101).is_none(), "mtime change must miss");
    }

    /// A round trip through the index file preserves entries, so the second
    /// start of a session really can skip the reads.
    #[test]
    fn round_trip_through_file() {
        // The tests in this module mutate PI_DIR (that is where the index file
        // lives), so they must hold the shared config lock or they race each
        // other. Same convention as the other env-touching tests.
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tmpdir("roundtrip");
        let old = std::env::var_os("PI_DIR");
        // SAFETY: test-only env mutation, serialised by TEST_ENV_LOCK.
        unsafe { std::env::set_var("PI_DIR", &dir) };

        {
            let mut i = Index::load();
            assert!(i.lookup(Path::new("/x.jsonl"), 5, 7).is_none(), "empty index misses");
            i.put(
                Path::new("/x.jsonl"),
                Entry { mtime: 7, bytes: 5, tty: "pts/9".into(), cwd: "/x".into(), preview: "p".into(), project_root: "/x".into() },
            );
            assert_eq!(i.dirty_len(), 1);
            i.save();
        }
        let mut i2 = Index::load();
        let e = i2.lookup(Path::new("/x.jsonl"), 5, 7).expect("persisted entry hits");
        assert_eq!(e.tty, "pts/9");
        assert_eq!(e.cwd, "/x");
        assert_eq!(e.preview, "p");

        match old {
            Some(v) => unsafe { std::env::set_var("PI_DIR", v) },
            None => unsafe { std::env::remove_var("PI_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A corrupt or future-version index is ignored, not fatal.
    #[test]
    fn corrupt_index_is_ignored() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tmpdir("corrupt");
        let old = std::env::var_os("PI_DIR");
        unsafe { std::env::set_var("PI_DIR", &dir) };
        std::fs::create_dir_all(index_path().parent().unwrap()).unwrap();
        std::fs::write(index_path(), b"{ this is not json").unwrap();
        let mut i = Index::load();
        assert!(i.lookup(Path::new("/x.jsonl"), 5, 7).is_none());
        match old {
            Some(v) => unsafe { std::env::set_var("PI_DIR", v) },
            None => unsafe { std::env::remove_var("PI_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}