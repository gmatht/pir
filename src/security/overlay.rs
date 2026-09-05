//! Overlayfs-backed write quarantine.
//!
//! Default-on safe posture (see `docs/SECURITY_MODEL.md` §12): the agent may
//! run *all* commands, but writes that land in the configured system trees are
//! transparently redirected into an overlayfs `upperdir`. The agent sees its
//! writes immediately (only it can see them); the real filesystem underneath is
//! untouched. The operator later reviews the staged writes (`/quarantine`) and
//! applies them to the real fs or discards them.
//!
//! This is strictly stronger than the in-process guardrail: the guardrail only
//! intercepts operations the agent routes through `SecurityContext::check`,
//! whereas overlayfs intercepts *every* write at the syscall level. The
//! guardrail still runs in parallel for the paths in `quarantine_critical`
//! (those are never applied to the real fs) and for non-root sessions where
//! overlayfs is unavailable.
//!
//! Only meaningful on unix with root/`CAP_SYS_ADMIN`. On non-unix the module
//! degrades to a no-op so the rest of the build stays portable.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::security::SecurityPolicy;

/// System trees overlaid by default. Empty policy list => these.
pub const DEFAULT_OVERLAY_DIRS: &[&str] = &[
    "/etc",
    "/usr/local",
    "/opt",
    "/srv",
    "/var",
    "/boot",
];

/// A write the agent has staged into an overlay `upperdir` but not yet applied
/// to the real filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedWrite {
    /// The overlaid tree this write lives under (e.g. `/etc`).
    pub tree: PathBuf,
    /// Path relative to `tree` (e.g. `nginx/nginx.conf`).
    pub rel: PathBuf,
    /// Full path as the agent sees it (inside the overlay).
    pub overlay_path: PathBuf,
    /// Full path on the real filesystem (hidden until applied).
    pub real_path: PathBuf,
    /// What kind of change the agent made.
    pub kind: WriteKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    Added,
    Modified,
    Deleted,
}

impl WriteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WriteKind::Added => "add",
            WriteKind::Modified => "mod",
            WriteKind::Deleted => "del",
        }
    }
}

/// The live quarantine state: where the staging layers live and which real
/// trees are currently overlaid.
#[derive(Debug, Clone)]
pub struct Quarantine {
    /// Where the per-tree `upper-*` / `work-*` layers live.
    pub staging: PathBuf,
    /// Real trees currently mounted as overlays (top of the agent's view).
    pub trees: Vec<PathBuf>,
    /// Patterns that must never be applied to the real fs (hard-denied at apply
    /// time; they stay staged in the overlay only).
    pub critical: Vec<String>,
}

/// Why a quarantine mount could not be established.
#[derive(Debug)]
pub enum OverlayError {
    Unsupported(String),
    Io(std::io::Error),
    Mount(String),
}

impl From<std::io::Error> for OverlayError {
    fn from(e: std::io::Error) -> Self {
        OverlayError::Io(e)
    }
}

impl std::fmt::Display for OverlayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OverlayError::Unsupported(s) => write!(f, "overlay unsupported: {s}"),
            OverlayError::Io(e) => write!(f, "overlay io error: {e}"),
            OverlayError::Mount(s) => write!(f, "overlay mount failed: {s}"),
        }
    }
}

/// The single live quarantine for this agent process (if mounted). Holds the
/// `Quarantine` so the `/quarantine` command and agent shutdown can read /
/// apply / tear it down.
static ACTIVE: OnceLock<Mutex<Option<Quarantine>>> = OnceLock::new();

fn active_lock() -> &'static Mutex<Option<Quarantine>> {
    ACTIVE.get_or_init(|| Mutex::new(None))
}

impl Quarantine {
    /// Build a quarantine description from a policy. Does not mount anything;
    /// call [`Quarantine::mount`] to engage. The staging area defaults to
    /// `~/.pi/agent/quarantine-staging` when the policy leaves it empty (and it
    /// must not live inside any tree we overlay).
    pub fn from_policy(policy: &SecurityPolicy) -> Quarantine {
        let trees = Quarantine::resolve_trees(&policy.quarantine_dirs);
        let staging = if policy.quarantine_staging.as_os_str().is_empty() {
            // Stage under the *agent* user's home (not the invoking user's, which
            // may be root's and unreachable by `ai_X`), so the agent's bash
            // commands can actually write the overlay upper/work dirs.
            quarantine_staging_base()
        } else {
            policy.quarantine_staging.clone()
        };
        Quarantine {
            staging,
            trees,
            critical: policy.quarantine_critical.clone(),
        }
    }

    /// Build the resolved list of trees to overlay from a policy.
    pub fn resolve_trees(dirs: &[String]) -> Vec<PathBuf> {
        if dirs.is_empty() {
            DEFAULT_OVERLAY_DIRS.iter().map(PathBuf::from).collect()
        } else {
            dirs.iter()
                .map(|d| PathBuf::from(d.trim()))
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        }
    }

    fn upper_for(&self, tree: &Path) -> PathBuf {
        self.staging.join(format!("upper-{}", safe_name(tree)))
    }

    /// Engage the overlay mounts for every existing tree. Trees that do not yet
    /// exist on the real fs are skipped (nothing to overlay). Returns the number
    /// of trees successfully mounted.
    #[cfg(unix)]
    pub fn mount(&mut self) -> Result<usize, OverlayError> {
        let kind = match overlay_kind() {
            Some(k) => k,
            None => {
                return Err(OverlayError::Unsupported(
                    "no overlay available (need root or fuse-overlayfs)".into(),
                ))
            }
        };
        create_dir_all_err(&self.staging)?;
        crate::user::chown_to_agent_user(&self.staging);
        let mut mounted = 0usize;
        for tree in self.trees.clone() {
            if !tree.exists() {
                continue;
            }
            let upper = self.staging.join(format!("upper-{}", safe_name(&tree)));
            let work = self.staging.join(format!("work-{}", safe_name(&tree)));
            let _ = std::fs::remove_dir_all(&work);
            create_dir_all_err(&upper)?;
            create_dir_all_err(&work)?;
            crate::user::chown_to_agent_user(&upper);
            crate::user::chown_to_agent_user(&work);
            let opts = format!(
                "lowerdir={},upperdir={},workdir={}",
                tree.display(),
                upper.display(),
                work.display()
            );
            let target = tree.to_str().unwrap_or("/bogus");
            let (ok, err) = match kind {
                OverlayKind::Kernel => {
                    run(&["mount", "-t", "overlay", "overlay", "-o", &opts, target])
                }
                OverlayKind::Fuse => run(&["fuse-overlayfs", "-o", &opts, target]),
            };
            if !ok {
                return Err(OverlayError::Mount(format!(
                    "mount overlay {}: {}",
                    tree.display(),
                    err
                )));
            }
            mounted += 1;
        }
        Ok(mounted)
    }

    #[cfg(not(unix))]
    pub fn mount(&mut self) -> Result<usize, OverlayError> {
        Err(OverlayError::Unsupported(
            "overlayfs quarantine requires unix".into(),
        ))
    }

    /// Enumerate every staged write across all overlaid trees.
    pub fn staged(&self) -> Vec<StagedWrite> {
        let mut out = Vec::new();
        for tree in &self.trees {
            let upper = self.upper_for(tree);
            collect_upper(&upper, tree, &upper, &mut out);
        }
        out
    }

    /// A human-readable manifest of the staged writes, grouped by tree.
    pub fn manifest(&self) -> String {
        let staged = self.staged();
        if staged.is_empty() {
            return "(no staged writes)".to_string();
        }
        let total = staged.len();
        let mut by_tree: BTreeMap<String, Vec<StagedWrite>> = BTreeMap::new();
        for s in staged {
            by_tree
                .entry(s.tree.display().to_string())
                .or_default()
                .push(s);
        }
        let mut lines = vec![format!("staged writes: {total}")];
        for (tree, mut items) in by_tree {
            lines.push(format!("  {tree}:"));
            // Deletions sort last so adds/mods are the first thing the eye hits.
            items.sort_by_key(|s| (s.kind != WriteKind::Deleted, s.rel.to_string_lossy().to_string()));
            for s in items {
                let crit = if self.is_critical(&s.real_path) {
                    " [CRITICAL:denied-on-apply]"
                } else {
                    ""
                };
                lines.push(format!("    [{}] {}{}", s.kind.as_str(), s.rel.display(), crit));
            }
        }
        lines.join("\n")
    }

    /// True when `path` matches a hard-deny critical pattern.
    pub fn is_critical(&self, path: &Path) -> bool {
        critical_matches(&self.critical, path)
    }

    /// Unmount every overlay (lazy) so subsequent writes reach the REAL tree
    /// underneath. Used by `apply` (so committed writes land on the real fs) and
    /// by teardown. The staging layers are kept.
    #[cfg(unix)]
    pub fn suspend(&self) -> Result<(), OverlayError> {
        for tree in &self.trees {
            let _ = run(&["umount", "-l", tree.to_str().unwrap_or("/")]);
        }
        Ok(())
    }
    #[cfg(not(unix))]
    pub fn suspend(&self) -> Result<(), OverlayError> {
        Ok(())
    }

