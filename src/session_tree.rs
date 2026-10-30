//! Left sessions tree: grouped session model + per-session presence.
//!
//! The flat session lists (`/sessions`, the `pir -r` picker, `/menu` resume)
//! all show one newest-first list with no project structure. This module is
//! the single model behind the left tree: sessions grouped by project root,
//! each carrying one [`SessionPresence`] value that answers "is it running,
//! waiting unread, or waiting but already seen".
//!
//! The model is backend-agnostic on purpose: the text renderer here
//! ([`render_text`]) drives `/sessions` today, and the `rustxwidgets` Tree
//! contract (`TreeStatus`/`TreeNode`, same four states) renders the identical
//! model graphically in the TUI/GUI tomorrow. No `#[cfg(target_os)]` widget
//! code lives here — or anywhere in `pir`.
//!
//! Presence sources (all existing signals, no new daemons):
//!   * `Running` — a live turn: the status sidecar's pid is alive
//!     ([`crate::session::pid_alive`]).
//!   * waiting — the light-model verdict is `waiting`/`retry`/`blocked`/
//!     `error` (or `interrupted`/`incomplete`), or the structural status is
//!     `Interrupted` with a reason. Verdicts ride in `<log>.verdict`
//!     ([`crate::session::read_verdict`]).
//!   * unread vs seen — the [`crate::notify::NotifyBus`] keeps a per-session
//!     feed; a non-empty feed for the session means `NeedsInputUnread(n)`,
//!     an empty feed with a waiting condition means `NeedsInputSeen` (the
//!     operator focused the finish state, which drains the feed via
//!     `mark_seen`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One scanned session, as produced by `scan_sessions` in the crate root.
/// A plain owned copy (rather than `&crate::Session`) so this module stays
/// independently testable and never depends on the REPL's private types.
#[derive(Debug, Clone)]
pub struct ScanRow {
    pub path: PathBuf,
    pub name: String,
    pub shell_pid: u32,
    pub tty: String,
    pub cwd: String,
    pub mtime: SystemTime,
    pub bytes: u64,
    pub preview: String,
    pub local: bool,
    /// Whether this session came from the operator's current shell (the `▸`
    /// highlight in the flat lists).
    pub from_here: bool,
}

/// Per-session presence: the ONLY state the tree (text or graphical) shows.
///
/// Four values, no more:
///   * `Running` — a live turn is in flight. Rendered as an animated
///     spinner/dot (cyan), never bold.
///   * `NeedsInputUnread(n)` — the turn exited waiting for input and the
///     operator has NOT seen the finish state (`n` pending notifications).
///     Bold label + amber dot + `● n` badge. The only state that steals
///     attention.
///   * `NeedsInputSeen` — waiting for input, but the operator already focused
///     the session (feed drained). Same dot outline / `✓`, normal weight,
///     no badge, no bell.
///   * `Idle` — finished/completed with nothing pending. Dim, no adornment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPresence {
    Running,
    NeedsInputUnread(u32),
    NeedsInputSeen,
    Idle,
}

impl SessionPresence {
    /// Text glyph fallback: carries the state on backends with no color or
    /// badge support (pancurses, no-color, the flat `/sessions` list). Every
    /// graphical mapping MUST keep these four visually distinct too.
    pub fn glyph(&self) -> &'static str {
        match self {
            SessionPresence::Running => "▶",
            SessionPresence::NeedsInputUnread(_) => "●",
            SessionPresence::NeedsInputSeen => "✓",
            SessionPresence::Idle => "·",
        }
    }

    /// Pending notification count (0 unless unread).
    pub fn unread(&self) -> u32 {
        match self {
            SessionPresence::NeedsInputUnread(n) => *n,
            _ => 0,
        }
    }

    /// True only for the attention-stealing state.
    pub fn demands_attention(&self) -> bool {
        matches!(self, SessionPresence::NeedsInputUnread(_))
    }
}

