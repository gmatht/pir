//! Turn-level checkpoints and rewind (Feature 8).
//!
//! `/undo` already reverts a single `write_file`/`edit_file`, by hashing each
//! file's pre-edit contents into a sidecar. That works for an accidental edit
//! but not for the case that actually hurts: a *turn* that went wrong, made
//! three edits and ran a command, and needs unwinding as a unit.
//!
//! A checkpoint here is the set of files a turn touched, snapshotted **once per
//! file per turn** (before the turn's first modification), plus the conversation
//! length at the point the turn started. Rewinding to a checkpoint therefore
//! restores two things that must move together:
//!
//! * the working tree — every file the turn changed goes back to its pre-turn
//!   contents, and files the turn *created* are removed;
//! * the conversation — history is truncated to where the turn began, so the
//!   model doesn't keep reasoning about work that has been undone.
//!
//! Rewinding only one of those is what makes an ordinary undo confusing: the
//! files are back but the model still believes it wrote them.
//!
//! # Persistence
//!
//! The checkpoint index lives next to the session transcript
//! (`<session>.checkpoints.json`) and the file snapshots under
//! `<session>.ckpt/`. That means rewind works across a crash or a `pir -c`
//! resume, which is exactly when you most want it.
//!
//! # Boundaries
//!
//! Only files the *agent* modified through its own file tools are captured.
//! Side effects of `bash` are deliberately out of scope: we cannot know what a
//! shell command touched without a filesystem snapshot facility, and pretending
//! otherwise would make rewind untrustworthy. `bash`-driven changes are reported
//! as such so the operator knows what a rewind did *not* cover.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// How many turns back rewind can reach. Snapshots are full file copies, so an
/// unbounded history would grow without limit; 50 turns is far more than anyone
/// rewinds by hand and keeps a session's checkpoint dir in the low megabytes.
pub const MAX_CHECKPOINTS: usize = 50;

/// One file captured by a checkpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileState {
    /// Absolute path as the agent addressed it.
    pub path: PathBuf,
    /// Snapshot file under the checkpoint dir, or `None` when the file did not
    /// exist before the turn (so a rewind must delete it).
    pub snapshot: Option<PathBuf>,
}

/// A turn boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Monotonic id, shown as `#N` in `/checkpoints` and accepted by `/rewind N`.
    pub id: usize,
    /// Unix epoch seconds when the turn began.
    pub ts: u64,
    /// The user prompt that opened the turn (for the listing).
    pub prompt: String,
    /// Conversation length when the turn began: rewinding truncates `history`
    /// back to this many entries.
    pub history_len: usize,
    /// Files captured, in first-touch order.
    pub files: Vec<FileState>,
    /// Whether the turn ran any `bash` command. Rewind cannot undo those, so the
    /// result says so rather than implying a complete restore.
    #[serde(default)]
    pub ran_bash: bool,
}

/// What a rewind did, for the status line.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RewindReport {
    pub restored: usize,
    pub removed: usize,
    pub failed: Vec<String>,
    /// Turns dropped from the conversation.
    pub history_dropped: usize,
    /// Whether the rewound turn had run shell commands (not undone).
    pub ran_bash: bool,
}

impl RewindReport {
    pub fn render(&self, checkpoint: &Checkpoint) -> String {
        let mut s = format!(
            "rewound to checkpoint #{} ({})",
            checkpoint.id,
            if checkpoint.prompt.is_empty() {
                "(no prompt recorded)".to_string()
            } else {
                clip(&checkpoint.prompt, 60)
            }
        );
        s.push_str(&format!(
            "\n  {} file(s) restored, {} created-file(s) removed",
            self.restored, self.removed
        ));
        if self.history_dropped > 0 {
            s.push_str(&format!(
                "\n  conversation truncated by {} message(s) — the model no longer sees the undone work",
                self.history_dropped
            ));
        }
        if self.ran_bash {
            s.push_str(
                "\n  note: that turn ran shell command(s). Their side effects are NOT undone — \
                 only edits made through the file tools are.",
            );
        }
        for f in &self.failed {
            s.push_str(&format!("\n  FAILED: {f}"));
        }
        s
    }
}