    /// Re-engage every overlay after [`suspend`].
    #[cfg(unix)]
    pub fn resume(&self) -> Result<(), OverlayError> {
        let kind = match overlay_kind() {
            Some(k) => k,
            None => return Err(OverlayError::Unsupported("no overlay available".into())),
        };
        for tree in self.trees.clone() {
            if !tree.exists() {
                continue;
            }
            let upper = self.staging.join(format!("upper-{}", safe_name(&tree)));
            let work = self.staging.join(format!("work-{}", safe_name(&tree)));
            std::fs::create_dir_all(&upper)?;
            std::fs::create_dir_all(&work)?;
            let opts = format!(
                "lowerdir={},upperdir={},workdir={}",
                tree.display(),
                upper.display(),
                work.display()
            );
            let target = tree.to_str().unwrap_or("/bogus");
            let (ok, err) = match kind {
                OverlayKind::Kernel => {
                    run(&["mount", "-t", "overlay", "overlay", "-o", &opts, target])
                }
                OverlayKind::Fuse => run(&["fuse-overlayfs", "-o", &opts, target]),
            };
            if !ok {
                return Err(OverlayError::Mount(format!("re-mount overlay {}: {}", tree.display(), err)));
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    pub fn resume(&self) -> Result<(), OverlayError> {
        Ok(())
    }

    /// Apply every staged, non-critical write to the real filesystem. Critical
    /// writes are left staged (and reported) so they can never silently reach
    /// the real fs — the operator must remove them from the stage explicitly.
    /// Returns the number of applied entries.
    ///
    /// The overlay is currently mounted, so writing `tree` would *stage again*
    /// instead of reaching the real fs. We therefore suspend (lazy-unmount) the
    /// overlay for the duration of the merge so the writes land on the REAL tree,
    /// then re-engage the overlay for continued use.
    pub fn apply(&self) -> Result<usize, OverlayError> {
        #[cfg(unix)]
        let _ = self.suspend();
        let result = (|| -> Result<usize, OverlayError> {
            let mut applied = 0usize;
            let mut blocked = Vec::new();
            for tree in &self.trees {
                let upper = self.upper_for(tree);
                if !upper.exists() {
                    continue;
                }
                let n = merge_upper(&upper, tree, &self.critical, Path::new(""), &mut blocked)?;
                applied += n;
            }
            for p in &blocked {
                eprintln!(
                    "[pir] quarantine: NOT applying critical path (denied on apply): {}",
                    p.display()
                );
            }
            Ok(applied)
        })();
        #[cfg(unix)]
        let _ = self.resume();
        result
    }

    /// Discard all staged writes for the current session: the overlaid trees
    /// stay mounted (the agent keeps working) but the stage is cleared so the
    /// next writes stage fresh.
    pub fn discard(&self) -> Result<(), OverlayError> {
        for tree in &self.trees {
            let upper = self.upper_for(tree);
            if upper.exists() {
                clear_dir(&upper)?;
            }
        }
        Ok(())
    }

    /// Unmount every overlay and remove the staging area. Called at agent exit.
    pub fn teardown(&self) -> Result<(), OverlayError> {
        for tree in &self.trees {
            let _ = run(&["umount", tree.to_str().unwrap_or("/")]);
        }
        let _ = std::fs::remove_dir_all(&self.staging);
        Ok(())
    }
}

/// Register the live quarantine so the `/quarantine` command and shutdown can
/// reach it. Only one agent quarantine exists per process.
pub fn set_active(q: Quarantine) {
    *active_lock().lock().unwrap() = Some(q);
    set_system_quarantine_engaged(true);
}

/// Run `f` against the active quarantine, if one is mounted.
pub fn with_active<R>(f: impl FnOnce(&mut Quarantine) -> R) -> Option<R> {
    active_lock().lock().ok().and_then(|mut g| g.as_mut().map(f))
}

/// A manifest of the active quarantine's staged writes, or a note that none is
/// mounted.
pub fn manifest_active() -> String {
    with_active(|q| q.manifest()).unwrap_or_else(|| "(write-quarantine not active)".to_string())
}

/// Apply the active quarantine's staged writes to the real fs.
pub fn apply_active() -> Result<usize, OverlayError> {
    with_active(|q| q.apply()).unwrap_or(Ok(0))
}

/// Discard the active quarantine's staged writes.
pub fn discard_active() -> Result<(), OverlayError> {
    with_active(|q| q.discard()).unwrap_or(Ok(()))
}

/// Tear down the active quarantine (unmount + remove staging).
pub fn teardown_active() -> Result<(), OverlayError> {
    let r = with_active(|q| q.teardown()).unwrap_or(Ok(()));
    set_system_quarantine_engaged(false);
    r
}

// ===========================================================================
// Truthful engagement status (the /quarantine + security banner authority)
// ===========================================================================

/// A snapshot of which write-quarantine backend is *actually* engaged right
/// now — the source of truth for the `/quarantine` + security-banner state.
/// A `true` field means a real overlay/staging mount (or container rootfs /
/// manifest store) is live and physically intercepting writes; a `false` field
/// means the backend was never mounted, so the in-process guardrail and the
/// mitigation engine are the only enforcement for writes outside those trees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuarantineEngagement {
    /// The selective system-tree overlay (`/etc`, `/usr/local`, `/opt`, `/srv`,
    /// `/var`, `/boot`) is mounted and staging writes to those trees.
    pub system: bool,
    /// The project-scoped overlay over the repo root (with a whitelisted
    /// worktree bound real) is mounted and staging non-whitelisted writes.
    pub project: bool,
    /// The NON-ROOT `$HOME` quarantine (fuse-overlay of `$HOME` + whitelists)
    /// is live.
    pub home: bool,
    /// The directory-rootfs container (root default: the agent chrooted into
    /// its own rootfs dir) is live — every write lands in the rootfs.
    pub container: bool,
    /// The FULL-ROOT overlay container (the whole `/` overlaid) is live.
    pub fullroot: bool,
    /// (Windows) the no-driver manifest staging store was initialised.
    pub staging: bool,
}

impl QuarantineEngagement {
    /// Is ANY physical quarantine backend actually mounted/live right now?
    pub fn any(&self) -> bool {
        self.system || self.project || self.home || self.container || self.fullroot || self.staging
    }

    /// A comma-separated list of the engaged backends, or "(none)".
    pub fn engaged_list(&self) -> String {
        let mut v: Vec<&str> = Vec::new();
        if self.system { v.push("system"); }
        if self.project { v.push("project"); }
        if self.home { v.push("home"); }
        if self.container { v.push("container"); }
        if self.fullroot { v.push("fullroot"); }
        if self.staging { v.push("staging"); }
        if v.is_empty() { "(none)".into() } else { v.join("+") }
    }
}

/// The live engagement state across every write-quarantine backend, queried
/// from the real process-global flags / env (not the policy config flag).
pub fn quarantine_engagement() -> QuarantineEngagement {
    QuarantineEngagement {
        system: system_quarantine_engaged(),
        project: project_quarantine_engaged(),
        home: home_quarantine_engaged(),
        container: container_engaged(),
        fullroot: fullroot_engaged(),
        #[cfg(windows)]
        staging: crate::security::windows::staging::staging_engaged(),
        #[cfg(not(windows))]
        staging: false,
    }
}

/// A human-readable, honest quarantine status line. Reports which backend is
/// actually engaged, and — when none is — states plainly that writes outside
/// the configured trees are NOT physically staged and are enforced by the
/// in-process guardrail + mitigation ask-gate instead (never a silent claim of
/// "quarantine on").
pub fn quarantine_status() -> String {
    let e = quarantine_engagement();
    if e.any() {
        format!("write-quarantine engaged: {}", e.engaged_list())
    } else {
        format!(
            "write-quarantine NOT physically engaged — out-of-tree writes are guarded in-process (Yellow/ask), not staged; \
             engage via a whitelisted worktree (`wt`), or set PIR_QUARANTINE_MODE and run as root to mount the overlay"
        )
    }
}

/// Turn an absolute path into a filesystem-safe token (`/etc` -> `etc`).
fn safe_name(tree: &Path) -> String {
    tree.to_string_lossy()
        .trim_matches(std::path::is_separator)
        .replace(std::path::is_separator, "_")
}

/// Resolve the quarantine staging root: prefer `<agent home>/.pi/agent/
/// quarantine-staging` (creating + chowning it); if that's not creatable (e.g.
/// an unprivileged agent process with a stale root-owned parent), fall back to
/// a user-writable `/tmp/pir-q-<uid>/quarantine-staging`.
pub fn quarantine_staging_base() -> PathBuf {
    if let Some(home) = crate::user::agent_user_home() {
        let p = home.join(".pi").join("agent").join("quarantine-staging");
        if std::fs::create_dir_all(&p).is_ok() {
            crate::user::chown_to_agent_user(&p);
            return p;
        }
    }
    let uid = unsafe { libc::getuid() };
    let base = std::env::temp_dir().join(format!("pir-q-{}", uid)).join("quarantine-staging");
    let _ = std::fs::create_dir_all(&base);
    base
}

/// `create_dir_all` whose io::Error names the path, so a stray EACCES reports
/// *which* staging dir failed instead of a bare "Permission denied".
fn create_dir_all_err(path: &Path) -> Result<(), OverlayError> {
    std::fs::create_dir_all(path).map_err(|e| {
        OverlayError::Io(std::io::Error::new(
            e.kind(),
            format!("create {}: {e}", path.display()),
        ))
    })
}

/// Run a command, returning `(success, stderr-on-failure)`. A spawn failure is
/// reported as `(false, "<reason>")`.
#[cfg(unix)]
fn run(args: &[&str]) -> (bool, String) {
    use std::process::Command;
    let (prog, rest) = args.split_first().unwrap();
    match Command::new(prog).args(rest).output() {
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            (out.status.success(), stderr)
        }
        Err(e) => (false, e.to_string()),
    }
}

#[cfg(unix)]
fn collect_upper(root: &Path, tree: &Path, upper: &Path, out: &mut Vec<StagedWrite>) {
    let Ok(rd) = std::fs::read_dir(upper) else {
        return;
    };
    for ent in rd.flatten() {
        let p = ent.path();
        let Ok(meta) = ent.metadata() else {
            continue;
        };
        if meta.is_dir() {
            collect_upper(root, tree, &p, out);
        } else {
            let rel = match p.strip_prefix(root) {
                Ok(r) => r.to_path_buf(),
                Err(_) => continue,
            };
            let overlay_path = tree.join(&rel);
            let real_path = tree.join(&rel);
            let kind = if is_whiteout(&meta) {
                WriteKind::Deleted
            } else if real_path.exists() {
                WriteKind::Modified
            } else {
                WriteKind::Added
            };
            out.push(StagedWrite {
                tree: tree.to_path_buf(),
                rel,
                overlay_path,
                real_path,
                kind,
            });
        }
    }
}

/// An overlayfs whiteout is a character device with rdev == 0.
#[cfg(unix)]
fn is_whiteout(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::MetadataExt;
    meta.file_type().is_char_device() && meta.rdev() == 0
}

/// Recursively merge an overlay `upperdir` onto the real tree. Whiteouts
/// (char device rdev==0) delete the corresponding real path. Critical-path
/// patterns are matched against the destination real path; matching entries are
/// collected into `blocked` and left untouched on the real fs.
#[cfg(unix)]
fn merge_upper(
    upper: &Path,
    tree: &Path,
    critical: &[String],
    skip: &Path,
    blocked: &mut Vec<PathBuf>,
) -> std::io::Result<usize> {
    let mut count = 0usize;
    merge_recurse(upper, tree, upper, critical, skip, blocked, &mut count)?;
    Ok(count)
}

#[cfg(unix)]
fn merge_recurse(
    root: &Path,
    tree: &Path,
    node: &Path,
    critical: &[String],
    skip: &Path,
    blocked: &mut Vec<PathBuf>,
    count: &mut usize,
) -> std::io::Result<()> {
    use std::os::unix::fs::symlink;
    for ent in std::fs::read_dir(node)?.flatten() {
        let p = ent.path();
        let meta = ent.metadata()?;
        let rel = match p.strip_prefix(root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        // Skip the whitelisted subtree (the agent's real worktree): it is
        // bind-mounted read-write and already present on the real fs, so it
        // must never be re-applied from the staging upper.
        if !skip.as_os_str().is_empty() && (rel == skip || rel.starts_with(skip)) {
            continue;
        }
        let dst = tree.join(&rel);
        if meta.is_dir() {
            std::fs::create_dir_all(&dst)?;
            merge_recurse(root, tree, &node.join(&rel), critical, skip, blocked, count)?;
            continue;
        }
        // Critical paths are never applied to the real fs.
        if critical_matches(critical, &dst) {
            blocked.push(dst);
            continue;
        }
        if is_whiteout(&meta) {
            let _ = std::fs::remove_file(&dst);
            let _ = std::fs::remove_dir(&dst);
            *count += 1;
            continue;
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&p)?;
            let _ = std::fs::remove_file(&dst);
            symlink(&target, &dst)?;
        } else {
            std::fs::copy(&p, &dst)?;
            let perm = meta.permissions();
            let _ = std::fs::set_permissions(&dst, perm);
        }
        *count += 1;
    }
    Ok(())
}

#[cfg(unix)]
fn critical_matches(critical: &[String], dst: &Path) -> bool {
    let s = dst.to_string_lossy();
    critical.iter().any(|p| {
        let p = p.trim();
        if p.is_empty() {
            return false;
        }
        if let Some(stripped) = p.strip_prefix('*') {
            return s.ends_with(stripped);
        }
        if let Some(stripped) = p.strip_suffix('*') {
            return s.starts_with(stripped);
        }
        s == p
    })
}

#[cfg(unix)]
fn clear_dir(dir: &Path) -> std::io::Result<()> {
    for ent in std::fs::read_dir(dir)?.flatten() {
        let p = ent.path();
        let meta = ent.metadata()?;
        if meta.is_dir() {
            std::fs::remove_dir_all(&p)?;
        } else {
            let _ = std::fs::remove_file(&p);
        }
    }
    Ok(())
}

// ===========================================================================
// Project-scoped write-quarantine with a whitelisted worktree
// ===========================================================================

/// True once the project-scoped overlayfs write-quarantine is engaged for this
/// agent. While set, the in-process write guardrail steps aside: the overlay
/// itself intercepts every write outside the whitelisted worktree and stages
/// it, so the guardrail must not also deny those writes (that would block them
/// entirely instead of staging them). See `docs/SECURITY_MODEL.md` §2.3 / §11.
pub static PROJECT_ENGAGED: AtomicBool = AtomicBool::new(false);

pub fn project_quarantine_engaged() -> bool {
    PROJECT_ENGAGED.load(Ordering::SeqCst)
}

/// True once the FULL-ROOT container quarantine is live (the agent is ai-root
/// in its own namespace; the selective overlays are skipped as redundant).
pub fn fullroot_engaged() -> bool {
    std::env::var_os("PIR_AGENT_NS_ROOT")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false)
}