/// One row in the tree: a session plus its derived display fields.
#[derive(Debug, Clone)]
pub struct SessionNode {
    /// Session id: the transcript path as a string (what `mark_seen` and
    /// `drain_session` key on).
    pub id: String,
    /// Display title: `.title` sidecar, else first-line preview, else name.
    pub title: String,
    /// Project group key (canonical root path).
    pub project_key: String,
    /// Last-two-components directory tail, so sibling checkouts stay distinct.
    pub cwd_tail: String,
    /// True when the transcript lives in the cwd-local `.pir/sessions` store.
    pub local: bool,
    pub mtime_secs: u64,
    pub bytes: u64,
    pub preview: String,
    pub presence: SessionPresence,
    /// The one-glyph "what is this session doing" status the GUI tree row
    /// paints (`session_tree::tree_status_of`): running, stopped-unseen,
    /// stopped-seen. Derived alongside `presence` in [`build_tree`] so a row
    /// cannot show one state in the text lists and another in the GUI tree.
    pub status: TreeStatus,
    pub live: bool,
    pub from_here: bool,
}

/// The GUI session-tree row status: three values, one icon, painted to the
/// left of every session row in the sidebar.
///
/// This is the tree's own vocabulary, deliberately narrower than
/// [`SessionPresence`] (which also distinguishes *why* a stopped session is
/// stopped, and how many notifications are pending):
///
///   * `Running` — work is in flight: a turn is running, a background job is
///     live, or the worker is parked on a dialog it raised (a tool approval, a
///     question, plan review). "Waiting on you" still counts as running here:
///     the session is mid-work and has not come to rest.
///   * `StoppedUnseen` — the session came to rest and the operator has not
///     looked at that since. The attention state: amber dot.
///   * `StoppedSeen` — the session came to rest and the operator has seen the
///     result (opened/selected the session, or it was resting last time we
///     looked). The settled state: dim dot.
///
/// "Came to rest" is the only thing that ever moves a row out of `Running`:
/// a stopped-but-unread session (a turn that finished while you were in
/// another tab, an extension follow-up that ended, `pir` exiting) and a
/// stopped-but-read one differ only in whether the finish was acknowledged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeStatus {
    Running,
    StoppedUnseen,
    StoppedSeen,
}

impl TreeStatus {
    /// The glyph painted in the tree. Three visually distinct shapes, so the
    /// status survives a colour-less theme: filled right-pointing triangle
    /// (in flight), filled circle (stopped, attention), hollow circle
    /// (stopped, settled).
    pub fn glyph(&self) -> &'static str {
        match self {
            TreeStatus::Running => "▶",
            TreeStatus::StoppedUnseen => "●",
            TreeStatus::StoppedSeen => "○",
        }
    }

    /// Hex colour for the glyph on backends that render markup (the GUI uses
    /// Pango). Kept next to [`TreeStatus::glyph`] so the two never disagree
    /// about which state a row is in.
    pub fn color(&self) -> &'static str {
        match self {
            TreeStatus::Running => "#3794ff",        // blue: work in flight
            TreeStatus::StoppedUnseen => "#e0a63a",  // amber: needs a look
            TreeStatus::StoppedSeen => "#6f6f6f",    // dim: settled
        }
    }

    /// Plain-text word for the state (status hints, `/help`, tests).
    pub fn label(&self) -> &'static str {
        match self {
            TreeStatus::Running => "running",
            TreeStatus::StoppedUnseen => "stopped (unseen)",
            TreeStatus::StoppedSeen => "stopped (seen)",
        }
    }

    /// Whether this state asks the operator to look (`StoppedUnseen`).
    pub fn wants_attention(&self) -> bool {
        matches!(self, TreeStatus::StoppedUnseen)
    }
}