/// The persisted checkpoint history for one session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub checkpoints: Vec<Checkpoint>,
    #[serde(default)]
    next_id: usize,
}

impl Default for Store {
    fn default() -> Self {
        // Ids are 1-based: `#0` in the listing reads like an error, and the
        // allocator below derives the next id from `max(ids) + 1`, so starting
        // at 0 would also make a fresh store's first id ambiguous with an
        // unset value.
        Store { checkpoints: Vec::new(), next_id: 1 }
    }
}

impl Store {
    /// Index path for a session log: `<session>.checkpoints.json`.
    pub fn index_path(log: &Path) -> PathBuf {
        log.with_extension("checkpoints.json")
    }

    /// Snapshot directory for a session log: `<session>.ckpt/`.
    pub fn dir_for(log: &Path) -> PathBuf {
        let mut p = log.to_path_buf();
        let name = p
            .file_name()
            .map(|n| format!("{}.ckpt", n.to_string_lossy()))
            .unwrap_or_else(|| "session.ckpt".to_string());
        p.set_file_name(name);
        p
    }

    /// Load a session's checkpoint history, or `None` when there is no
    /// transcript to key it off (a one-shot run). The distinction matters: a
    /// session with no log has no checkpoint *file*, and saying so is more
    /// honest than reporting an empty history.
    pub fn load_opt(log: Option<&Path>) -> Option<Store> {
        log.map(Self::load)
    }

    /// Load a session's checkpoint history. A missing/corrupt index is an empty
    /// store, never an error — checkpoints are a convenience, not session state
    /// the agent depends on.
    pub fn load(log: &Path) -> Store {
        let mut store: Store = fs::read_to_string(Self::index_path(log))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        store.next_id = store.checkpoints.iter().map(|c| c.id).max().unwrap_or(0) + 1;
        store
    }

    /// Persist the store (atomically: write a temp then rename, so a crash
    /// mid-write leaves the previous index intact rather than a truncated one).
    pub fn save(&self, log: &Path) -> Result<(), String> {
        let path = Self::index_path(log);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
    }