pub fn set_project_quarantine_engaged(v: bool) {
    PROJECT_ENGAGED.store(v, Ordering::SeqCst);
}

/// True once the *system-tree* overlayfs write-quarantine (the default
/// `quarantine` posture over `/etc`, `/usr/local`, `/opt`, `/srv`, `/var`,
/// `/boot`) is engaged. While set, writes to those trees are intercepted by the
/// overlay and staged — the in-process guardrail (`is_system_state`) must step
/// aside so the write reaches the overlay and stages, instead of being hard-
/// denied before it ever gets there. See `docs/SECURITY_MODEL.md` §9.3.
pub static SYSTEM_ENGAGED: AtomicBool = AtomicBool::new(false);

pub fn system_quarantine_engaged() -> bool {
    SYSTEM_ENGAGED.load(Ordering::SeqCst)
}

pub fn set_system_quarantine_engaged(v: bool) {
    SYSTEM_ENGAGED.store(v, Ordering::SeqCst);
}

/// Full-root quarantine mode (default `on`): attempt the rootless-container
/// recipe — overlay the WHOLE `/` in a user+mount+PID namespace so the agent is
/// ai-root (uid 0 inside, unprivileged on the host) and EVERY write stages,
/// with the worktree + essential dirs bind-mounted real. Engages only when the
/// kernel can host it (native Linux generally can; the WSL2 Microsoft kernel
/// refuses in-kernel overlay-in-userns and fuse-overlayfs can't host /sys//dev,
/// so it falls back to the selective overlays with a clear banner).
/// Which write-quarantine mode to engage (env `PIR_QUARANTINE_MODE`):
/// `fullroot` | `selective` | `auto-writable` | `off`. Default: **selective**
/// when the launcher is root (the shipped posture), **auto-writable** when
/// unprivileged (fuse-overlay of $HOME + user-writable surfaces; see
/// docs/NONROOT_SECURITY.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuarantineMode {
    Fullroot,
    Selective,
    /// Directory-rootfs container: the root default on kernels that refuse
    /// overlay-of-`/` (WSL2). The agent's whole `/` is a directory (rootfs) of
    /// RO toolchain binds + RW whitelist binds + its own writable dirs; every
    /// write lands in the rootfs (the quarantine = the rootfs dir), none reach
    /// the host outside the whitelist. Escape-able (root stays in the init user
    /// namespace) but TOTAL — no coverage gap. Apply = copy accepted rootfs
    /// files to the real host; discard = wipe the rootfs.
    Container,
    AutoWritable,
    Off,
}

#[cfg(unix)]
fn euid_is_zero() -> bool {
    unsafe { libc::geteuid() == 0 }
}
#[cfg(not(unix))]
fn euid_is_zero() -> bool {
    true
}

pub fn resolve_mode() -> QuarantineMode {
    let v = std::env::var("PIR_QUARANTINE_MODE").unwrap_or_default().to_ascii_lowercase();
    match v.trim() {
        "fullroot" | "full-root" => QuarantineMode::Fullroot,
        "container" | "rootfs" => QuarantineMode::Container,
        "selective" | "trees" => QuarantineMode::Selective,
        "off" | "none" | "0" => QuarantineMode::Off,
        "auto-writable" | "auto" | "home" => QuarantineMode::AutoWritable,
        _ => {
            // unset/unknown: root -> directory-rootfs container (escape-able but
            // TOTAL: every write lands in a rootfs dir, none on the host outside
            // the whitelist); non-root -> auto-writable ($HOME overlay).
            if euid_is_zero() {
                QuarantineMode::Container
            } else {
                QuarantineMode::AutoWritable
            }
        }
    }
}

/// True when the operator explicitly opted into the full-root overlay container.
pub fn fullroot_wanted() -> bool {
    resolve_mode() == QuarantineMode::Fullroot
}

/// True when the directory-rootfs container should engage (the root default).
pub fn container_wanted() -> bool {
    euid_is_zero() && resolve_mode() == QuarantineMode::Container
}

/// True once the directory-rootfs container is live.
pub fn container_engaged() -> bool {
    std::env::var_os("PIR_AGENT_NS_CONTAINER")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false)
}

/// Where the container rootfs lives (staging path).
pub fn container_rootfs_path() -> PathBuf {
    quarantine_staging_base().join("container-rootfs")
}

/// True when the NON-ROOT "$HOME + user-writable surfaces" quarantine should
/// engage (the default posture for unprivileged launchers).
pub fn home_quarantine_wanted() -> bool {
    !euid_is_zero() && resolve_mode() == QuarantineMode::AutoWritable
}

/// True once the home quarantine is live (fuse-overlay over $HOME; the
/// selective system overlay is then redundant/skipped).
pub fn home_quarantine_engaged() -> bool {
    std::env::var_os("PIR_AGENT_NS_HOME")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false)
}