/// Derive a row's [`TreeStatus`] from the live signals.
///
/// * `active` — something in this session is still working *and* has not come
///   to rest: the foreground turn, a background turn/job, or the live pid in
///   the status sidecar. A worker blocked on a dialog keeps this true.
/// * `at_rest` — the session has settled (idle at the prompt, finished,
///   interrupted, failed) *and* the GUI has already acknowledged it: the
///   operator has it in front of them, or it was resting last time. A resting
///   session that is not acknowledged is `StoppedUnseen`.
///
/// `active` wins: a session mid-work has nothing to acknowledge yet, even if a
/// stale "seen" mark lingers from a previous run.
pub fn tree_status_of(active: bool, at_rest: bool) -> TreeStatus {
    if active {
        TreeStatus::Running
    } else if at_rest {
        TreeStatus::StoppedSeen
    } else {
        TreeStatus::StoppedUnseen
    }
}

/// [`TreeStatus`] derived from the presence / liveness signals [`build_tree`]
/// already computes, for callers that have no GUI acknowledgements (the
/// `/sessions` text tree, the flat lists). There, "stopped unseen" is exactly
/// what `SessionPresence` calls `NeedsInputUnread`; everything else that is
/// not running has been seen.
pub fn tree_status_from_presence(presence: SessionPresence, live: bool) -> TreeStatus {
    match presence {
        _ if live => TreeStatus::Running,
        SessionPresence::Running => TreeStatus::Running,
        SessionPresence::NeedsInputUnread(_) => TreeStatus::StoppedUnseen,
        SessionPresence::NeedsInputSeen | SessionPresence::Idle => TreeStatus::StoppedSeen,
    }
}

/// One project group in the tree.
#[derive(Debug, Clone)]
pub struct ProjectGroup {
    /// Canonical project root path (or `"(unknown)"`).
    pub key: String,
    /// Short label (root basename).
    pub label: String,
    /// True when any session in the group is project-local.
    pub local: bool,
    pub sessions: Vec<SessionNode>,
}

/// Selection + expansion state, persisted per cwd (see [`load_state`] /
/// [`save_state`]). Groups default to expanded; only explicit collapses are
/// stored.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TreeState {
    pub selected: Option<String>,
    pub expanded: HashSet<String>,
    /// Group keys the operator explicitly collapsed. Absent = expanded.
    #[serde(default)]
    pub collapsed: HashSet<String>,
}

impl TreeState {
    pub fn is_expanded(&self, key: &str) -> bool {
        !self.collapsed.contains(key)
    }

    pub fn set_expanded(&mut self, key: &str, expanded: bool) {
        if expanded {
            self.collapsed.remove(key);
        } else {
            self.collapsed.insert(key.to_string());
        }
        self.expanded.insert(key.to_string());
    }
}

/// Verdict tokens (or structural states) that mean "waiting for input".
/// Mirrors `titler::VERDICTS` + the `interrupted`/`incomplete` labels in
/// `titler::verdict_label`: anything in this set on an idle session renders
/// as `NeedsInput*` rather than `Idle`.
fn is_waiting_verdict(token: &str) -> bool {
    matches!(
        token.trim(),
        "waiting" | "retry" | "blocked" | "error" | "interrupted" | "incomplete"
    )
}

/// Derive the presence for one session from the three existing signals.
///
/// `live` = status sidecar pid is alive. `waiting` = verdict or structural
/// status says the turn needs the user. `unread` = pending notifications for
/// this session on the bus. Priority is fixed: a live turn is `Running` even
/// when a stale verdict also says waiting.
pub fn presence_for(live: bool, waiting: bool, unread: u32) -> SessionPresence {
    if live {
        SessionPresence::Running
    } else if waiting && unread > 0 {
        SessionPresence::NeedsInputUnread(unread)
    } else if waiting {
        SessionPresence::NeedsInputSeen
    } else {
        SessionPresence::Idle
    }
}