    /// Begin a checkpoint for a new turn. Returns its id.
    pub fn begin(&mut self, prompt: &str, history_len: usize, ts: u64) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        self.checkpoints.push(Checkpoint {
            id,
            ts,
            prompt: prompt.to_string(),
            history_len,
            files: Vec::new(),
            ran_bash: false,
        });
        id
    }

    /// The checkpoint currently being built (the newest one).
    pub fn current_mut(&mut self) -> Option<&mut Checkpoint> {
        self.checkpoints.last_mut()
    }

    /// Capture `path`'s pre-turn contents into the newest checkpoint, once.
    ///
    /// "Once" is the important part: a turn that edits the same file three times
    /// must capture the state from *before the turn*, not before the latest edit,
    /// or a rewind would land on an intermediate state the operator never asked
    /// for.
    pub fn capture(&mut self, log: &Path, path: &Path) -> Result<(), String> {
        let Some(cp) = self.checkpoints.last_mut() else { return Ok(()) };
        if cp.files.iter().any(|f| f.path == path) {
            return Ok(()); // already snapshotted this turn
        }
        let snapshot = match fs::read(path) {
            Ok(bytes) => {
                let dir = Self::dir_for(log).join(format!("cp{}", cp.id));
                fs::create_dir_all(&dir)
                    .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
                let dest = dir.join(snapshot_name(path));
                fs::write(&dest, &bytes)
                    .map_err(|e| format!("cannot write {}: {e}", dest.display()))?;
                Some(dest)
            }
            // The file does not exist yet: a rewind must delete it, not restore
            // it. Distinguishing these is what makes a "created a file" turn
            // fully reversible.
            Err(_) => None,
        };
        cp.files.push(FileState { path: path.to_path_buf(), snapshot });
        Ok(())
    }

    /// Note that the current turn ran a shell command.
    pub fn note_bash(&mut self) {
        if let Some(cp) = self.checkpoints.last_mut() {
            cp.ran_bash = true;
        }
    }

    /// Drop the oldest checkpoints (and their snapshots) once the cap is hit.
    pub fn prune(&mut self, log: &Path) {
        while self.checkpoints.len() > MAX_CHECKPOINTS {
            let old = self.checkpoints.remove(0);
            let _ = fs::remove_dir_all(Self::dir_for(log).join(format!("cp{}", old.id)));
        }
    }

    /// Rewind to checkpoint `id`.
    ///
    /// Restores every captured file (or removes a file that did not exist), drops
    /// the snapshots for the rewound checkpoints, and reports how far the
    /// conversation must be truncated so the caller can do it.
    pub fn rewind(&mut self, log: &Path, id: usize) -> Result<(Checkpoint, RewindReport), String> {
        let idx = self
            .checkpoints
            .iter()
            .position(|c| c.id == id)
            .ok_or_else(|| {
                let known: Vec<String> = self.checkpoints.iter().map(|c| format!("#{}", c.id)).collect();
                format!(
                    "no checkpoint #{id}{}",
                    if known.is_empty() { String::new() } else { format!(" (have: {})", known.join(", ")) }
                )
            })?;
        let cp = self.checkpoints[idx].clone();
        let mut report = RewindReport { ran_bash: cp.ran_bash, ..Default::default() };

        // Restore each file to its pre-turn state. Later checkpoints may also
        // have touched the same path; the oldest snapshot for a path is the
        // pre-turn truth, so apply files in reverse order and let the earliest
        // one win.
        for f in cp.files.iter().rev() {
            match &f.snapshot {
                Some(snap) => match fs::copy(snap, &f.path) {
                    Ok(_) => report.restored += 1,
                    Err(e) => report.failed.push(format!("{}: {e}", f.path.display())),
                },
                None => match fs::remove_file(&f.path) {
                    Ok(_) => report.removed += 1,
                    // Already gone: the desired end state, so not a failure.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => report.failed.push(format!("{}: {e}", f.path.display())),
                },
            }
        }

        // Everything from the rewound checkpoint onward is discarded.
        for dropped in self.checkpoints.drain(idx..) {
            let _ = fs::remove_dir_all(Self::dir_for(log).join(format!("cp{}", dropped.id)));
        }
        let _ = self.save(log);
        Ok((cp, report))
    }

    /// Listing for `/checkpoints`.
    pub fn listing(&self) -> String {
        if self.checkpoints.is_empty() {
            return "(no checkpoints yet — one is recorded at the start of every turn that edits files)"
                .to_string();
        }
        let mut s = format!("{} checkpoint(s), newest last:\n", self.checkpoints.len());
        for c in &self.checkpoints {
            let when = fmt_time(c.ts);
            s.push_str(&format!(
                "  #{:<3} {}  {} file(s){}  {}\n",
                c.id,
                when,
                c.files.len(),
                if c.ran_bash { " +shell" } else { "" },
                if c.prompt.is_empty() { "(no prompt)".to_string() } else { clip(&c.prompt, 50) }
            ));
        }
        s.push_str("rewind with /rewind <#>");
        s.trim_end().to_string()
    }
}

/// A compact `HH:MM:SS` for a checkpoint's timestamp (UTC, matching
/// `term::timestamp_compact`). Kept local so the listing needs no extra surface
/// on `term`.
fn fmt_time(ts: u64) -> String {
    let sod = ts % 86_400;
    format!("{:02}:{:02}:{:02}", sod / 3600, (sod % 3600) / 60, sod % 60)
}

/// A snapshot's file name: the path flattened so two files with the same
/// basename in different directories can't collide.
fn snapshot_name(path: &Path) -> String {
    let mut s: String = path
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if s.len() > 180 {
        // Keep the tail (the basename) when the flattened path is long.
        s = s[s.len() - 180..].to_string();
    }
    format!("{s}.snap")
}