/// True when the operator has not opted out of the project write-quarantine
/// (`PIR_QUARANTINE=0` disables it). The launcher still needs `can_mount()`
/// (root + overlayfs) before the overlay is actually engaged.
pub fn project_quarantine_wanted() -> bool {
    std::env::var_os("PIR_QUARANTINE")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(true)
}

/// Project-scoped overlayfs write-quarantine with a single whitelisted worktree.
///
/// The agent's view of the *repo root* is an overlay (`lowerdir` = the real
/// root, `upperdir` = a private staging dir). Every write the agent makes
/// outside its whitelisted worktree therefore lands in `upper` and is visible
/// *only* to the agent — the real filesystem is untouched until the operator
/// reviews + applies it (`/quarantine apply`). The agent's own git worktree is
/// bind-mounted read-write ON TOP of the overlay, so it is the one tree the
/// agent can write to the real filesystem through. The central `.git`, the
/// trunk checkout, and every other agent's worktree are *not* whitelisted, so
/// writes to them are quarantined. This is exactly the posture described in the
/// task: "run all commands, but intercept and quarantine non-whitelisted
/// WRITES where only the agent can see them … the agent's own worktree should
/// be white-listed, but not the central .git or other worktrees." Engaged from
/// the `wt` extension the moment a worktree is created.
pub struct ProjectQuarantine {
    pub root: PathBuf,
    pub whitelist: PathBuf,
    pub whitelist_rel: PathBuf,
    pub staging: PathBuf,
    pub upper: PathBuf,
    pub work: PathBuf,
    /// Staging mountpoint where the overlay is built *before* being promoted
    /// over `root` (`--rbind`), so the real lower (incl. the real worktree)
    /// stays reachable by path for the whitelist bind.
    pub merged: PathBuf,
    /// Additional real paths bound read-write into the overlay view (beyond the
    /// main `whitelist`) — e.g. ~/.cargo, ~/.pi, gh config inside a quarantined
    /// HOME. Their writes are real (never staged), and `apply` naturally skips
    /// them (their upper subtrees stay empty).
    pub extra_whitelists: Vec<PathBuf>,
    pub critical: Vec<String>,
}

impl ProjectQuarantine {
    pub fn new(root: &Path, whitelist: &Path, staging: &Path) -> Self {
        let whitelist_rel = whitelist
            .strip_prefix(root)
            .unwrap_or_else(|_| Path::new("."))
            .to_path_buf();
        let upper = staging.join("upper");
        let work = staging.join("work");
        let merged = staging.join("merged");
        ProjectQuarantine {
            root: root.to_path_buf(),
            whitelist: whitelist.to_path_buf(),
            whitelist_rel,
            staging: staging.to_path_buf(),
            upper,
            work,
            merged,
            extra_whitelists: Vec::new(),
            critical: Vec::new(),
        }
    }

    #[cfg(unix)]
    pub fn mount(&mut self) -> Result<(), OverlayError> {
        let kind = match overlay_kind() {
            Some(k) => k,
            None => {
                return Err(OverlayError::Unsupported(
                    "no overlay available (need root or fuse-overlayfs)".into(),
                ))
            }
        };
        create_dir_all_err(&self.staging)?;
        create_dir_all_err(&self.upper)?;
        // The overlay WORKDIR must be empty; leftover scratch from a prior run
        // breaks remount ("cannot open workdir"). Clear only the transient work
        // dir — the UPPER (staged writes under review) is preserved.
        let _ = std::fs::remove_dir_all(&self.work);
        create_dir_all_err(&self.work)?;
        crate::user::chown_to_agent_user(&self.staging);
        crate::user::chown_to_agent_user(&self.upper);
        crate::user::chown_to_agent_user(&self.work);
        // 1) Overlay the repo root at a STAGING mountpoint (`merged`), never
        //    directly over `root`: this keeps the real lower — including the
        //    real worktree — reachable by its own path, which step 2 needs as
        //    the bind source. lower = real root; writes outside the whitelist
        //    stage into `upper` and are hidden from the real fs.
        create_dir_all_err(&self.merged)?;
        crate::user::chown_to_agent_user(&self.merged);
        let opts = format!(
            "lowerdir={},upperdir={},workdir={}",
            self.root.display(),
            self.upper.display(),
            self.work.display()
        );
        let mnt = self.merged.to_str().unwrap_or("/");
        let (ok, err) = match kind {
            OverlayKind::Kernel => {
                run(&["mount", "-t", "overlay", "overlay", "-o", &opts, mnt])
            }
            OverlayKind::Fuse => run(&["fuse-overlayfs", "-o", &opts, mnt]),
        };
        if !ok {
            return Err(OverlayError::Mount(format!("overlay {}: {}", self.merged.display(), err)));
        }
        // 2) Bind-mount the agent's REAL worktree into the overlay view so it is
        //    the only tree the agent can write to the real fs through.
        let mtarget = self.merged.join(&self.whitelist_rel);
        std::fs::create_dir_all(&mtarget)?;
        crate::user::chown_to_agent_user(&mtarget);
        let (ok, err) = run(&[
            "mount",
            "--bind",
            self.whitelist.to_str().unwrap_or("/"),
            mtarget.to_str().unwrap_or("/"),
        ]);
        if !ok {
            // Overlay is still up; without the bind the worktree would itself
            // stage (less safe, not breaking). Degrade gracefully.
            eprintln!("[pir] project-quarantine: worktree bind failed: {err}");
        }
        // Extra whitelists (e.g. ~/.cargo, ~/.pi inside a quarantined HOME).
        for extra in &self.extra_whitelists {
            let rel = extra.strip_prefix(&self.root).unwrap_or(extra);
            let et = self.merged.join(rel);
            let _ = std::fs::create_dir_all(&et);
            let (ok, err) = run(&[
                "mount",
                "--bind",
                extra.to_str().unwrap_or("/"),
                et.to_str().unwrap_or("/"),
            ]);
            if !ok {
                eprintln!("[pir] project-quarantine: extra whitelist bind failed ({}): {err}", extra.display());
            }
        }
        // 3) Promote the merged view over the repo root. `--rbind` (recursive)
        //    carries the inner worktree bind along; a plain `--bind` would drop
        //    it and the worktree would silently stage instead of being real.
        let (ok, err) = run(&["mount", "--rbind", mnt, self.root.to_str().unwrap_or("/")]);
        if !ok {
            return Err(OverlayError::Mount(format!("promote {}: {}", self.root.display(), err)));
        }
        Ok(())
    }

    #[cfg(not(unix))]
    pub fn mount(&mut self) -> Result<(), OverlayError> {
        Err(OverlayError::Unsupported("project quarantine requires unix".into()))
    }

    /// Unmount the overlay + worktree bind (lazy) so system-level git ops (merge,
    /// remove) run against the *real* root. The staging layers are kept.
    #[cfg(unix)]
    pub fn suspend(&self) -> Result<(), OverlayError> {
        let mtarget = self.merged.join(&self.whitelist_rel);
        let _ = run(&["umount", "-l", self.root.to_str().unwrap_or("/")]);
        let _ = run(&["umount", "-l", mtarget.to_str().unwrap_or("/")]);
        for extra in &self.extra_whitelists {
            let rel = extra.strip_prefix(&self.root).unwrap_or(extra);
            let _ = run(&["umount", "-l", self.merged.join(rel).to_str().unwrap_or("/")]);
        }
        let _ = run(&["umount", "-l", self.merged.to_str().unwrap_or("/")]);
        Ok(())
    }
    #[cfg(not(unix))]
    pub fn suspend(&self) -> Result<(), OverlayError> {
        Ok(())
    }