/// Derive `(key, label)` for the project group owning `cwd`.
///
/// Walks up at most 8 levels looking for a `.git` dir/file (worktree) or
/// `.jj` dir; the first hit is the project root. Otherwise the cwd itself is
/// the root. An empty cwd groups under `"(unknown)"`. The `label` is the root
/// basename (what the tree header shows); the `key` is the full path (stable
/// across checkouts with the same leaf name).
pub fn derive_project_root(cwd: &str) -> (String, String) {
    let cwd = cwd.trim().trim_end_matches('/');
    if cwd.is_empty() {
        return ("(unknown)".to_string(), "(no directory)".to_string());
    }
    let mut dir: &str = cwd;
    for _ in 0..8 {
        let git = Path::new(dir).join(".git");
        let jj = Path::new(dir).join(".jj");
        if git.exists() || jj.exists() {
            return (dir.to_string(), basename_of(dir).to_string());
        }
        match Path::new(dir).parent().and_then(|p| p.to_str()) {
            Some(parent) if !parent.is_empty() && parent != dir => dir = parent,
            _ => break,
        }
    }
    (cwd.to_string(), basename_of(cwd).to_string())
}

fn basename_of(path: &str) -> &str {
    path.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(path)
}

/// Last-two-components tail of `cwd` (`a/b/c` → `b/c`), so sibling checkouts
/// stay distinguishable in the detail line.
pub fn cwd_tail_of(cwd: &str) -> String {
    let parts: Vec<&str> = cwd.split('/').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [] => String::new(),
        [a] => a.to_string(),
        [.., a, b] => format!("{a}/{b}"),
    }
}

/// Whether the session at `log` currently has a live process driving it:
/// a turn in flight (`Active`) owned by a pid that still exists. A sidecar
/// that says `Completed`/`Interrupted`/`Finished` is at rest even when its
/// pid is still alive (the GUI writes its own pid on every turn, so the
/// process outlives the turn) — without the status gate every idle session
/// of a live process would read as running and its ▶ would never clear.
pub(crate) fn live_session(log: &Path) -> bool {
    match crate::session::read_status(log) {
        Some(m) => {
            m.status == crate::session::SessionStatus::Active
                && m.pid != 0
                && crate::session::pid_alive(m.pid)
        }
        None => false,
    }
}

/// Whether the session at `log` is waiting for input: light-model verdict in
/// the waiting set, or a structural `Interrupted` status with a reason.
pub(crate) fn waiting_session(log: &Path) -> bool {
    if let Some(v) = crate::session::read_verdict(log)
        && is_waiting_verdict(&v)
    {
        return true;
    }
    matches!(
        crate::session::read_status(log),
        Some(m) if m.status == crate::session::SessionStatus::Interrupted
    )
}

pub(crate) fn title_for(log: &Path, preview: &str, name: &str) -> String {
    if let Some(t) = crate::session::read_title(log) {
        return t;
    }
    if !preview.is_empty() {
        return preview.to_string();
    }
    name.replace("pir-", "").replace(".jsonl", "")
}