fn clip(s: &str, n: usize) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= n {
        one_line
    } else {
        format!("{}…", one_line.chars().take(n.saturating_sub(1)).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: PathBuf,
        log: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("pir_ckpt_{}_{}", tag, std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let log = dir.join("session.jsonl");
            fs::write(&log, "{}\n").unwrap();
            Fixture { dir, log }
        }
        fn file(&self, name: &str, contents: &str) -> PathBuf {
            let p = self.dir.join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, contents).unwrap();
            p
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn begin_capture_and_rewind_restores_a_modified_file() {
        let f = Fixture::new("modify");
        let p = f.file("a.txt", "before\n");
        let mut store = Store::default();
        store.begin("do the thing", 3, 1000);
        store.capture(&f.log, &p).unwrap();
        // The turn edits it.
        fs::write(&p, "after\n").unwrap();
        store.save(&f.log).unwrap();
        assert!(fs::read_to_string(&p).unwrap().contains("after"));

        let (cp, report) = store.rewind(&f.log, 1).unwrap();
        assert_eq!(cp.prompt, "do the thing");
        assert_eq!(report.restored, 1);
        assert_eq!(report.removed, 0);
        assert_eq!(fs::read_to_string(&p).unwrap(), "before\n");
        assert!(store.checkpoints.is_empty(), "the rewound checkpoint is discarded");
    }

    #[test]
    fn rewind_removes_a_file_the_turn_created() {
        let f = Fixture::new("created");
        let p = f.dir.join("brand-new.txt");
        assert!(!p.exists());
        let mut store = Store::default();
        store.begin("create it", 1, 1000);
        // Capturing a missing file records "did not exist".
        store.capture(&f.log, &p).unwrap();
        fs::write(&p, "hello\n").unwrap();

        let (_, report) = store.rewind(&f.log, 1).unwrap();
        assert_eq!(report.removed, 1);
        assert_eq!(report.restored, 0);
        assert!(!p.exists(), "a created file must be removed by rewind");
    }

    #[test]
    fn capture_is_once_per_turn_not_once_per_edit() {
        let f = Fixture::new("once");
        let p = f.file("a.txt", "v0\n");
        let mut store = Store::default();
        store.begin("edit twice", 1, 1000);
        store.capture(&f.log, &p).unwrap();
        fs::write(&p, "v1\n").unwrap();
        // A second capture of the same path in the same turn must be ignored:
        // otherwise a rewind would land on v1 instead of v0.
        store.capture(&f.log, &p).unwrap();
        fs::write(&p, "v2\n").unwrap();
        assert_eq!(store.checkpoints[0].files.len(), 1);

        store.rewind(&f.log, 1).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "v0\n", "must restore the pre-turn state");
    }

    #[test]
    fn rewind_reports_history_drop_and_shell_side_effects() {
        let f = Fixture::new("meta");
        let p = f.file("a.txt", "x\n");
        let mut store = Store::default();
        store.begin("prompt", 7, 1000);
        store.capture(&f.log, &p).unwrap();
        store.note_bash();
        fs::write(&p, "y\n").unwrap();

        let (cp, report) = store.rewind(&f.log, 1).unwrap();
        assert_eq!(cp.history_len, 7, "the caller truncates history to this");
        assert!(report.ran_bash);
        let text = report.render(&cp);
        assert!(text.contains("rewound to checkpoint #1"), "got: {text}");
        assert!(text.contains("NOT undone"), "must warn about shell effects: {text}");
    }

    #[test]
    fn rewind_unknown_id_lists_what_exists() {
        let f = Fixture::new("unknown");
        let mut store = Store::default();
        store.begin("a", 1, 1);
        store.begin("b", 2, 2);
        let err = store.rewind(&f.log, 99).unwrap_err();
        assert!(err.contains("no checkpoint #99"), "got: {err}");
        assert!(err.contains("#1") && err.contains("#2"), "must list known ids: {err}");
    }

    #[test]
    fn survives_a_reload_from_disk() {
        let f = Fixture::new("persist");
        let p = f.file("a.txt", "original\n");
        {
            let mut store = Store::default();
            store.begin("turn one", 2, 1000);
            store.capture(&f.log, &p).unwrap();
            store.note_bash();
            store.save(&f.log).unwrap();
        }
        fs::write(&p, "changed\n").unwrap();
        // Simulate a resumed session: a fresh store loaded from disk.
        let mut store = Store::load(&f.log);
        assert_eq!(store.checkpoints.len(), 1);
        assert_eq!(store.checkpoints[0].prompt, "turn one");
        assert!(store.checkpoints[0].ran_bash);
        let (_, report) = store.rewind(&f.log, 1).unwrap();
        assert_eq!(report.restored, 1);
        assert_eq!(fs::read_to_string(&p).unwrap(), "original\n");
    }

    #[test]
    fn ids_continue_after_a_reload() {
        let f = Fixture::new("ids");
        {
            let mut store = Store::default();
            store.begin("one", 1, 1);
            store.begin("two", 2, 2);
            store.save(&f.log).unwrap();
        }
        let mut store = Store::load(&f.log);
        let id = store.begin("three", 3, 3);
        assert_eq!(id, 3, "a reloaded store must not reuse ids");
    }

    #[test]
    fn prune_caps_the_history_and_removes_snapshots() {
        let f = Fixture::new("prune");
        let p = f.file("a.txt", "v\n");
        let mut store = Store::default();
        for i in 0..(MAX_CHECKPOINTS + 5) {
            store.begin(&format!("turn {i}"), i, i as u64);
            store.capture(&f.log, &p).unwrap();
        }
        store.prune(&f.log);
        assert_eq!(store.checkpoints.len(), MAX_CHECKPOINTS);
        assert_eq!(store.checkpoints[0].id, 6, "the oldest were dropped");
        // The pruned snapshots are gone from disk.
        assert!(!Store::dir_for(&f.log).join("cp1").exists());
    }

    #[test]
    fn rewinding_discards_later_checkpoints_too() {
        let f = Fixture::new("later");
        let p = f.file("a.txt", "v0\n");
        let mut store = Store::default();
        store.begin("one", 1, 1);
        store.capture(&f.log, &p).unwrap();
        fs::write(&p, "v1\n").unwrap();
        store.begin("two", 5, 2);
        store.capture(&f.log, &p).unwrap();
        fs::write(&p, "v2\n").unwrap();

        // Rewinding to checkpoint 1 also removes checkpoint 2 (it describes a
        // future that no longer exists).
        let (_, report) = store.rewind(&f.log, 1).unwrap();
        assert!(store.checkpoints.is_empty());
        assert_eq!(fs::read_to_string(&p).unwrap(), "v0\n");
        assert_eq!(report.restored, 1);
    }

    #[test]
    fn listing_is_readable_and_reports_emptiness() {
        let f = Fixture::new("listing");
        let mut store = Store::default();
        assert!(store.listing().contains("no checkpoints yet"));
        let p = f.file("a.txt", "x\n");
        store.begin("add the parser", 1, 1000);
        store.capture(&f.log, &p).unwrap();
        store.note_bash();
        let l = store.listing();
        assert!(l.contains("#1"), "got: {l}");
        assert!(l.contains("1 file(s)"), "got: {l}");
        assert!(l.contains("+shell"), "got: {l}");
        assert!(l.contains("add the parser"), "got: {l}");
        assert!(l.contains("/rewind"), "must name the command: {l}");
    }

    #[test]
    fn corrupt_or_missing_index_loads_as_empty() {
        let f = Fixture::new("corrupt");
        // Missing.
        assert!(Store::load(&f.log).checkpoints.is_empty());
        // Corrupt.
        fs::write(Store::index_path(&f.log), "{not json").unwrap();
        assert!(Store::load(&f.log).checkpoints.is_empty());
    }

    #[test]
    fn snapshot_names_cannot_collide_across_directories() {
        let a = snapshot_name(Path::new("/x/one/config.toml"));
        let b = snapshot_name(Path::new("/x/two/config.toml"));
        assert_ne!(a, b, "same basename in different dirs must not collide");
        assert!(a.ends_with(".snap"));
        // A very long path is truncated but still ends distinctly.
        let long = snapshot_name(Path::new(&format!("/{}/f.txt", "d/".repeat(200))));
        assert!(long.len() <= 190, "got {} chars", long.len());
    }
}