    /// Re-engage the overlay + worktree bind after [`suspend`].
    #[cfg(unix)]
    pub fn resume(&self) -> Result<(), OverlayError> {
        let kind = match overlay_kind() {
            Some(k) => k,
            None => return Err(OverlayError::Unsupported("no overlay available".into())),
        };
        std::fs::create_dir_all(&self.merged)?;
        let opts = format!(
            "lowerdir={},upperdir={},workdir={}",
            self.root.display(),
            self.upper.display(),
            self.work.display()
        );
        let mnt = self.merged.to_str().unwrap_or("/");
        let (ok, err) = match kind {
            OverlayKind::Kernel => {
                run(&["mount", "-t", "overlay", "overlay", "-o", &opts, mnt])
            }
            OverlayKind::Fuse => run(&["fuse-overlayfs", "-o", &opts, mnt]),
        };
        if !ok {
            return Err(OverlayError::Mount(format!("re-mount overlay {}: {}", self.merged.display(), err)));
        }
        let mtarget = self.merged.join(&self.whitelist_rel);
        std::fs::create_dir_all(&mtarget)?;
        let (ok, err) = run(&[
            "mount",
            "--bind",
            self.whitelist.to_str().unwrap_or("/"),
            mtarget.to_str().unwrap_or("/"),
        ]);
        if !ok {
            eprintln!("[pir] project-quarantine: worktree re-bind failed: {err}");
        }
        for extra in &self.extra_whitelists {
            let rel = extra.strip_prefix(&self.root).unwrap_or(extra);
            let et = self.merged.join(rel);
            let _ = std::fs::create_dir_all(&et);
            let (ok, err) = run(&[
                "mount",
                "--bind",
                extra.to_str().unwrap_or("/"),
                et.to_str().unwrap_or("/"),
            ]);
            if !ok {
                eprintln!("[pir] project-quarantine: extra whitelist re-bind failed ({}): {err}", extra.display());
            }
        }
        let (ok, err) = run(&["mount", "--rbind", mnt, self.root.to_str().unwrap_or("/")]);
        if !ok {
            return Err(OverlayError::Mount(format!("re-promote {}: {}", self.root.display(), err)));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    pub fn resume(&self) -> Result<(), OverlayError> {
        Ok(())
    }

    /// Enumerate staged writes (everything outside the whitelisted worktree).
    pub fn staged(&self) -> Vec<StagedWrite> {
        let mut out = Vec::new();
        collect_upper(&self.upper, &self.root, &self.upper, &mut out);
        out.retain(|s| !under(&s.rel, &self.whitelist_rel));
        out
    }

    pub fn manifest(&self) -> String {
        let staged = self.staged();
        if staged.is_empty() {
            return "(no staged writes outside the worktree)".to_string();
        }
        let total = staged.len();
        let mut lines = vec![format!("staged writes (quarantined, agent-only view): {total}")];
        for s in staged {
            let crit = if self.is_critical(&s.real_path) {
                " [CRITICAL:denied-on-apply]"
            } else {
                ""
            };
            lines.push(format!("    [{}] {}{}", s.kind.as_str(), s.rel.display(), crit));
        }
        lines.join("\n")
    }

    pub fn is_critical(&self, path: &Path) -> bool {
        critical_matches(&self.critical, path)
    }

    /// Apply every staged, non-critical write to the real filesystem. The
    /// whitelisted worktree is skipped (it is already real). Critical writes are
    /// left staged. Returns the number applied.
    ///
    /// The overlay is mounted over `root`; writing `root` would *stage again*
    /// instead of reaching the real fs. Suspend (lazy-unmount) for the merge so
    /// the writes land on the REAL root, then re-engage the overlay.
    pub fn apply(&self) -> Result<usize, OverlayError> {
        #[cfg(unix)]
        let _ = self.suspend();
        let result = (|| -> Result<usize, OverlayError> {
            let mut blocked = Vec::new();
            let applied = merge_upper(
                &self.upper,
                &self.root,
                &self.critical,
                &self.whitelist_rel,
                &mut blocked,
            )?;
            for p in &blocked {
                eprintln!(
                    "[pir] quarantine: NOT applying critical path (denied on apply): {}",
                    p.display()
                );
            }
            Ok(applied)
        })();
        #[cfg(unix)]
        let _ = self.resume();
        result
    }

    /// Discard all staged writes (the agent keeps working; the stage is cleared).
    pub fn discard(&self) -> Result<(), OverlayError> {
        clear_dir(&self.upper)?;
        Ok(())
    }

    /// Unmount the overlay + bind and remove the staging area.
    pub fn teardown(&self) -> Result<(), OverlayError> {
        let _ = self.suspend();
        let _ = std::fs::remove_dir_all(&self.staging);
        Ok(())
    }
}

/// True when `rel` lives under `base` (or equals it).
fn under(rel: &Path, base: &Path) -> bool {
    if base.as_os_str().is_empty() {
        return false;
    }
    rel == base || rel.starts_with(base)
}

static ACTIVE_PROJECT: OnceLock<Mutex<Option<ProjectQuarantine>>> = OnceLock::new();

fn active_project_lock() -> &'static Mutex<Option<ProjectQuarantine>> {
    ACTIVE_PROJECT.get_or_init(|| Mutex::new(None))
}

fn with_project<R>(f: impl FnOnce(&ProjectQuarantine) -> R) -> Option<R> {
    active_project_lock().lock().ok().and_then(|g| g.as_ref().map(f))
}

/// Engage the project-scoped overlayfs write-quarantine: overlay `root` with a
/// private staging upper and bind-mount `whitelist` (the agent's worktree)
/// read-write on top. The real fs is untouched outside the worktree until the
/// operator reviews (`project_active_manifest` / `/quarantine`).
///
/// The overlays are scoped to THIS agent's private mount namespace (see
/// `enter_private_mount_ns`) so only the agent sees the staged view — the host
/// keeps its real `/var`, `/etc`, and repos. Refuses to mount if a private
/// namespace can't be obtained, rather than shadowing the host's filesystems.
pub fn mount_project_quarantine(root: &Path, whitelist: &Path) -> Result<(), OverlayError> {
    // ROOT default (container mode): directory-rootfs container. Escape-able but
    // TOTAL — every write lands in the rootfs dir; nothing outside the whitelist
    // reaches the real host. Falls back to the selective project overlay if the
    // container can't be established.
    if container_wanted() {
        match mount_rootfs_container(root, whitelist) {
            Ok(()) => {
                eprintln!(
                    "{}",
                    crate::term::dim(&format!(
                        "[pir] container quarantine engaged: agent confined to {}",
                        container_rootfs_path().display()
                    ))
                );
                eprintln!(
                    "{}",
                    crate::term::dim(
                        "       (every write lands in the rootfs; whitelisted worktree + ~/.cargo + ~/.pi real; review/apply with /quarantine; escape-able like the selective posture, but with NO coverage gap)."
                    )
                );
                return Ok(());
            }
            Err(e) => {
                eprintln!(
                    "{}",
                    crate::term::red(&format!(
                        "[pir] container quarantine unavailable ({e}); falling back to the selective overlays"
                    ))
                );
            }
        }
    }
    // Non-root auto-writable: the $HOME quarantine (mounted later, in the
    // security block) covers the repo when it is under $HOME, so the selective
    // project overlay is redundant here. If the repo is OUTSIDE $HOME, keep the
    // selective project overlay below.
    if home_quarantine_wanted() {
        if let Some(h) = std::env::var_os("HOME").map(PathBuf::from) {
            if root.starts_with(&h) {
                return Ok(());
            }
        }
    }
    // FULL-ROOT container quarantine (PIR_QUARANTINE_MODE=fullroot):
    // overlay the whole / in a user+mount+PID namespace so the agent is ai-root
    // and every write stages. Engages only when the kernel can host it; on
    // kernels that can't (WSL2: no in-kernel overlay-in-userns, fuse can't host
    // the pseudo-fs) it falls back to the selective overlays + UID isolation.
    if fullroot_wanted() {
        let staging = quarantine_staging_base();
        match try_full_root_quarantine(root, whitelist, &staging) {
            Ok(()) => {
                eprintln!(
                    "{}",
                    crate::term::dim(
                        "[pir] FULL-ROOT write-quarantine engaged: agent is ai-root in a container; every write stages (review with /quarantine)"
                    )
                );
                return Ok(());
            }
            Err(e) => {
                // Fall back to the selective project overlay + UID isolation.
                // (Do NOT return early; the caller still wants the worktree.)
                eprintln!(
                    "{}",
                    crate::term::yellow(&format!(
                        "[pir] full-root quarantine unavailable on this kernel ({e}); using selective overlays + UID isolation."
                    ))
                );
            }
        }
    }
    if enter_private_mount_ns().is_err() {
        return Err(OverlayError::Unsupported(
            "private mount namespace unavailable; refusing to quarantine globally".into(),
        ));
    }
    let staging = quarantine_staging_base().join("project");
    let mut q = ProjectQuarantine::new(root, whitelist, &staging);
    q.mount()?;
    *active_project_lock().lock().unwrap() = Some(q);
    set_project_quarantine_engaged(true);
    Ok(())
}

/// Engage the ROOT directory-rootfs container (the default root posture on
/// kernels that refuse overlay-of-`/`, i.e. WSL2). Builds a rootfs directory of
/// RO toolchain binds + RW whitelist binds + the agent's own writable dirs (a
/// copy of /etc, own /root //tmp //var //opt //srv //usr/local//home), then
/// `chroot`s the (root) agent process into it. Every write lands in the rootfs;
/// nothing outside the whitelist reaches the real host. Escape-able (root in the
/// init user namespace) but TOTAL.
pub fn mount_rootfs_container(root: &Path, whitelist: &Path) -> Result<(), OverlayError> {
    let run = |args: &[&str]| -> Result<(), OverlayError> {
        let (ok, err) = crate::security::overlay::run(args);
        if ok {
            Ok(())
        } else {
            Err(OverlayError::Mount(err))
        }
    };
    if enter_private_mount_ns().is_err() {
        return Err(OverlayError::Unsupported("private mount namespace unavailable".into()));
    }
    // Own PID namespace BEFORE mounting /proc: the container's /proc must show
    // only its own processes. Binding the HOST /proc would expose /proc/1/root
    // — a magic symlink to the host root fs, which root + CAP_DAC_OVERRIDE could
    // read/write straight through (the single most practical escape). An owned
    // PID ns has no pid-1 pointing at the host init.
    #[cfg(unix)]
    {
        let r = unsafe { libc::unshare(libc::CLONE_NEWPID) };
        if r != 0 {
            return Err(OverlayError::Unsupported("pid namespace unavailable".into()));
        }
    }
    let _ = run(&["mount", "--make-rprivate", "/"]);
    let rootfs = container_rootfs_path();
    let _ = std::fs::remove_dir_all(&rootfs);
    std::fs::create_dir_all(&rootfs).map_err(OverlayError::from)?;
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/root"));
    let agent = crate::user::agent_user_home().unwrap_or_else(|| home.clone());
    for d in ["usr", "lib", "lib64", "bin", "sbin", "proc", "sys", "dev", "run", "tmp", "var", "root", "opt", "srv"] {
        let _ = std::fs::create_dir_all(rootfs.join(d));
    }
    std::fs::create_dir_all(rootfs.join("usr/local")).ok();
    std::fs::create_dir_all(rootfs.join(format!("home/{}", agent.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "agent".into())))).ok();
    // The container's OWN /etc (a copy): edits here are the reviewable delta.
    let _ = run(&["cp", "-a", "/etc/.", rootfs.join("etc").to_str().ok_or_else(|| OverlayError::Unsupported("etc path".into()))?]);
    // Baseline snapshot of the copied /etc: lets us detect the agent's DELETES
    // (a file that existed at start but is now missing from the container, while
    // the real host file still exists) as reviewable delete-intents (tombstones).
    let mut baseline = Vec::new();
    collect_baseline(&rootfs.join("etc"), &mut baseline);
    if let Ok(mut b) = std::fs::File::create(container_baseline_path()) {
        use std::io::Write;
        let _ = b.write_all(baseline.join("\n").as_bytes());
    }
    // Read-only toolchain binds (reads pass through; writes are container-only).
    for d in ["usr", "lib", "lib64", "bin", "sbin"] {
        let src = PathBuf::from("/").join(d);
        if src.exists() {
            let _ = run(&["mount", "--bind", "-o", "ro", src.to_str().unwrap_or("/"), rootfs.join(d).to_str().unwrap_or("/")]);
        }
    }
    // FRESH /proc from our own PID ns (not a host bind — see CLONE_NEWPID above).
    let _ = run(&["mount", "-t", "proc", "proc", rootfs.join("proc").to_str().unwrap_or("/")]);
    // sysfs info only (enumeration); writes need SYS_ADMIN-family caps (dropped).
    let _ = run(&["mount", "-t", "sysfs", "sysfs", "-o", "ro", rootfs.join("sys").to_str().unwrap_or("/")]);
    // MINIMAL /dev (tmpfs + the few nodes tools need) — NEVER the host's /dev:
    // root + CAP_DAC_OVERRIDE could otherwise open /dev/sda etc. and dd the
    // host disks (incl. the Windows drive through WSL).
    let _ = run(&["mount", "-t", "tmpfs", "tmpfs", rootfs.join("dev").to_str().unwrap_or("/")]);
    for node in ["null", "zero", "full", "random", "urandom", "tty"] {
        let _ = run(&["mknod", rootfs.join("dev").join(node).to_str().unwrap_or("/dev/null"), "c", "1", node_kind(node)]);
    }
    let _ = run(&["mount", "-t", "tmpfs", "tmpfs", rootfs.join("run").to_str().unwrap_or("/")]);
    std::fs::create_dir_all(rootfs.join("dev/pts")).ok();
    std::fs::create_dir_all(rootfs.join("dev/shm")).ok();
    let _ = run(&["mount", "-t", "devpts", "devpts", rootfs.join("dev/pts").to_str().unwrap_or("/")]);
    let _ = run(&["mount", "-t", "tmpfs", "tmpfs", rootfs.join("dev/shm").to_str().unwrap_or("/")]);
    let _ = run(&["mount", "-t", "tmpfs", "tmpfs", rootfs.join("tmp").to_str().unwrap_or("/")]);
    // Whitelists (REAL, RW): the agent's own essential dirs.
    for w in [agent.join(".cargo"), agent.join(".pi")] {
        let rel = format!("home/{}/{}", agent.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), w.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        let tgt = rootfs.join(&rel);
        let _ = std::fs::create_dir_all(&tgt);
        let _ = run(&["mount", "--bind", w.to_str().unwrap_or("/"), tgt.to_str().unwrap_or("/")]);
    }
    // Repo: RO bind (trunk read-only), worktree RW bind over it (the whitelist).
    let repo_rel = root.strip_prefix("/").unwrap_or(root);
    let rt = rootfs.join(repo_rel);
    let _ = std::fs::create_dir_all(&rt);
    let _ = run(&["mount", "--bind", "-o", "ro", root.to_str().unwrap_or("/"), rt.to_str().unwrap_or("/")]);
    let wt_rel = whitelist.strip_prefix("/").unwrap_or(whitelist);
    let wtt = rootfs.join(wt_rel);
    let _ = std::fs::create_dir_all(&wtt);
    let _ = run(&["mount", "--bind", whitelist.to_str().unwrap_or("/"), wtt.to_str().unwrap_or("/")]);
    // chroot the (root) process into the container; cwd = the whitelisted worktree.
    #[cfg(unix)]
    {
        let c = std::ffi::CString::new(rootfs.as_os_str().as_encoded_bytes()).map_err(|_| OverlayError::Unsupported("bad rootfs path".into()))?;
        if unsafe { libc::chroot(c.as_ptr()) } != 0 {
            return Err(OverlayError::Unsupported(format!("chroot {}: {}", rootfs.display(), std::io::Error::last_os_error())));
        }
    }
    let _ = std::env::set_current_dir(PathBuf::from("/").join(wt_rel));
    std::env::set_var("PIR_AGENT_NS_CONTAINER", "1");
    set_project_quarantine_engaged(true);
    set_system_quarantine_engaged(true);
    Ok(())
}