fn mtime_secs(t: SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Build the grouped tree from scanned rows.
///
/// `unread` maps session id (transcript path string) to pending notification
/// count, from `NotifyBus::unread_counts`. Groups sort with project-local
/// groups first, then alphabetically by label; sessions sort newest-first
/// (`mtime` desc, `bytes` desc — the same order the flat lists use).
pub fn build_tree(rows: &[ScanRow], unread: &HashMap<String, u32>) -> Vec<ProjectGroup> {
    let mut groups: HashMap<String, ProjectGroup> = HashMap::new();
    for r in rows {
        let (key, label) = derive_project_root(&r.cwd);
        let id = r.path.to_string_lossy().into_owned();
        let live = live_session(&r.path);
        let waiting = !live && waiting_session(&r.path);
        let n = unread.get(&id).copied().unwrap_or(0);
        let presence = presence_for(live, waiting, n);
        let node = SessionNode {
            id,
            title: title_for(&r.path, &r.preview, &r.name),
            project_key: key.clone(),
            cwd_tail: cwd_tail_of(&r.cwd),
            local: r.local,
            mtime_secs: mtime_secs(r.mtime),
            bytes: r.bytes,
            preview: r.preview.clone(),
            presence,
            status: tree_status_from_presence(presence, live),
            live,
            from_here: r.from_here,
        };
        groups
            .entry(key.clone())
            .and_modify(|g| {
                g.sessions.push(node.clone());
                g.local |= r.local;
            })
            .or_insert_with(|| ProjectGroup {
                key: key.clone(),
                label,
                local: r.local,
                sessions: vec![node],
            });
    }
    let mut out: Vec<ProjectGroup> = groups.into_values().collect();
    for g in &mut out {
        g.sessions.sort_by(|a, b| {
            b.mtime_secs.cmp(&a.mtime_secs).then(b.bytes.cmp(&a.bytes))
        });
    }
    // Project-local groups first (they belong to *this* checkout), then
    // alphabetically — so the current project is always at the top.
    out.sort_by(|a, b| b.local.cmp(&a.local).then(a.label.cmp(&b.label)));
    out
}

/// Render the tree as plain text (the `/sessions` fallback and the
/// no-color/no-badge backend mapping). `selected` highlights one session id
/// with `▸`; collapsed groups show a `▸ N sessions` summary line instead of
/// their rows.
///
/// Every row carries the presence glyph, so the four states stay distinct
/// even where color/badges are unavailable:
/// `▶` running, `● n` waiting-unread, `✓` waiting-seen, `·` idle.
pub fn render_text(groups: &[ProjectGroup], state: &TreeState) -> String {
    let mut out = String::new();
    for g in groups {
        let expanded = state.is_expanded(&g.key);
        let marker = if expanded { "▾" } else { "▸" };
        let counts = group_counts(g);
        out.push_str(&format!("{} {}  {counts}\n", marker, g.label));
        if !expanded {
            continue;
        }
        for s in &g.sessions {
            let sel = if state.selected.as_deref() == Some(&s.id) || s.from_here {
                "▸"
            } else {
                " "
            };
            let badge = match s.presence {
                SessionPresence::NeedsInputUnread(n) => format!("●{n} "),
                _ => format!("{} ", s.presence.glyph()),
            };
            let detail = if s.cwd_tail.is_empty() {
                String::new()
            } else {
                format!(" ({})", s.cwd_tail)
            };
            out.push_str(&format!("  {sel} {badge}{s_title}{detail}  [{st}]\n",
                s_title = truncate(&s.title, 60),
                st = s.status.label()));
        }
    }
    out
}

fn group_counts(g: &ProjectGroup) -> String {
    let running = g.sessions.iter().filter(|s| s.presence == SessionPresence::Running).count();
    let unread: u32 = g.sessions.iter().map(|s| s.presence.unread()).sum();
    let waiting_seen = g
        .sessions
        .iter()
        .filter(|s| s.presence == SessionPresence::NeedsInputSeen)
        .count();
    let mut parts = vec![format!("{} session{}", g.sessions.len(), if g.sessions.len() == 1 { "" } else { "s" })];
    if running > 0 {
        parts.push(format!("{running} running"));
    }
    if unread > 0 {
        parts.push(format!("{unread} unread"));
    }
    if waiting_seen > 0 {
        parts.push(format!("{waiting_seen} waiting (seen)"));
    }
    parts.join(" · ")
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>())
    }
}

/// Where the tree state lives: `<pi_dir>/agent/session-tree.json`, a map from
/// cwd to [`TreeState`]. One file (not one per cwd) so a checkout with ten
/// sessions costs one read.
pub fn state_file() -> PathBuf {
    crate::config::pi_dir().join("agent").join("session-tree.json")
}

/// Load the [`TreeState`] for `cwd`, or a default (everything expanded) when
/// absent/unreadable. Never fails: a missing state just means "fresh tree".
pub fn load_state(cwd: &str) -> TreeState {
    load_state_from(&state_file(), cwd)
}

pub fn load_state_from(path: &Path, cwd: &str) -> TreeState {
    let raw = match std::fs::read(path) {
        Ok(r) => r,
        Err(_) => return TreeState::default(),
    };
    match serde_json::from_slice::<HashMap<String, TreeState>>(&raw) {
        Ok(m) => m.get(cwd).cloned().unwrap_or_default(),
        Err(_) => TreeState::default(),
    }
}