/// char-device major:minor for the minimal /dev nodes.
fn node_kind(name: &str) -> &'static str {
    match name {
        "null" => "1 3",
        "zero" => "1 5",
        "full" => "1 7",
        "random" => "1 8",
        "urandom" => "1 9",
        "tty" => "5 0",
        _ => "1 3",
    }
}

fn container_baseline_path() -> PathBuf {
    quarantine_staging_base().join("container-baseline")
}

/// Collect the real-host-relative paths ("/etc/hosts") of every file under the
/// container's copied /etc — the delete-tombstone baseline.
fn collect_baseline(dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for ent in rd.flatten() {
        let p = ent.path();
        let Ok(meta) = ent.metadata() else { continue };
        if meta.is_dir() {
            collect_baseline(&p, out);
        } else if let Some(rel) = p.strip_prefix(container_rootfs_path()).ok() {
            out.push(format!("/{}", rel.display()));
        }
    }
}

/// The container's OWN (non-bind) dirs whose files are reviewable: writable
/// copies/scratch that mirror host paths; whitelists (worktree, ~/.cargo,
/// ~/.pi) and read-only binds are excluded.
fn container_own_dirs(rootfs: &Path, agent: &Path) -> Vec<PathBuf> {
    let agent_name = agent.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut dirs = vec![
        rootfs.join("etc"),
        rootfs.join("root"),
        rootfs.join("opt"),
        rootfs.join("srv"),
        rootfs.join("usr/local"),
    ];
    dirs.push(rootfs.join(format!("home/{agent_name}")));
    dirs
}

/// All staged container writes: creates/modifies from the own dirs, plus
/// deletes (baseline /etc files missing from the container, real host file
/// still present). Sorted by real path so indices align everywhere.
fn container_entries(rootfs: &Path, agent: &Path) -> Vec<(PathBuf, crate::security::rules::ReviewOp)> {
    let mut staged = Vec::new();
    for dir in container_own_dirs(rootfs, agent) {
        walk_own_dir(rootfs, &dir, &mut staged);
    }
    // Tombstones: /etc files present at container start, gone now, host still has them.
    if let Ok(raw) = std::fs::read_to_string(container_baseline_path()) {
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() { continue; }
            let rel = line.trim_start_matches('/');
            let src = rootfs.join(rel);
            if !src.exists() && Path::new(line).exists() {
                staged.push((PathBuf::from(line), crate::security::rules::ReviewOp::Delete));
            }
        }
    }
    staged.sort_by_key(|(real, _)| real.to_string_lossy().to_string());
    staged
}