/// Persist `state` for `cwd`, best-effort. Other cwds' states are preserved.
pub fn save_state(cwd: &str, state: &TreeState) {
    save_state_to(&state_file(), cwd, state);
}

pub fn save_state_to(path: &Path, cwd: &str, state: &TreeState) {
    let mut m: HashMap<String, TreeState> = std::fs::read(path)
        .ok()
        .and_then(|r| serde_json::from_slice(&r).ok())
        .unwrap_or_default();
    m.insert(cwd.to_string(), state.clone());
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec(&m).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, cwd: &str, mtime: u64, local: bool) -> ScanRow {
        ScanRow {
            path: PathBuf::from(format!("/tmp/tree-test/{name}.jsonl")),
            name: name.to_string(),
            shell_pid: 1,
            tty: String::new(),
            cwd: cwd.to_string(),
            mtime: std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime),
            bytes: 100,
            preview: format!("preview {name}"),
            local,
            from_here: false,
        }
    }

    /// Sessions in different roots land in different groups; the local group
    /// sorts first even when its sessions are older; sessions sort newest-first
    /// inside a group. None of the fixture paths exist on disk, so no sidecar
    /// reads fire (no `.title`/`.status.json` beside them): titles fall back to
    /// the preview and presence falls back to `Idle`.
    #[test]
    fn groups_by_root_local_first_newest_first() {
        let rows = vec![
            row("old-global", "/other/proj", 100, false),
            row("new-global", "/other/proj", 300, false),
            row("mid-local", "/home/u/pir", 200, true),
        ];
        let groups = build_tree(&rows, &HashMap::new());
        assert_eq!(groups.len(), 2, "two roots = two groups, got {groups:?}");
        // Local first despite being older.
        assert!(groups[0].local, "project-local group sorts first");
        assert_eq!(groups[0].sessions.len(), 1);
        assert_eq!(groups[0].sessions[0].name_preview(), "preview mid-local");
        // Newest first inside the global group.
        assert_eq!(groups[1].sessions.len(), 2);
        assert_eq!(groups[1].sessions[0].name_preview(), "preview new-global");
        assert_eq!(groups[1].sessions[1].name_preview(), "preview old-global");
    }

    /// Unknown (empty) cwds group together instead of scattering.
    #[test]
    fn empty_cwd_groups_under_unknown() {
        let rows = vec![row("a", "", 1, false), row("b", "", 2, false)];
        let groups = build_tree(&rows, &HashMap::new());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].key, "(unknown)");
    }

    /// The presence priority the tree paints: live beats waiting beats idle,
    /// and unread beats seen. This asserts on the value the code under test
    /// itself computed (`presence_for`), not a recomputation from test inputs.
    #[test]
    fn presence_priority_live_then_unread_then_seen_then_idle() {
        assert_eq!(presence_for(true, true, 3), SessionPresence::Running);
        assert_eq!(presence_for(false, true, 2), SessionPresence::NeedsInputUnread(2));
        assert_eq!(presence_for(false, true, 0), SessionPresence::NeedsInputSeen);
        assert_eq!(presence_for(false, false, 0), SessionPresence::Idle);
        // A stale notification for an idle session never promotes it.
        assert_eq!(presence_for(false, false, 5), SessionPresence::Idle);
    }

    /// Glyphs are four distinct values — the no-color/no-badge fallback the
    /// whole contract depends on.
    #[test]
    fn glyphs_are_distinct() {
        let g = [
            SessionPresence::Running.glyph(),
            SessionPresence::NeedsInputUnread(1).glyph(),
            SessionPresence::NeedsInputSeen.glyph(),
            SessionPresence::Idle.glyph(),
        ];
        let mut set = HashSet::new();
        for x in g {
            assert!(set.insert(x), "glyph {x} is not distinct");
        }
    }

    /// Only `NeedsInputUnread` demands attention; `Running` explicitly does
    /// not (a spinner must never steal focus).
    #[test]
    fn only_unread_demands_attention() {
        assert!(SessionPresence::NeedsInputUnread(1).demands_attention());
        assert!(!SessionPresence::Running.demands_attention());
        assert!(!SessionPresence::NeedsInputSeen.demands_attention());
        assert!(!SessionPresence::Idle.demands_attention());
    }

    /// `derive_project_root` climbs to the VCS root: a nested cwd inside a
    /// temp repo groups under the repo, and a plain dir groups under itself.
    #[test]
    fn derive_project_root_climbs_to_vcs_root() {
        let base = std::env::temp_dir().join(format!("pir_tree_root_{}", std::process::id()));
        let nested = base.join("a").join("b");
        let _ = std::fs::create_dir_all(&nested);
        let _ = std::fs::create_dir_all(base.join(".git"));
        let nested_s = nested.to_string_lossy().into_owned();
        let (key, label) = derive_project_root(&nested_s);
        assert_eq!(key, base.to_string_lossy(), "must climb to the .git root");
        assert_eq!(label, basename_of(&base.to_string_lossy()));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A dir with no VCS marker anywhere nearby groups under itself.
    #[test]
    fn derive_project_root_without_vcs_is_cwd() {
        let (key, label) = derive_project_root("/x/y/proj");
        assert_eq!(key, "/x/y/proj");
        assert_eq!(label, "proj");
    }

    /// Collapse/expand round-trips through the state file, and other cwds'
    /// states survive a save.
    #[test]
    fn tree_state_persists_per_cwd() {
        let dir = std::env::temp_dir().join(format!("pir_tree_state_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("session-tree.json");
        let mut s = TreeState::default();
        assert!(s.is_expanded("g1"), "default is expanded");
        s.set_expanded("g1", false);
        s.selected = Some("sess-1".into());
        save_state_to(&p, "/cwd/a", &s);
        save_state_to(&p, "/cwd/b", &TreeState::default());

        let back = load_state_from(&p, "/cwd/a");
        assert!(!back.is_expanded("g1"), "collapse must persist");
        assert_eq!(back.selected.as_deref(), Some("sess-1"));
        assert!(load_state_from(&p, "/cwd/b").is_expanded("g1"), "other cwds unaffected");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The text renderer shows group headers with counts, presence glyphs on
    /// every row, and hides collapsed groups' rows.
    #[test]
    fn render_text_shows_groups_glyphs_and_collapse() {
        let groups = vec![ProjectGroup {
            key: "k".into(),
            label: "proj".into(),
            local: true,
            sessions: vec![
                SessionNode {
                    id: "a".into(),
                    title: "fix parser".into(),
                    project_key: "k".into(),
                    cwd_tail: "x/proj".into(),
                    local: true,
                    mtime_secs: 2,
                    bytes: 10,
                    preview: String::new(),
                    presence: SessionPresence::NeedsInputUnread(2),
                    status: TreeStatus::StoppedUnseen,
                    live: false,
                    from_here: false,
                },
                SessionNode {
                    id: "b".into(),
                    title: "other".into(),
                    project_key: "k".into(),
                    cwd_tail: String::new(),
                    local: true,
                    mtime_secs: 1,
                    bytes: 10,
                    preview: String::new(),
                    presence: SessionPresence::Running,
                    status: TreeStatus::Running,
                    live: true,
                    from_here: true,
                },
            ],
        }];
        let mut st = TreeState::default();
        st.selected = Some("a".into());
        let out = render_text(&groups, &st);
        assert!(out.contains("▾ proj"), "expanded group header:\n{out}");
        assert!(out.contains("●2 fix parser"), "unread badge + title:\n{out}");
        assert!(out.contains("▶"), "running glyph:\n{out}");
        // The text list also carries the three-state status word, so the
        // `/sessions` rows and the GUI sidebar icons tell one story.
        assert!(out.contains("[stopped (unseen)]"), "unseen status:\n{out}");
        assert!(out.contains("[running]"), "running status:\n{out}");

        st.set_expanded("k", false);
        let out2 = render_text(&groups, &st);
        assert!(out2.contains("▸ proj"), "collapsed header:\n{out2}");
        assert!(!out2.contains("fix parser"), "collapsed rows hidden:\n{out2}");
    }

    /// The three tree statuses stay visually distinct (glyph AND colour), and
    /// only "stopped unseen" asks for attention.
    #[test]
    fn tree_status_glyphs_and_colours_are_distinct() {
        let all = [TreeStatus::Running, TreeStatus::StoppedUnseen, TreeStatus::StoppedSeen];
        let glyphs: HashSet<&str> = all.iter().map(|s| s.glyph()).collect();
        let colours: HashSet<&str> = all.iter().map(|s| s.color()).collect();
        assert_eq!(glyphs.len(), 3, "one glyph per status");
        assert_eq!(colours.len(), 3, "one colour per status");
        assert!(TreeStatus::StoppedUnseen.wants_attention());
        assert!(!TreeStatus::Running.wants_attention());
        assert!(!TreeStatus::StoppedSeen.wants_attention());
    }

    /// `tree_status_of`: active wins; otherwise "at rest" is settled.
    #[test]
    fn tree_status_of_prefers_running_then_rest() {
        assert_eq!(tree_status_of(true, false), TreeStatus::Running);
        assert_eq!(tree_status_of(true, true), TreeStatus::Running);
        assert_eq!(tree_status_of(false, true), TreeStatus::StoppedSeen);
        assert_eq!(tree_status_of(false, false), TreeStatus::StoppedUnseen);
    }

    /// `build_tree` stamps each node with a status consistent with its
    /// presence (the value the text row then renders).
    #[test]
    fn build_tree_stamps_status_from_presence() {
        // None of these fixture paths exist, so both are idle + not live.
        let rows = vec![row("a", "/other/proj", 1, false)];
        let groups = build_tree(&rows, &HashMap::new());
        let n = &groups[0].sessions[0];
        assert_eq!(n.presence, SessionPresence::Idle);
        assert_eq!(n.status, TreeStatus::StoppedSeen, "idle = settled, not amber");
        assert_eq!(
            tree_status_from_presence(SessionPresence::NeedsInputUnread(1), false),
            TreeStatus::StoppedUnseen
        );
        assert_eq!(
            tree_status_from_presence(SessionPresence::Idle, true),
            TreeStatus::Running,
            "a live pid is running whatever the presence says"
        );
    }

    /// A sidecar that says `Completed` is at rest even when its pid is still
    /// alive: the GUI writes its own pid on every turn, so the process
    /// outlives the turn. Without the status gate every idle session of a
    /// live process reads as running and its ▶ never clears — the
    /// stuck-play-icon bug. Asserts on what `live_session` itself reads back
    /// from the sidecar, not a recomputation from the test's inputs.
    #[test]
    fn live_session_requires_an_active_sidecar() {
        let dir = std::env::temp_dir().join(format!("pir_live_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let log = dir.join("s.jsonl");
        let _ = std::fs::write(&log, "{}\n");
        let me = std::process::id();
        crate::session::write_status(
            &log,
            crate::session::SessionStatus::Active,
            me,
            "",
            false,
            "",
        );
        assert!(live_session(&log), "active + live pid = driven");
        crate::session::write_status(
            &log,
            crate::session::SessionStatus::Completed,
            me,
            "",
            false,
            "",
        );
        assert!(!live_session(&log), "completed + live pid = at rest, or ▶ never clears");
        crate::session::write_status(
            &log,
            crate::session::SessionStatus::Interrupted,
            me,
            "",
            false,
            "cancelled",
        );
        assert!(!live_session(&log), "interrupted + live pid = at rest");
        let _ = std::fs::remove_dir_all(&dir);
    }

    impl SessionNode {
        fn name_preview(&self) -> &str {
            &self.preview
        }
    }
}