/// List the agent's staged (container-owned) writes as a numbered, reviewable
/// manifest: each file in the container's own dirs that differs from the real
/// host path (added or modified). `apply` copies them to the real host; the
/// whitelists and read-only toolchain binds are never listed.
pub fn container_manifest() -> String {
    let rootfs = container_rootfs_path();
    let agent = crate::user::agent_user_home().unwrap_or_else(|| std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/root")));
    let staged = container_entries(&rootfs, &agent);
    if staged.is_empty() {
        return "(container: no staged writes to review)".to_string();
    }
    let mut lines = vec![format!("container staged writes ({}):", staged.len())];
    for (i, (real, op)) in staged.iter().enumerate() {
        let auto = match crate::security::rules::evaluate(*op, &real.to_string_lossy()) {
            Some(crate::security::rules::RuleVerdict::Deny) => " [auto-deny]",
            Some(crate::security::rules::RuleVerdict::Approve) => " [auto-approve]",
            None => "",
        };
        let irreversible = if *op == crate::security::rules::ReviewOp::Delete {
            " (irreversible)"
        } else {
            ""
        };
        lines.push(format!("  [{}] {:<6}{}{}{}", i, op.as_str(), real.display(), auto, irreversible));
    }
    lines.push("  approve: /quarantine apply [n|all]   ·   discard: /quarantine discard [n|all]".to_string());
    lines.push("  deletes are IRREVERSIBLE on apply    ·   regex rules: ~/.pi/agent/quarantine-rules".to_string());
    lines.join("\n")
}

fn walk_own_dir(rootfs: &Path, dir: &Path, out: &mut Vec<(PathBuf, crate::security::rules::ReviewOp)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for ent in rd.flatten() {
        let p = ent.path();
        let Ok(meta) = ent.metadata() else { continue };
        if meta.is_dir() {
            walk_own_dir(rootfs, &p, out);
        } else {
            let rel = match p.strip_prefix(rootfs) { Ok(r) => r.to_path_buf(), Err(_) => continue };
            let real = std::fs::canonicalize("/").unwrap_or_else(|_| PathBuf::from("/")).join(&rel);
            if rel.starts_with("home") && (rel.to_string_lossy().contains("/.cargo") || rel.to_string_lossy().contains("/.pi")) {
                continue; // whitelist
            }
            if out.len() >= 200 { return; }
            let added = !real.exists();
            let modified = !added && std::fs::read(&p).ok() != std::fs::read(&real).ok();
            if added || modified {
                let op = if added {
                    crate::security::rules::ReviewOp::Create
                } else {
                    crate::security::rules::ReviewOp::Modify
                };
                out.push((real, op));
            }
        }
    }
}

/// The real host path of the staged write at `idx` (the manifest ordering), if
/// any — used by `/quarantine apply r <idx> <regex>`.
pub fn container_entry_path(idx: usize) -> Option<PathBuf> {
    let rootfs = container_rootfs_path();
    let agent = crate::user::agent_user_home().unwrap_or_else(|| std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/root")));
    container_entries(&rootfs, &agent).get(idx).map(|(real, _)| real.clone())
}

/// Apply the container's staged writes to the real host, optionally just one
/// (by its zero-based index). `apply all` (None) SKIPS auto-DENIED entries
/// (rules: DENY on the path means the write stays staged — e.g. new cache dies
/// with the container); an explicit index applies regardless (the operator's
/// explicit override). Returns (applied, auto-denied).
pub fn container_apply(only: Option<usize>) -> Result<(usize, usize), OverlayError> {
    let rootfs = container_rootfs_path();
    let agent = crate::user::agent_user_home().unwrap_or_else(|| std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/root")));
    let staged = container_entries(&rootfs, &agent);
    let mut applied = 0usize;
    let mut denied = 0usize;
    for (i, (real, op)) in staged.into_iter().enumerate() {
        if let Some(n) = only { if i != n { continue; } }
        // Auto-deny on `all` (explicit single apply overrides).
        if only.is_none()
            && matches!(crate::security::rules::evaluate(op, &real.to_string_lossy()),
                Some(crate::security::rules::RuleVerdict::Deny))
        {
            denied += 1;
            continue;
        }
        match op {
            // Delete-intent tombstone: perform the real host removal (irreversible).
            crate::security::rules::ReviewOp::Delete => {
                let _ = std::fs::remove_file(&real);
                let _ = std::fs::remove_dir(&real);
                applied += 1;
            }
            _ => {
                let rel = real.strip_prefix("/").unwrap_or(&real);
                let src = rootfs.join(rel);
                if let Some(parent) = real.parent() { let _ = std::fs::create_dir_all(parent); }
                if std::fs::copy(&src, &real).is_ok() { applied += 1; }
            }
        }
    }
    Ok((applied, denied))
}

/// Discard the container's staged writes, optionally just one entry. Discarding
/// all wipes the container rootfs (the session's whole staged state) — the
/// real host is untouched either way.
pub fn container_discard(only: Option<usize>) -> Result<usize, OverlayError> {
    if only.is_none() {
        let _ = std::fs::remove_dir_all(container_rootfs_path());
        return Ok(usize::MAX);
    }
    let n = only.unwrap();
    let rootfs = container_rootfs_path();
    let agent = crate::user::agent_user_home().unwrap_or_else(|| std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/root")));
    let staged = container_entries(&rootfs, &agent);
    if let Some((real, op)) = staged.get(n) {
        match op {
            // Discarding a delete-intent: the real host file is untouched (the
            // container copy is already gone).
            crate::security::rules::ReviewOp::Delete => Ok(1),
            _ => {
                let rel = real.strip_prefix("/").unwrap_or(real);
                let _ = std::fs::remove_file(rootfs.join(rel));
                Ok(1)
            }
        }
    } else {
        Ok(0)
    }
}

/// Engage the NON-ROOT "$HOME + user-writable surfaces" quarantine (the default
/// for unprivileged launchers, docs/NONROOT_SECURITY.md): fuse-overlay $HOME
/// with the worktree + essential dirs (~/.cargo, ~/.pi, gh config) bound real.
/// Staging lives on /tmp/<uid> (OUTSIDE $HOME, so it isn't inside the overlaid
/// lower). `/tmp` itself is deliberately NOT covered.
pub fn mount_home_quarantine() -> Result<(), OverlayError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| OverlayError::Unsupported("no HOME".into()))?;
    let Some(wt) = std::env::var_os("PIR_WT_WHITELIST").map(PathBuf::from) else {
        return Err(OverlayError::Unsupported(
            "no whitelisted worktree (PIR_WT_WHITELIST unset)".into(),
        ));
    };
    if !wt.starts_with(&home) {
        return Err(OverlayError::Unsupported(
            "worktree is not under $HOME; the home quarantine covers only $HOME (a repo outside $HOME is not overlayed here)".into(),
        ));
    }
    if enter_private_mount_ns().is_err() {
        return Err(OverlayError::Unsupported("private namespace unavailable".into()));
    }
    let uid = unsafe { libc::getuid() };
    let staging = std::env::temp_dir().join(format!("pir-home-q-{}", uid));
    let mut q = ProjectQuarantine::new(&home, &wt, &staging);
    q.extra_whitelists = home_extra_whitelists(&home);
    q.mount()?;
    *active_project_lock().lock().unwrap() = Some(q);
    set_project_quarantine_engaged(true);
    set_system_quarantine_engaged(true);
    std::env::set_var("PIR_AGENT_NS_HOME", "1");
    Ok(())
}

/// Essential dirs under $HOME that must stay REAL (never staged): the cargo
/// cache/registry, ~/.pi (config + session logs — persistence!), and the gh
/// credential config dir. Each is bind-mounted read-write on top of the overlay.
fn home_extra_whitelists(home: &Path) -> Vec<PathBuf> {
    let mut out = vec![home.join(".cargo"), home.join(".pi")];
    match std::env::var_os("GH_CONFIG_DIR").map(PathBuf::from) {
        Some(gh) if gh.starts_with(home) => out.push(gh),
        _ => out.push(home.join(".config").join("gh")),
    }
    out
}

pub fn project_active_manifest() -> String {
    with_project(|q| q.manifest()).unwrap_or_else(|| "(project write-quarantine not active)".into())
}

pub fn project_active_apply() -> Result<usize, OverlayError> {
    with_project(|q| q.apply()).unwrap_or(Ok(0))
}

pub fn project_active_discard() -> Result<(), OverlayError> {
    with_project(|q| q.discard()).unwrap_or(Ok(()))
}

pub fn project_active_suspend() -> Result<(), OverlayError> {
    with_project(|q| q.suspend()).unwrap_or(Ok(()))
}

pub fn project_active_resume() -> Result<(), OverlayError> {
    with_project(|q| q.resume()).unwrap_or(Ok(()))
}

pub fn project_active_teardown() -> Result<(), OverlayError> {
    let r = with_project(|q| q.teardown()).unwrap_or(Ok(()));
    set_project_quarantine_engaged(false);
    r
}

pub fn project_active_engaged() -> bool {
    active_project_lock().lock().map(|g| g.is_some()).unwrap_or(false)
}

/// Attempt the FULL-ROOT container quarantine (see `fullroot_wanted`).
/// Enters user+mount+PID namespaces (ai-root, no-escape), overlays `/`
/// (lower=/, upper+work on tmpfs), mounts the pseudo-fs fresh inside (rebinding
/// where the kernel refuses fresh mounts), binds the whitelist (worktree) real,
/// and promotes the whole view over `/`. Best-effort per mount: if the kernel
/// can't host a piece (e.g. WSL2), returns `Unsupported` so the caller falls
/// back to the selective overlays with a banner. Only meaningful on unix.
#[cfg(unix)]
pub fn try_full_root_quarantine(root: &Path, whitelist: &Path, staging_base: &Path) -> Result<(), OverlayError> {
    
    let run = |args: &[&str]| -> Result<(), OverlayError> {
        let (ok, err) = crate::security::overlay::run(args);
        if ok {
            Ok(())
        } else {
            Err(OverlayError::Mount(err))
        }
    };
    // Enter user + mount namespaces: ai-root inside, unprivileged on the host
    // (cannot setns back to the host user namespace). We deliberately do NOT
    // enter a PID namespace here: `unshare(CLONE_NEWPID)` in a *multithreaded*
    // process permanently breaks `pthread_create` (it returns EINVAL for every
    // subsequent thread), because the caller becomes PID 1 of a fresh PID
    // namespace while the process's other threads belong to the parent
    // namespace. pir is multithreaded (the broadcast watcher, spinner, turn
    // workers, smol reactor, rustyline) — a PID-namespace unshare here would
    // make the very next `thread::spawn` panic with
    //   `failed to spawn thread: Os { code: 22, kind: InvalidInput }`,
    // crashing pir the moment Ctrl-C/ESC queues a turn worker or refreshes the
    // spinner. We instead mount the pseudo-fs by fresh-mounting /proc only when
    // the user namespace allows it; if that fails we return `Unsupported` and
    // the caller falls back to the selective overlays (which never enter a PID
    // namespace and never break thread creation).
    #[cfg(unix)]
    {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let r = unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) };
        if r != 0 {
            return Err(OverlayError::Unsupported("userns/mntns unavailable in this kernel".into()));
        }
        // Map our host uid/gid to 0 inside (ai-root): we own the ns we created,
        // no setuid helper needed for a self-map.
        let _ = std::fs::write("/proc/self/setgroups", b"deny\n");
        let gmap = format!("0 {} 1\n", gid);
        let umap = format!("0 {} 1\n", uid);
        if std::fs::write("/proc/self/gid_map", gmap).is_err()
            || std::fs::write("/proc/self/uid_map", umap).is_err()
        {
            return Err(OverlayError::Unsupported("could not map uid/gid in the user namespace".into()));
        }
    }
    let _ = run(&["mount", "--make-rprivate", "/"]);
    let pq = staging_base.join("fullroot");
    for d in ["upper", "work", "root"] {
        let _ = std::fs::create_dir_all(pq.join(d));
    }
    run(&["mount", "-t", "tmpfs", "tmpfs", pq.join("upper").to_str().unwrap_or("")])?;
    run(&["mount", "-t", "tmpfs", "tmpfs", pq.join("work").to_str().unwrap_or("")])?;
    // Overlay / (whole fs) at the staging root; upper/work MUST be outside /
    // (they're on tmpfs, separate from the lower). fuse-overlayfs is used when
    // in-kernel overlay is refused inside a user namespace (WSL2).
    let over = if in_init_user_ns() && can_mount() {
        run(&["mount", "-t", "overlay", "overlay", "-o",
              &format!("lowerdir=/,upperdir={},workdir={}",
                       pq.join("upper").display(), pq.join("work").display()),
              pq.join("root").to_str().unwrap_or("")])
    } else if fuse_overlayfs_available() {
        run(&["fuse-overlayfs", "-o",
              &format!("lowerdir=/,upperdir={},workdir={}",
                       pq.join("upper").display(), pq.join("work").display()),
              pq.join("root").to_str().unwrap_or("")])
    } else {
        return Err(OverlayError::Unsupported("no overlay backend usable for full-root mode".into()));
    };
    over?;
    // Mount the pseudo-fs FRESH inside the overlay view (bind-into-fuse fails;
    // proc may need CAP_SYS_ADMIN in the userns, which we don't guarantee now
    // that we skip NEWPID to keep thread creation intact). sys/dev best-effort;
    // if the kernel refuses them, bail to the selective fallback rather than
    // ship a broken root (the fallback never enters a PID namespace and so never
    // breaks thread creation).
    for (target, args) in [
        (pq.join("root/proc"), &["-t", "proc", "proc"] as &[&str]),
        (pq.join("root/sys"), &["-t", "sysfs", "sysfs"]),
        (pq.join("root/dev"), &["-t", "devtmpfs", "devtmpfs"]),
        (pq.join("root/run"), &["-t", "tmpfs", "tmpfs"]),
    ] {
        std::fs::create_dir_all(&target).ok();
        let mut a: Vec<&str> = vec!["mount"];
        a.extend_from_slice(args);
        a.push(target.to_str().unwrap_or(""));
        if run(&a).is_err() {
            return Err(OverlayError::Unsupported(
                format!("cannot mount pseudo-fs at {} in a user namespace (WSL2?)", target.display()),
            ));
        }
    }
    std::fs::create_dir_all(pq.join("root/dev/pts")).ok();
    std::fs::create_dir_all(pq.join("root/run")).ok();
    // Whitelist: bind the agent's real worktree into the overlay view so it is
    // the only real writable tree.
    let wrel = whitelist.strip_prefix(root).unwrap_or(whitelist);
    let wt = pq.join("root").join(wrel);
    std::fs::create_dir_all(&wt).ok();
    let _ = run(&["mount", "--bind", whitelist.to_str().unwrap_or("/"), wt.to_str().unwrap_or("/")]);
    // Promote the whole overlay view over /.
    run(&["mount", "--rbind", pq.join("root").to_str().unwrap_or("/"), "/"])?;
    // The agent is now ai-root inside its own overlay; tell the drop logic to
    // stop setuid-ing to ai_X (it IS root here) and that full-root is live.
    std::env::set_var("PIR_AGENT_NS_ROOT", "1");
    set_project_quarantine_engaged(true);
    set_system_quarantine_engaged(true);
    Ok(())
}

#[cfg(not(unix))]
pub fn try_full_root_quarantine(_r: &Path, _w: &Path, _s: &Path) -> Result<(), OverlayError> {
    Err(OverlayError::Unsupported("full-root quarantine requires unix".into()))
}

// ===========================================================================
// Capability detection (unix)
// ===========================================================================

/// True once this process has entered its own private mount namespace. Used to
/// make `enter_private_mount_ns` idempotent.
static NS_ENTERED: AtomicBool = AtomicBool::new(false);

/// Enter a private mount namespace so every overlay we mount below is visible
/// ONLY to this agent process (and the commands it spawns) — never to the rest
/// of the host. Without this, an overlay over `/var`, `/etc`, … would shadow
/// those trees for the whole system ("quarantine everybody's writes"). On the
/// private namespace the host keeps its real `/var`; only the agent sees the
/// staged view, and the namespace (and all its mounts) vanish when the agent
/// exits. **Quarantining requires this**: callers must refuse to mount if it
/// fails, otherwise they would quarantine the host's writes, not just the
/// agent's. Idempotent.
pub fn enter_private_mount_ns() -> std::io::Result<()> {
    if NS_ENTERED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    #[cfg(unix)]
    {
        if can_mount() {
            // Root path: a private mount namespace is enough; we hold
            // CAP_SYS_ADMIN so in-kernel overlay mounts work directly.
            let r = unsafe { libc::unshare(libc::CLONE_NEWNS) };
            if r != 0 {
                NS_ENTERED.store(false, Ordering::SeqCst);
                return Err(std::io::Error::last_os_error());
            }
        } else if fuse_overlayfs_available() {
            // Unprivileged path: a *user* namespace grants CAP_SYS_ADMIN *inside*
            // it (and scopes the mounts to us) so we can mount fuse-overlayfs
            // without host root. Map our own uid/gid to 0 inside the ns (no
            // setuid helper needed -- we own the ns we just created).
            let uid = unsafe { libc::getuid() };
            let gid = unsafe { libc::getgid() };
            let r = unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) };
            if r != 0 {
                NS_ENTERED.store(false, Ordering::SeqCst);
                return Err(std::io::Error::last_os_error());
            }
            let _ = std::fs::write("/proc/self/setgroups", b"deny\n");
            // Single-line root-map: ns-0 -> our own host uid/gid. (The kernel
            // only lets an unprivileged process write a ONE-line map of its own
            // uid; multi-line maps need setuid-root newuidmap helpers.) The agent
            // runs as virtual root here; `drop_to_agent_user` recognises that
            // virtual root already IS the agent and skips the setuid.
            let gmap = format!("0 {} 1\n", gid);
            let umap = format!("0 {} 1\n", uid);
            if std::fs::write("/proc/self/gid_map", gmap).is_err()
                || std::fs::write("/proc/self/uid_map", umap).is_err()
            {
                NS_ENTERED.store(false, Ordering::SeqCst);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "failed to write user-namespace id map",
                ));
            }
        } else {
            NS_ENTERED.store(false, Ordering::SeqCst);
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no overlay implementation available (need root or fuse-overlayfs)",
            ));
        }
        // Make our copy of the mount tree private so our mounts don't propagate
        // back to the parent namespace.
        let slash = b"/\0".as_ptr() as *const std::os::raw::c_char;
        let r = unsafe {
            libc::mount(
                slash,
                slash,
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            )
        };
        if r != 0 {
            NS_ENTERED.store(false, Ordering::SeqCst);
            return Err(std::io::Error::last_os_error());
        }
    }
    #[cfg(not(unix))]
    {
        NS_ENTERED.store(false, Ordering::SeqCst);
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "private mount namespace requires unix",
        ));
    }
    Ok(())
}

#[cfg(unix)]
pub fn can_mount() -> bool {
    if !overlay_supported() {
        return false;
    }
    // euid 0 is the common case here (the launcher runs as root). We shell
    // `id -u` to avoid a libc dep.
    match run(&["id", "-u"]) {
        (true, out) => out.trim() == "0",
        _ => false,
    }
}

/// Which overlay implementation backs the write-quarantine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    /// In-kernel `overlayfs` (`mount -t overlay`). Needs `CAP_SYS_ADMIN` in
    /// the mount namespace (root, or a user namespace where the kernel allows
    /// it). Fast and kernel-backed.
    Kernel,
    /// `fuse-overlayfs` (userspace). Mounts without `CAP_SYS_ADMIN`, so it is
    /// the unprivileged fallback when we are not root / the kernel refuses
    /// in-kernel overlay in a user namespace (e.g. WSL2). Slower (FUSE
    /// boundary) and needs `/dev/fuse` + the `fuse-overlayfs` binary.
    Fuse,
}

/// True if the `fuse-overlayfs` userspace binary exists and `/dev/fuse` is
/// present, so we can mount an overlay *without* `CAP_SYS_ADMIN`.
pub fn fuse_overlayfs_available() -> bool {
    Path::new("/dev/fuse").exists()
        && std::process::Command::new("fuse-overlayfs")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
}

/// True when this process is in the *init* user namespace (i.e. real root, not
/// virtual root inside a user namespace created by `enter_private_mount_ns`).
fn in_init_user_ns() -> bool {
    match (
        std::fs::read_link("/proc/self/ns/user").ok(),
        std::fs::read_link("/proc/1/ns/user").ok(),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// The overlay implementation to use, or `None` if neither in-kernel overlay
/// (root) nor `fuse-overlayfs` is available.
pub fn overlay_kind() -> Option<OverlayKind> {
    // In-kernel overlay only when we are root in the *init* user namespace.
    // After `enter_private_mount_ns` takes the unprivileged path we are virtual
    // root inside a user namespace (`id -u` == 0), where in-kernel overlay is
    // often refused (e.g. WSL2) — so we must fall back to fuse-overlayfs there.
    if in_init_user_ns() && can_mount() {
        Some(OverlayKind::Kernel)
    } else if fuse_overlayfs_available() {
        Some(OverlayKind::Fuse)
    } else {
        None
    }
}

/// Whether *any* overlay quarantine is possible (kernel or fuse).
pub fn overlay_available() -> bool {
    overlay_kind().is_some()
}

#[cfg(unix)]
fn overlay_supported() -> bool {
    let Ok(fs) = std::fs::read_to_string("/proc/filesystems") else {
        return false;
    };
    fs.lines().any(|l| l.trim_end().ends_with("overlay"))
}

#[cfg(not(unix))]
pub fn can_mount() -> bool {
    false
}
