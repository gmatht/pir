//! Recording which zellij session/pane a `pir` runs in, and rebuilding a
//! whole zellij session from those records.
//!
//! # Why this exists
//!
//! zellij already persists *layout*: a live session can be dumped with
//! `zellij action dump-layout`, and an exited one keeps its layout at
//! `~/.cache/zellij/contract_version_1/session_info/<name>/session-layout.kdl`
//! (that is what "attach to resurrect" reads). So the tabs, splits, sizes and
//! even the `pane command="pir" cwd="…" { args "-r" }` lines all survive a
//! restart on their own.
//!
//! What zellij cannot know is **which pir conversation belonged to which pane**:
//! a resurrected pane restarts as bare `pir` (the layout's `command="pir"`
//! carries no resume token), which would otherwise start a fresh session every
//! time. The gap this module fills is therefore the *pane → pir transcript*
//! mapping, not the layout: each pane claims its title as `pir:<stem>`
//! ([`claim_pane_title`]), zellij persists that title in the layout's `name=`,
//! and a bare `pir` with no resume token reads it back at startup
//! ([`resurrect_from_own_title`]) and resumes the transcript it names.
//!
//! # Recording
//!
//! On each prompt, a pir that is running inside zellij writes a small sidecar
//! next to its session log:
//!
//! ```json
//! { "session": "vitreous-hill", "pane_id": "3",
//!   "cwd": "/home/ai/src/pir", "captured": 1758888000 }
//! ```
//!
//! The capture is **env-only** — `ZELLIJ_SESSION_NAME` + `ZELLIJ_PANE_ID` — so
//! it never spawns a process on the prompt path. (zellij also sets
//! `ZELLIJ_TAB_NAME`/`ZELLIJ_TAB_ID`, but on zellij 0.45.1 they are empty, so
//! nothing is built on them.) A pir that is *not* running inside zellij writes
//! nothing at all.
//!
//! # Rebuilding
//!
//! `rebuild` takes a zellij session name, loads its layout (live dump, else the
//! persisted `session-layout.kdl`), and rewrites each `pane command="pir"` so
//! the pane comes back as the *specific* conversation it held:
//!
//! ```kdl
//! pane command="pir" cwd="/home/ai/src/pir" { args "-r" "pir-20260926-…-sh123" }
//! ```
//!
//! The rewrite is a targeted text transform over the dumped KDL rather than a
//! parse/round-trip: the dumped layout is hundreds of lines of `swap_tiled_layout`
//! boilerplate we have no business reformatting, and preserving it byte-for-byte
//! outside the panes we touch keeps the rebuild faithful by construction.
//!
//! The result is written to a temp `.kdl` and handed to
//! `zellij --new-session-with-layout <file> --session <name>` — verified to
//! actually create a session (note that `-s NAME` alone only *attaches*).
//!
//! # Why the create is detached (and why it must be)
//!
//! `--new-session-with-layout` *always* attaches: zellij 0.45.1 has no
//! `--detach`. So the child process is not "create and return" — it is "create
//! and then become a terminal client". Waiting for that child to exit
//! (`Command::output()`) waits for a human to detach, which never happens when
//! the operator is sitting at the keyboard, and `pir -r @` hangs forever. The
//! three facts below were each measured on a live zellij 0.45.1, not inferred:
//!
//!   * Inherited a controlling tty → the client attaches and blocks forever
//!     (the original bug; a `poll()` in `Command::output()` never returns).
//!   * `setsid()` + `/dev/null` stdio → no controlling terminal, so the client
//!     takes the "no tty" path, panics on `enable raw mode` **after** the server
//!     has created the session, and exits in ~0.2 s.
//!   * That panic is cosmetic: the session and its panes keep running, because
//!     they live in the zellij server, not in the client that created them.
//!
//! So the create runs detached and bounded, and success is then established by
//! *probing the session* rather than by the client's exit code — which is 101 on
//! the normal path. See [`create_session_detached`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// What we recorded about the zellij pane a pir was running in.
///
/// Deliberately small and env-derived: the recording happens on every prompt, so
/// it must not cost a subprocess. Anything derived from zellij itself (tab name,
/// geometry) is looked up at *rebuild* time instead, where a `zellij` call is
/// already being made.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRecord {
    /// `ZELLIJ_SESSION_NAME` — the session this pane belongs to.
    pub session: String,
    /// `ZELLIJ_PANE_ID` — the pane's id within that session.
    pub pane_id: String,
    /// The pane's working directory, so the rebuild can restore `cwd`.
    #[serde(default)]
    pub cwd: String,
    /// Unix epoch seconds of the last capture. Used to prefer the freshest
    /// record when a pane changed hands (e.g. the operator resumed a different
    /// conversation in place).
    #[serde(default)]
    pub captured: u64,
    /// The tab this pane was in, and its ordinal among that tab's terminal panes
    /// — the [`PaneSlot`] join key, captured while the session was alive.
    ///
    /// Why this is recorded rather than looked up later: a dumped layout carries
    /// no pane ids, so the only way to place a `pane_id` record back into a
    /// layout block is to ask zellij — and `zellij action list-panes` can only be
    /// asked about the session we are *currently inside*. A rebuild of a session
    /// that has since exited has no one to ask, so the geometry is snapshotted
    /// here instead, on the prompt path, where it costs nothing.
    ///
    /// Empty means "not captured" (a record written before this field existed, or
    /// a capture that could not resolve geometry); the rebuild then falls back to
    /// its conservative positional check.
    #[serde(default)]
    pub tab: String,
    #[serde(default)]
    pub index: u32,
}

impl PaneRecord {
    /// This record's [`PaneSlot`], or `None` when geometry was never captured.
    pub fn slot(&self) -> Option<PaneSlot> {
        if self.tab.is_empty() {
            return None;
        }
        Some(PaneSlot { tab: self.tab.clone(), index: self.index })
    }
}

/// The zellij session name from the environment, if this process is inside one.
pub fn session_name() -> Option<String> {
    non_empty(std::env::var("ZELLIJ_SESSION_NAME").ok())
}

/// The zellij pane id from the environment, if this process is inside one.
pub fn pane_id() -> Option<String> {
    non_empty(std::env::var("ZELLIJ_PANE_ID").ok())
}

/// True when the current process is running inside a zellij pane. Both variables
/// are required: zellij sets them together, and a stray one alone would produce
/// a record we could not act on.
pub fn in_zellij() -> bool {
    session_name().is_some() && pane_id().is_some()
}

fn non_empty(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Path of the zellij sidecar for a session log: `<log>.zellij.json`.
pub fn record_path(log: &Path) -> PathBuf {
    log.with_extension("zellij.json")
}

/// Build the [`PaneRecord`] for this process, or `None` when not inside zellij.
///
/// The pane's [`PaneSlot`] is resolved from a *cached* view of the session's pane
/// layout, never by spawning zellij here: this runs on the prompt path, and the
/// session's pane geometry only changes when the operator re-splits a tab. See
/// [`slot_cache`] for how the cache is kept fresh.
pub fn capture() -> Option<PaneRecord> {
    let session = session_name()?;
    let pane_id = pane_id()?;
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let slot = slot_cache(&session, &pane_id);
    Some(PaneRecord {
        session,
        pane_id,
        cwd,
        captured: crate::term::epoch(),
        tab: slot.as_ref().map(|s| s.tab.clone()).unwrap_or_default(),
        index: slot.map(|s| s.index).unwrap_or(0),
    })
}

/// The last pane-layout snapshot taken for `session`, refreshed at most once per
/// [`SLOT_CACHE_TTL`] seconds.
///
/// The cache exists so recording stays subprocess-free on the prompt path. The
/// window a stale cache can mis-slice is bounded by the TTL: if the operator
/// re-splits mid-TTL, a pane's ordinal is momentarily wrong — which the rebuild
/// treats as *unmatched* (that pane is left alone) rather than mis-assigned,
/// because a slot is only trusted when it is unambiguous.
static SLOT_CACHE: Mutex<Option<(String, u64, HashMap<String, PaneSlot>)>> = Mutex::new(None);

/// True while a background refresh of [`SLOT_CACHE`] is in flight, so the prompt
/// path never queues a second one (and never waits behind the first).
static SLOT_REFRESH_IN_FLIGHT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// How long a cached pane layout is trusted before re-querying zellij.
const SLOT_CACHE_TTL: u64 = 30;

/// The slot for `pane_id` in `session`, or `None` when it cannot be resolved.
///
/// **Never spawns.** This runs on the prompt path (via [`capture`]), and the
/// subprocess it used to run here — `zellij action list-panes` — can block
/// indefinitely when the session named by `ZELLIJ_SESSION_NAME` is gone. Holding
/// the cache lock across it meant one hung `zellij` stalled *every* prompt, and
/// because [`SLOT_CACHE_TTL`] is shorter than a typical prompt cadence the cache
/// was always stale, so it re-spawned (and re-hung) each time.
///
/// So the lookup is now purely a cached read: it returns the last-known-good slot
/// (or `None` on a cold cache) and, if the entry is stale, kicks a **detached
/// background thread** to refresh it. The prompt returns immediately either way;
/// the refreshed geometry is picked up by a later prompt. A stale slot only ever
/// costs a pane its geometry (the rebuild then treats it as unmatched and leaves
/// it alone), never a hung prompt.
fn slot_cache(session: &str, pane_id: &str) -> Option<PaneSlot> {
    let now = crate::term::epoch();
    let (fresh, cached) = {
        let guard = SLOT_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = guard
            .as_ref()
            .is_some_and(|(s, at, _)| s == session && now.saturating_sub(*at) < SLOT_CACHE_TTL);
        let cached = guard
            .as_ref()
            .filter(|(s, _, _)| s == session)
            .and_then(|(_, _, m)| m.get(pane_id).cloned());
        (fresh, cached)
    };
    if !fresh {
        refresh_slot_cache(session);
    }
    cached
}

/// Refresh [`SLOT_CACHE`] for `session` on a detached thread, at most once at a
/// time. Best-effort: a refresh that cannot run (no live session, a dead one, a
/// hung zellij) simply leaves the previous snapshot in place.
///
/// Detached and single-flight by design — the caller is the prompt path, so this
/// must not join, must not block on the lock, and must not accumulate one thread
/// per prompt if a refresh is slow.
fn refresh_slot_cache(session: &str) {
    use std::sync::atomic::Ordering;
    if SLOT_REFRESH_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return; // a refresh is already running
    }
    let session = session.to_string();
    std::thread::spawn(move || {
        // `live_panes` has its own hard deadline, so this thread terminates even
        // when zellij hangs; the in-flight flag is cleared even if it panics.
        let map = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| live_panes(&session)))
            .ok()
            .flatten();
        if let Some(map) = map {
            let now = crate::term::epoch();
            let mut guard = SLOT_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some((session, now, map));
        }
        SLOT_REFRESH_IN_FLIGHT.store(false, Ordering::SeqCst);
    });
}

/// Record this process's zellij pane for `log`. Best-effort and a no-op outside
/// zellij or when there is no session log (a one-shot run with no transcript).
///
/// Called once per prompt, so this is deliberately just an env read and a small
/// file write — no subprocess. The one exception is the *first* call, which also
/// claims the pane's zellij title; [`claim_pane_title`] latches itself so that
/// costs a single `zellij` spawn per pir process, not one per prompt.
pub fn record(log: &Path) {
    if log.as_os_str().is_empty() {
        return;
    }
    let Some(rec) = capture() else { return };
    let Ok(json) = serde_json::to_string_pretty(&rec) else { return };
    let _ = std::fs::write(record_path(log), json);
    // The title names the transcript's file stem — the same token `pir -r` takes.
    claim_pane_title(&log_stem(log));
}

/// The token `pir -r` resolves this log by: its file stem, or `""`.
fn log_stem(log: &Path) -> String {
    log.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}

/// Marker that identifies a pane title we own. The full title is
/// `pir:<session-stem>`, e.g. `pir:pir-20260926-085546-sh945107`.
///
/// Why a title and not zellij's `post_command_discovery_hook`: the hook runs
/// `sh -c` from the zellij *server* (`background_jobs.rs` on 0.45.1 drives it
/// once a second, per pane, on process change), so it costs a fork per active
/// pane per second and — decisively — it runs in the server's environment,
/// which carries `ZELLIJ_SESSION_NAME` but **no** `ZELLIJ_PANE_ID`, and whose
/// cwd is the server's, not the pane's. It therefore cannot key anything per
/// pane. A pane title *is* per-pane data that zellij itself serializes
/// (`name="…"` on the `pane` node) and re-applies on resurrect, so it carries
/// the identity with no polling at all.
///
/// The marker exists so a title the operator set by hand is never mistaken for
/// ours and rewritten: only a title that still starts with it is ours to update.
const TITLE_MARKER: &str = "pir:";

/// The pane title `pir` claims for `session_stem`: `pir:<session-stem>`.
fn pane_title(session_stem: &str) -> String {
    format!("{TITLE_MARKER}{session_stem}")
}

/// The session stem encoded in a `pir:`-marked pane title, if that is what it is.
fn title_session(title: &str) -> Option<&str> {
    title.strip_prefix(TITLE_MARKER).filter(|s| !s.is_empty())
}

/// True when `title` is zellij's own default for a pane rather than something
/// the operator typed: the bare command name (`"pir"`, and the `"pir · …"`
/// frame zellij renders around it — measured live as `pir · pir`, `pir ·
/// bash-o4`).
///
/// A resurrected pane comes back with exactly this, so the claim must treat it
/// as unset: otherwise every pane created by `zellij attach` keeps its default
/// title forever and no transcript is ever named.
fn is_default_pane_title(title: &str) -> bool {
    title == "pir" || title.strip_prefix("pir · ").is_some_and(|s| !s.is_empty())
}

/// True when `title` was emitted by the shell itself rather than typed by the
/// operator: an OSC title (`🐧<dir>`, from `PROMPT_COMMAND` — measured live as
/// `🐧pir`, `🐧bash-o4` on panes sitting at a shell prompt).
///
/// A pir the operator started from such a shell still carries the shell's
/// title when the first prompt claims it, so the claim must treat it as unset
/// too: otherwise no shell-started pir is ever claimed (hence no transcript
/// named) and every attach of those panes starts fresh. An explicit
/// `rename-pane` sticks over shell OSC output (probed live), so claiming here
/// cannot clobber a real operator rename — those never carry the marker.
fn is_shell_title(title: &str) -> bool {
    title.strip_prefix("🐧").is_some_and(|s| !s.is_empty())
}

/// Whether [`claim_pane_title`] may rename a pane currently titled `current`:
/// ours (re-claiming is a no-op rename), unset, zellij's own default, or the
/// shell's OSC title.
///
/// Split out so the regression test asserts on exactly what the claim
/// consults, not a recomputation of it.
fn title_is_claimable(current: &str) -> bool {
    title_session(current).is_some()
        || current.is_empty()
        || is_default_pane_title(current)
        || is_shell_title(current)
}

/// Claim this pane's zellij title, once per session, so plain
/// `zellij attach` resurrects the *right* conversation without any help from
/// `pir -r @<session>`.
///
/// Best-effort in the strong sense: every failure is ignored, because a pane
/// title is a convenience and a missing one costs nothing but a resumed pane
/// that starts fresh — which is exactly what happens today. Called from the
/// prompt path, so it must stay cheap.
///
/// Two things make this safe to call there:
///
///   * **It is called at most once per process**, via [`TITLE_CLAIMED`]. This is
///     what keeps the prompt path cheap: one `zellij` spawn for the whole
///     session, not one per prompt.
///   * **It only claims a title that is ours, unset, zellij's default, or the
///     shell's OSC title.** A title the operator chose by hand is left alone,
///     so this can never rename a pane behind their back. (A `/rewind` to a
///     different transcript does *not* re-trigger it — the latch holds for the
///     process's lifetime. The sidecar path still tracks the new transcript, so
///     `pir -r @<session>` remains correct; only the title, and therefore plain
///     `zellij attach`, goes stale until the next `pir`.)
///
/// A pane zellij just resurrected reports its title as the bare command name
/// (`"pir"`) — zellij fills that in itself, the operator never typed it — so
/// that counts as claimable exactly like an empty title. Without this every
/// resurrected pane kept its default title forever (measured on live zellij
/// 0.44.1: panes 10/11/12 all titled `"pir"`, none ever claimed), and plain
/// `zellij attach` kept starting fresh sessions.
///
/// The spawn is `zellij action rename-pane --pane-id <id> <title>`; 0.45.1
/// accepts an explicit `--pane-id`, so this does not depend on which pane
/// happens to be focused.
pub fn claim_pane_title(session_stem: &str) {
    let (Some(_session), Some(pane)) = (session_name(), pane_id()) else { return };
    if session_stem.is_empty() {
        return;
    }
    let title = pane_title(session_stem);

    // Only the first turn pays for a subprocess.
    {
        let mut claimed = TITLE_CLAIMED.lock().unwrap_or_else(|e| e.into_inner());
        if *claimed {
            return;
        }
        *claimed = true;
    }

    // Never stomp a title the operator chose. `name=` is absent unless something
    // already named this pane, so "not ours and not empty" means hands off —
    // with two exceptions, both things the operator never typed: zellij itself
    // reports a never-renamed pane's title as its bare command name, and the
    // shell reports its own OSC title (`🐧<dir>` from `PROMPT_COMMAND`).
    if let Some(current) = current_pane_title(&pane) {
        if !title_is_claimable(&current) {
            return;
        }
    }

    // Bounded: this runs on the prompt path, and `zellij action` against a
    // session that has gone away can block instead of failing. A title is a
    // convenience, so a slow zellij must never stall the turn that is starting.
    let _ = run_zellij_bounded(
        &["action", "rename-pane", "--pane-id", &pane, &title],
        TITLE_CALL_TIMEOUT,
        false,
    );
}

/// Run a `zellij <args…>` command with a hard deadline, returning its stdout on
/// success. `None` on spawn failure, non-zero exit, or timeout (the child is
/// killed in that case, and its reader joined).
///
/// `capture_stdout` selects a piped read; when false the output is discarded (a
/// fire-and-forget action like `rename-pane`). This is the one place a bounded
/// `zellij` call is made from the prompt path — everything here used to be a bare
/// blocking `Command::output()`, which is exactly how a stale `ZELLIJ_SESSION_NAME`
/// could freeze every prompt.
fn run_zellij_bounded(args: &[&str], timeout: std::time::Duration, capture_stdout: bool) -> Option<Vec<u8>> {
    let mut child = Command::new("zellij")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(if capture_stdout {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let reader = child.stdout.take().map(|mut stdout| {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            buf
        })
    });
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let buf = reader.and_then(|r| r.join().ok()).unwrap_or_default();
                return if status.success() { Some(buf) } else { None };
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    if let Some(r) = reader {
                        let _ = r.join();
                    }
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
}

/// Bound on the `list-panes` / `rename-pane` calls the title claim makes, which
/// run on the first turn's prompt path.
const TITLE_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Set once the title claim has been attempted (successfully or not), so the
/// prompt path pays for at most one `zellij` spawn per pir process.
static TITLE_CLAIMED: Mutex<bool> = Mutex::new(false);

/// The title currently set on `pane`, or `None` when it could not be read.
///
/// `list-panes` has no `--pane-id` filter (checked on 0.45.1 — it rejects the
/// flag), so the whole session is listed and the one pane is picked out of the
/// JSON. A subprocess like the ones this module already runs at rebuild time,
/// and only ever called from [`claim_pane_title`] — i.e. once per process.
fn current_pane_title(pane: &str) -> Option<String> {
    // Bounded: see `run_zellij_bounded`. The title read sits on the first turn's
    // prompt path, so it gets the same deadline as the rename that follows it.
    let out = run_zellij_bounded(
        &["action", "list-panes", "-a", "-j"],
        TITLE_CALL_TIMEOUT,
        true,
    )?;
    let v: serde_json::Value = serde_json::from_slice(&out).ok()?;
    title_of_pane(&v, pane)
}

/// The title of terminal pane `pane` in a parsed `list-panes -a -j` array.
///
/// The pure half of [`current_pane_title`]: no subprocess, just the pick.
/// Split out so the regression test asserts on exactly what the claim reads,
/// without needing a live zellij.
fn title_of_pane(v: &serde_json::Value, pane: &str) -> Option<String> {
    let want: i64 = pane.parse().ok()?;
    v.as_array()?.iter().find_map(|p| {
        // zellij reuses small ids across plugins and terminals (measured live:
        // id 0 is both the `zellij:link` plugin and a terminal), and array
        // order puts plugins first — so an unfiltered id match reads the
        // *plugin's* title, never the pane's. Terminals only.
        let is_terminal = !p.get("is_plugin").and_then(serde_json::Value::as_bool).unwrap_or(true);
        (is_terminal && p.get("id").and_then(serde_json::Value::as_i64) == Some(want))
            .then(|| p.get("title").and_then(serde_json::Value::as_str).unwrap_or_default().to_string())
    })
}

/// Read the pane record for a session log, if one was written.
pub fn read_record(log: &Path) -> Option<PaneRecord> {
    let raw = std::fs::read_to_string(record_path(log)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// A `(session, pane_id)` → session-log mapping, built from every `.zellij.json`
/// sidecar in the given session directories.
///
/// When two records claim the same pane (the operator resumed another
/// conversation in place) the one with the newest `captured` wins, which is the
/// conversation that was actually running there last.
pub fn pane_map(dirs: &[PathBuf]) -> HashMap<(String, String), PathBuf> {
    use std::fs;
    let mut best: HashMap<(String, String), (u64, PathBuf)> = HashMap::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(rec) = read_record(&path) else { continue };
            if rec.session.is_empty() || rec.pane_id.is_empty() {
                continue;
            }
            let key = (rec.session.clone(), rec.pane_id.clone());
            let entry = (rec.captured, path.clone());
            match best.get(&key) {
                Some((seen, _)) if *seen >= rec.captured => {}
                _ => {
                    best.insert(key, entry);
                }
            }
        }
    }
    best.into_iter().map(|(k, (_, v))| (k, v)).collect()
}

/// Where `pir` keeps its session logs: the global dir plus the project-local one
/// when it exists (mirrors `main::scan_sessions`).
pub fn session_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![crate::config::pi_dir().join("agent").join("sessions")];
    if let Ok(cwd) = std::env::current_dir() {
        let local = cwd.join(".pir").join("sessions");
        if local != dirs[0] {
            dirs.push(local);
        }
    }
    dirs
}

/// The KDL that zellij persists for an exited session, if it is on disk.
///
/// This is what "attach to resurrect" uses, so it is the authoritative layout
/// for a session that is no longer running.
pub fn exited_layout_path(session: &str) -> PathBuf {
    let base = std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"));
    base.join(".cache")
        .join("zellij")
        .join("contract_version_1")
        .join("session_info")
        .join(session)
        .join("session-layout.kdl")
}

/// Load the layout KDL for `session`.
///
/// Prefers the live layout (`zellij action dump-layout`, which reflects any
/// splits the operator made since the session was created) and falls back to the
/// persisted `session-layout.kdl` for an exited session.
pub fn load_layout(session: &str) -> Result<String, String> {
    // Live first: only meaningful when we are *inside* this session, which is
    // exactly the case `dump-layout` acts on.
    if session_name().as_deref() == Some(session)
        && let Ok(out) = Command::new("zellij").args(["action", "dump-layout"]).output()
        && out.status.success()
    {
        let kdl = String::from_utf8_lossy(&out.stdout).to_string();
        if !kdl.trim().is_empty() {
            return Ok(kdl);
        }
    }
    let path = exited_layout_path(session);
    match std::fs::read_to_string(&path) {
        Ok(kdl) if !kdl.trim().is_empty() => Ok(kdl),
        Ok(_) => Err(format!("zellij layout for '{session}' is empty ({})", path.display())),
        Err(e) => Err(format!(
            "no layout for zellij session '{session}' — {} ({e}). \
             It may never have been saved; `zellij list-sessions` shows what exists.",
            path.display()
        )),
    }
}

/// One `pane` block in a dumped layout, located in the source text.
struct PaneBlock {
    /// Byte offset of the `pane` keyword.
    start: usize,
    /// Byte offset just past the block's closing `}` (or the end of a
    /// brace-less one-liner).
    end: usize,
    /// The block's source text.
    text: String,
    /// Where this pane sits: which tab, and its row/column in that tab's tiling
    /// tree. Used to match a recorded pane id to this block without relying on
    /// document order.
    slot: PaneSlot,
}

/// A pane's addressable position within a session: its tab, and its ordinal
/// among that tab's terminal panes.
///
/// The dumped layout carries **no pane ids**, so a record (`pane_id = "8"`) can
/// only be matched to a layout block through zellij. The join key is the tab
/// *name* (stable under tab reordering/insertion, unlike its index) plus the
/// pane's ordinal within that tab. Both sides can compute that ordinal:
///
///   * layout — the n-th leaf `command=` pane in the tab's nesting, which is
///     zellij's own dump order;
///   * live   — the n-th non-plugin pane of that tab from
///     `zellij action list-panes -a -j`, sorted by `(pane_y, pane_x)`, which is
///     the same top-to-bottom then left-to-right order the nesting encodes.
///
/// Plugins (tab-bar/status-bar) are excluded from both counts, so the two
/// ordinals line up.
///
/// Public because [`PaneRecord::slot`] returns it on a `pub` type: the
/// sidecar's geometry is part of pir's on-disk contract, not a private detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSlot {
    /// The tab's `name` attribute (e.g. `Tab #3`).
    pub tab: String,
    /// 0-based ordinal among the tab's terminal panes.
    pub index: u32,
}

/// The logs `match_panes` was given but did **not** place, i.e. the records for
/// `session` whose log does not appear in the resulting `by_block`.
///
/// Derived from `match_panes`' own output rather than re-deriving the join, so
/// the shortfall cannot drift from what was actually rewritten: a log is placed
/// exactly when some block names it.
fn unplaced_logs(
    session: &str,
    map: &HashMap<(String, String), PathBuf>,
    by_block: &[Option<PathBuf>],
) -> Vec<PathBuf> {
    let placed: Vec<&PathBuf> = by_block.iter().flatten().collect();
    let mut out: Vec<PathBuf> = map
        .iter()
        .filter(|(k, _)| k.0 == session)
        .map(|(_, v)| v.clone())
        .filter(|log| !placed.iter().any(|p| *p == log))
        .collect();
    out.sort();
    out
}

/// The subset of `unplaced_logs` that is *stale*: records naming a tab that is
/// not a tab in this layout (e.g. `"Tab#4"` when zellij calls it `"Tab #4"`).
/// These are the actionable ones — the record itself is wrong, rather than the
/// join simply being too cautious to place it.
fn unknown_tabs_of(logs: &[PathBuf], layout_tabs: &[String]) -> Vec<String> {
    let mut out: Vec<String> = logs
        .iter()
        .filter_map(|p| read_record(p))
        .map(|r| r.tab)
        .filter(|tab| !tab.is_empty() && !layout_tabs.iter().any(|t| t == tab))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Find `pane … { … }` blocks in `kdl`, descending into *container* panes.
///
/// zellij nests a split's children inside the parent block:
///
/// ```kdl
/// pane split_direction="vertical" {
///     pane command="pir" { args "-r" }   // ← must be rewritten too
///     pane command="htop" { }
/// }
/// ```
///
/// so returning only the outermost blocks would miss exactly the panes we care
/// about in any tab with a split. A container is a `pane` with no `command`; its
/// children are collected instead of the container itself. A `pane` that *does*
/// carry a command is a leaf and is returned as-is.
///
/// This is a small hand-rolled scanner rather than a KDL parser: the dumped
/// layout is a few hundred lines of boilerplate we want to preserve
/// byte-for-byte, and the only thing we need to locate is `pane` blocks — the
/// language's braces are balanced and its strings are quoted, which is all this
/// has to respect.
fn find_pane_blocks(kdl: &str) -> Vec<PaneBlock> {
    let mut out = Vec::new();
    let mut cur = ScanPos::default();
    scan_panes(kdl, 0, kdl.len(), &mut cur, &mut out);
    out
}

/// The pane address the scanner is currently filling in: its tab, and how many
/// terminal panes that tab has yielded so far.
#[derive(Default)]
struct ScanPos {
    tab: String,
    index: u32,
}


/// Walk `kdl[start..end]`, collecting leaf `pane` blocks into `out`.
///
/// `cur` carries the pane's position within its tab across the recursion into
/// nested splits, so a pane two levels deep still gets the ordinal it would have
/// in zellij's own flat pane numbering. A block holding only plugins (the
/// tab-bar / status-bar `pane size=1` entries) is not a terminal pane and must
/// not advance the counter.
fn scan_panes(kdl: &str, start: usize, end: usize, cur: &mut ScanPos, out: &mut Vec<PaneBlock>) {
    let bytes = kdl.as_bytes();
    let mut i = start;
    while i < end {
        // Skip comments (`// …`) and quoted strings so a `pane` inside either
        // can't be mistaken for a block.
        if bytes[i] == b'/' && i + 1 < end && bytes[i + 1] == b'/' {
            while i < end && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'"' {
            i = skip_string(bytes, i);
            continue;
        }
        // A `tab` node starts a new tiling tree: it resets the ordinal, and its
        // `name` attribute is the join key against the live pane listing.
        if kdl[i..].starts_with("tab ") && is_word_boundary(bytes, i, 3) {
            if let Some((bs, be, node_end)) = brace_body_and_end(kdl, i, end) {
                cur.tab = attribute_value(&kdl[i..bs], "name").unwrap_or_default();
                cur.index = 0;
                scan_panes(kdl, bs, be, cur, out);
                i = node_end;
                continue;
            }
        }
        if kdl[i..].starts_with("pane") && is_word_boundary(bytes, i, 4) {
            let block_start = i;
            // Walk forward to the first `{` or the end of the statement.
            let mut j = i + 4;
            let mut block_end = None;
            let mut body: Option<(usize, usize)> = None;
            while j < end {
                match bytes[j] {
                    b'"' => j = skip_string(bytes, j),
                    b'{' => {
                        let k = matching_brace_end(kdl, j, end);
                        body = Some((j + 1, k.saturating_sub(1)));
                        block_end = Some(k);
                        break;
                    }
                    // A `pane` with no body ends at the newline.
                    b'\n' => {
                        block_end = Some(j);
                        break;
                    }
                    _ => j += 1,
                }
            }
            let Some(b_end) = block_end else { break };
            let head = &kdl[block_start..body.map(|(s, _)| s).unwrap_or(b_end)];
            if head.contains("command=") {
                // A leaf pane running something: this is what we rewrite. It
                // occupies the next ordinal in this tab.
                let slot = PaneSlot { tab: cur.tab.clone(), index: cur.index };
                cur.index += 1;
                out.push(PaneBlock {
                    start: block_start,
                    end: b_end,
                    text: kdl[block_start..b_end].to_string(),
                    slot,
                });
            } else if let Some((bs, be)) = body {
                if kdl[bs..be].contains("plugin location=") {
                    // The tab-bar / status-bar entries are real panes in the
                    // layout but not *terminal* panes, and never `pir`. Skip
                    // them without advancing the ordinal, which is what keeps
                    // our numbering equal to zellij's non-plugin pane count.
                } else {
                    // A split/holder: descend, keeping the same tab and running
                    // ordinal so its children number as they do in zellij.
                    scan_panes(kdl, bs, be, cur, out);
                }
            }
            i = b_end;
            continue;
        }
        i += 1;
    }
}

/// Index just past the `}` matching the `{` at `open`, or `end` if unbalanced.
fn matching_brace_end(kdl: &str, open: usize, end: usize) -> usize {
    let bytes = kdl.as_bytes();
    let mut depth = 0usize;
    let mut k = open;
    while k < end {
        match bytes[k] {
            b'"' => k = skip_string(bytes, k),
            b'{' => {
                depth += 1;
                k += 1;
            }
            b'}' => {
                depth -= 1;
                k += 1;
                if depth == 0 {
                    return k;
                }
            }
            _ => k += 1,
        }
    }
    end
}

/// The `{ … }` body span of the block starting at `start`, plus the offset just
/// past the block itself. `None` when the block has no body (a brace-less
/// one-liner), in which case there is nothing to descend into.
fn brace_body_and_end(kdl: &str, start: usize, end: usize) -> Option<(usize, usize, usize)> {
    let bytes = kdl.as_bytes();
    let mut j = start;
    while j < end {
        match bytes[j] {
            b'"' => j = skip_string(bytes, j),
            b'{' => {
                let k = matching_brace_end(kdl, j, end);
                return Some((j + 1, k.saturating_sub(1), k));
            }
            b'\n' => return None,
            _ => j += 1,
        }
    }
    None
}

/// Read `key="value"` out of a block's header (the text before its `{`).
fn attribute_value(header: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let pos = header.find(&needle)?;
    let rest = &header[pos + needle.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Skip a quoted string starting at `i` (a `"` byte), returning the index just
/// past the closing quote. Handles backslash escapes.
fn skip_string(bytes: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2,
            b'"' => return j + 1,
            _ => j += 1,
        }
    }
    bytes.len()
}

/// Is the keyword at `[i, i+len)` a standalone word (not part of a longer
/// identifier such as `panes`)?
fn is_word_boundary(bytes: &[u8], i: usize, len: usize) -> bool {
    let before_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
    let after = i + len;
    let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
    before_ok && after_ok
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Rewrite one `pane` block so it runs `pir -r <session>`.
///
/// Three shapes are handled:
///   * `pane command="pir" { args "-r" }`  → replace/extend the `args`
///   * `pane command="pir" cwd="…" { … }`  → also force the recorded `cwd`
///   * `pane` (no command)                 → add `command="pir" args "-r" …`
///
/// Only `command`/`cwd`/`args` are touched; sizes, focus, borders and any plugin
/// children are preserved exactly.
fn inject_pir_args(block: &str, session_log: &Path, record: &PaneRecord) -> String {
    let target = session_arg(session_log);
    let mut out = block.to_string();

    // 1. `args "-r"` → `args "-r" "<session>"`. A bare `-r` in a recorded pane
    //    means "resume whatever this pane was", which is precisely the record we
    //    hold. When there is no `args`, one is inserted before the body.
    if out.contains("args \"-r\"") {
        out = out.replace("args \"-r\"", &format!("args \"-r\" \"{target}\""));
    } else if !out.contains("args ") {
        out = insert_before_body(&out, &format!("args \"-r\" \"{target}\""));
    } else {
        // An `args` list that isn't the resume form: append our target to it.
        out = append_arg(&out, &target);
    }

    // 2. Force the recorded cwd so the pane starts where the work actually lived
    //    (the dumped cwd is already usually right, but a session recorded from a
    //    different directory would otherwise come back in the wrong place).
    if !record.cwd.is_empty() {
        out = set_attribute(&out, "cwd", &record.cwd);
    }
    out
}

/// The session argument to hand `pir -r`: the log's file stem, which is the
/// token `pir -r` resolves by name.
fn session_arg(log: &Path) -> String {
    log.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Insert `text` on its own line just inside the block's body.
///
/// The insertion point must be *after* the opening `{`, not before it: splicing
/// before the brace produces `pane command="pir"      args "-r" "x"\n{` — the
/// attributes and the brace end up on separate lines, which is not valid KDL
/// (and would have made every rebuild with a bare `pir` pane fail to load).
///
/// A block with no body gets the text appended after its last line instead.
fn insert_before_body(block: &str, text: &str) -> String {
    let Some(brace) = block.find('{') else {
        let mut s = block.trim_end().to_string();
        s.push('\n');
        s.push_str(text.trim());
        s.push('\n');
        return s;
    };
    let after_brace = brace + 1;
    // Match the indentation the block already uses for its body, so the inserted
    // line looks like the lines around it.
    let body = block[after_brace..].strip_prefix('\n').unwrap_or(&block[after_brace..]);
    let indent = body
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .unwrap_or(0);

    let mut out = String::with_capacity(block.len() + text.len() + 2);
    out.push_str(&block[..after_brace]);
    out.push('\n');
    out.push_str(&" ".repeat(indent));
    out.push_str(text.trim());
    out.push('\n');
    out.push_str(body);
    out
}

/// Set (or add) `key="value"` on the block's opening line without disturbing the
/// rest of the block.
fn set_attribute(block: &str, key: &str, value: &str) -> String {
    let header_end = block.find('{').unwrap_or(block.len());
    let (head, tail) = block.split_at(header_end);
    let needle = format!("{key}=\"");
    if let Some(pos) = head.find(&needle) {
        let val_start = pos + needle.len();
        if let Some(rel_end) = head[val_start..].find('"') {
            let val_end = val_start + rel_end;
            let mut out = String::with_capacity(block.len() + value.len());
            out.push_str(&head[..val_start]);
            out.push_str(value);
            out.push_str(&head[val_end..]);
            out.push_str(tail);
            return out;
        }
    }
    // Not present: add it right after the `pane` keyword.
    if let Some(rest) = head.strip_prefix("pane") {
        return format!("pane {key}=\"{value}\"{rest}{tail}");
    }
    block.to_string()
}

/// Append `target` to an existing `args` list that isn't the `-r` form.
fn append_arg(block: &str, target: &str) -> String {
    let Some(pos) = block.find("args \"-r\"") else {
        return block.to_string();
    };
    let mut out = String::with_capacity(block.len() + target.len() + 3);
    out.push_str(&block[..pos]);
    out.push_str(&format!("args \"-r\" \"{target}\""));
    out.push_str(&block[pos + "args \"-r\"".len()..]);
    out
}

/// Enumerate the panes of a **running** zellij session, keyed by pane id.
///
/// This is the authoritative pane↔id mapping. The layout KDL has no ids, so
/// without this a record (`pane_id = "8"`) can only be paired with a layout
/// block by assuming both enumerate panes in the same order — which breaks as
/// soon as the operator re-splits a tab, or the layout on disk is stale.
///
/// Within a tab, panes are ordered by `(pane_y, pane_x)`: top-to-bottom then
/// left-to-right, the same order the dumped KDL nesting encodes. Plugins are
/// excluded so the ordinals match [`find_pane_blocks`]. `tab_name` is the join
/// key, not `tab_id` — ids shift when tabs are inserted, names do not.
///
/// Returns `None` when the session is not the one we are inside (the action
/// only targets the current session) or the call fails; the caller then falls
/// back to the record's own snapshotted slot, and ultimately to the positional
/// heuristic, rather than refusing to rebuild.
fn live_panes(session: &str) -> Option<HashMap<String, PaneSlot>> {
    if session_name().as_deref() != Some(session) {
        return None;
    }
    let mut child = Command::new("zellij")
        .args(["action", "list-panes", "-a", "-j"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    // Hard timeout: this runs on the prompt path (via `capture`). If the
    // session has gone away — or the env names a session that no longer exists
    // — `zellij action` can block indefinitely instead of exiting, and a hung
    // prompt is far worse than a missing geometry record. So read the pipe on a
    // helper thread and give up on the deadline, killing the child.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let buf = reader.join().ok()?;
                if !status.success() {
                    return None;
                }
                return parse_live_panes(&buf);
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    // Kill the child, then *join the reader*: once the process is
                    // gone its stdout pipe reaches EOF and the reader returns
                    // promptly. Letting `reader` detach here leaked one thread per
                    // timed-out call — and on the "stale session name" path that
                    // was one leaked thread per prompt.
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
}

/// Parse `zellij action list-panes -a -j` output into pane id → [`PaneSlot`].
fn parse_live_panes(bytes: &[u8]) -> Option<HashMap<String, PaneSlot>> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    // Terminal panes only: the tab-bar/status-bar plugins appear in the JSON but
    // are never `pane command=` blocks in the layout.
    let mut by_tab: HashMap<String, Vec<(u32, u32, String)>> = HashMap::new();
    for p in v.as_array()? {
        if p.get("is_plugin").and_then(serde_json::Value::as_bool).unwrap_or(true) {
            continue;
        }
        if p.get("exited").and_then(serde_json::Value::as_bool).unwrap_or(false) {
            continue;
        }
        let id = p.get("id").and_then(serde_json::Value::as_i64)?.to_string();
        let tab = p
            .get("tab_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let y = p.get("pane_y").and_then(serde_json::Value::as_i64).unwrap_or(0) as u32;
        let x = p.get("pane_x").and_then(serde_json::Value::as_i64).unwrap_or(0) as u32;
        by_tab.entry(tab).or_default().push((y, x, id));
    }

    let mut out_map = HashMap::new();
    for (tab, mut panes) in by_tab {
        panes.sort_by_key(|(y, x, _)| (*y, *x));
        for (index, (_, _, id)) in panes.into_iter().enumerate() {
            out_map.insert(id, PaneSlot { tab: tab.clone(), index: index as u32 });
        }
    }
    Some(out_map)
}

///
/// `map` is the `(session, pane_id)` → session-log mapping from [`pane_map`].
/// Panes we cannot match are left exactly as they were.
///
/// Returns the rewritten KDL plus the number of panes that received a resume
/// session, so callers can report what actually happened.
///
/// # How a record is matched to a layout block
///
/// The layout has no pane ids, so the two sides are joined on
/// [`PaneSlot`] (tab name + ordinal within the tab):
///
///   * **authoritative** — when the session is the one we are running inside,
///     `zellij action list-panes` supplies the real id→slot map. This is the
///     only correct mapping, and it is what we use whenever it is available.
///   * **fallback** — for an *exited* session there is nothing to interrogate,
///     so we fall back to the old assumption: the n-th recorded pane id (in
///     numeric order) pairs with the n-th `pir` block in the dump.
///
/// The fallback is a guess, and it is wrong exactly when the recorded pane set
/// and the saved layout disagree (a re-split after the last record, a pane whose
/// pir exited). Restoring the *wrong conversation into a pane* is worse than
/// leaving it alone, so the fallback only fires when every recorded pane is
/// being consumed in order — if the counts do not line up, nothing is claimed
/// and the caller reports a layout-only rebuild.
/// What `inject_resume_args` produced: the rewritten layout, how many `pir`
/// panes actually received a conversation, and how many recorded panes could
/// not be placed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Injection {
    /// The layout with resume args injected.
    pub kdl: String,
    /// `pir` blocks that were rewritten to resume a specific conversation.
    pub restored: usize,
    /// Recorded panes for this session that matched no `pir` block. Non-zero
    /// means the rebuild is *partial*, which the caller must say out loud
    /// rather than let pass as a clean run.
    pub unmatched: usize,
    /// Of the unmatched records, the tab names that are not tabs in this layout.
    /// Sorted and deduplicated. These are the stale ones (see
    /// [`RebuildReport::unknown_tabs`]).
    pub unknown_tabs: Vec<String>,
}

impl Injection {
    /// The rewritten layout alone, for callers that do not report counts.
    pub fn kdl(&self) -> &str {
        &self.kdl
    }
}

pub fn inject_resume_args(kdl: &str, session: &str, map: &HashMap<(String, String), PathBuf>) -> Injection {
    let blocks = find_pane_blocks(kdl);
    let pir_blocks: Vec<&PaneBlock> =
        blocks.iter().filter(|b| b.text.contains("command=\"pir\"")).collect();
    let live = live_panes(session);
    let by_block = match_panes(session, map, &pir_blocks, live.as_ref());
    let (kdl, restored) = rewrite_with(kdl, &pir_blocks, &by_block);

    // Which records exist for this session but were placed nowhere? Those are
    // the silent-partial case. `unplaced_logs` derives them from `by_block`
    // itself, so the shortfall cannot drift from what was actually rewritten,
    // and `unknown_tabs_of` picks out the stale ones (a record naming a tab this
    // layout does not have) — which is the diagnosable half.
    let layout_tabs: Vec<String> = blocks.iter().map(|b| b.slot.tab.clone()).collect();
    let unplaced = unplaced_logs(session, map, &by_block);
    let unknown_tabs = unknown_tabs_of(&unplaced, &layout_tabs);
    let unmatched = unplaced.len();
    Injection { kdl, restored, unmatched, unknown_tabs }
}

/// The `pir:` session stem encoded in a pane block's `name=` attribute, if any.
///
/// This is the *authoritative* pane→transcript key, better than [`PaneSlot`]:
/// it was written by the pane itself (`claim_pane_title`) and travels inside
/// zellij's own layout, so it survives a session that has exited and needs no
/// inference. A block with no title, or a title the operator set by hand, yields
/// `None` and is matched the old way.
///
/// Read off the block's *header* only (before `{`), so a `name=` inside a child
/// pane is never mistaken for this pane's.
fn block_title_session(block: &str) -> Option<&str> {
    let header = match block.find('{') {
        Some(i) => &block[..i],
        // A brace-less one-liner (`pane name="pir:x" cwd="/y"`) has no header
        // boundary; the whole line is its header.
        None => block.lines().next().unwrap_or(block),
    };
    let needle = "name=\"";
    let mut from = 0usize;
    while let Some(rel) = header[from..].find(needle) {
        let val_start = from + rel + needle.len();
        let Some(end) = header[val_start..].find('"') else { break };
        let value = &header[val_start..val_start + end];
        if let Some(stem) = title_session(value) {
            return Some(stem);
        }
        from = val_start + end + 1;
    }
    None
}

/// Apply `by_block` (one optional log per pir block) to `kdl`, returning the
/// rewritten layout and how many blocks actually received a conversation.
fn rewrite_with(kdl: &str, pir_blocks: &[&PaneBlock], by_block: &[Option<PathBuf>]) -> (String, usize) {
    let mut out = String::with_capacity(kdl.len() + 256);
    let mut cursor = 0usize;
    let mut used = 0usize;
    for (i, b) in pir_blocks.iter().enumerate() {
        let Some(log) = by_block.get(i).cloned().flatten() else { continue };
        let Some(record) = read_record(&log) else { continue };
        out.push_str(&kdl[cursor..b.start]);
        out.push_str(&inject_pir_args(&b.text, &log, &record));
        cursor = b.end;
        used += 1;
    }
    out.push_str(&kdl[cursor..]);
    (out, used)
}

/// Locate the session log whose file stem is `stem`.
///
/// The title names a stem, not a path, so this resolves it against the same
/// directories [`pane_map`] reads. Returns `None` when the stem is ambiguous
/// (two logs share it) or when no such log exists — in which case the caller
/// leaves the pane alone rather than guessing.
fn log_for_stem(stem: &str) -> Option<PathBuf> {
    let mut found: Option<PathBuf> = None;
    for dir in session_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            if path.file_stem().and_then(|s| s.to_str()) != Some(stem) {
                continue;
            }
            // Ambiguous: the same stem in two directories. Refuse rather than
            // pick — a wrong transcript in a pane is the failure mode we care
            // most about avoiding.
            if found.is_some() {
                return None;
            }
            found = Some(path);
        }
    }
    found
}

/// The session log this pane held before zellij detached, or `None` when there
/// is nothing to resurrect.
///
/// This is the read-back half of [`claim_pane_title`]: a pane resurrected by a
/// plain `zellij attach` restarts as **bare** `pir` (the layout's
/// `command="pir"` carries no resume token — zellij only persists the layout,
/// and only `pir -r @` rewrites it with per-pane targets). The pane's *title*,
/// however, is persisted by zellij in the layout's `name=` attribute, so the
/// `pir:<stem>` claim survives the restart. When this process starts inside
/// zellij with no explicit resume token, its own title names the transcript it
/// was running — resolving it here is what makes plain `attach` come back to
/// the right conversation instead of a fresh session.
///
/// Returns `None` (start fresh) in every ambiguous case: not inside zellij,
/// the title could not be read (a slow/dead `zellij` must never stall startup),
/// the title is not one of ours (the operator named the pane by hand), no such
/// transcript exists on disk (it was deleted or moved), the stem names two logs
/// (a wrong transcript in a pane is the failure mode to avoid), or the one log
/// it names is explicitly finished (`/finished` means "do not come back").
///
/// Bounded by [`TITLE_CALL_TIMEOUT`] twice (one `list-panes` read): startup
/// must never hang behind a zellij that has gone away.
pub fn resurrect_from_own_title() -> Option<PathBuf> {
    let pane = pane_id()?;
    let current = current_pane_title(&pane)?;
    resurrect_log_for_title(&current)
}

/// The transcript named by a `pir:<stem>` pane title, or `None`.
///
/// The pure half of [`resurrect_from_own_title`]: no subprocess, just the
/// stem → log resolution plus the finished gate. Split out so the regression
/// test asserts on exactly what startup uses, without needing a live zellij.
fn resurrect_log_for_title(title: &str) -> Option<PathBuf> {
    let stem = title_session(title)?.to_string();
    let log = log_for_stem(&stem)?;
    match crate::session::read_status(&log) {
        Some(meta) if meta.status == crate::session::SessionStatus::Finished => None,
        _ => Some(log),
    }
}

/// Decide which session log belongs in each `pir` block of the layout.
///
/// Returns one entry per block: the log to restore there, or `None` to leave the
/// block untouched.
///
/// `live` is zellij's own pane-id → [`PaneSlot`] map, available only for the
/// session we are currently inside. A record carries its own slot in its sidecar,
/// so a session that has since exited is still placed correctly; the positional
/// fallback is a last resort for records written before slots were captured.
fn match_panes(
    session: &str,
    map: &HashMap<(String, String), PathBuf>,
    pir_blocks: &[&PaneBlock],
    live: Option<&HashMap<String, PaneSlot>>,
) -> Vec<Option<PathBuf>> {
    let mut by_block: Vec<Option<PathBuf>> = vec![None; pir_blocks.len()];

    // Strongest key first: a `pir:` title written by the pane itself. It needs no
    // live session and no inference, and it identifies the transcript even when
    // the pane was re-split or re-used since the last record was written.
    //
    // A pane that HAS such a title is settled either way: resolved, it gets its
    // own transcript; unresolvable (the log was deleted, renamed, or never
    // reached the disk), it stays empty. It must NOT then fall through to the
    // geometry join — that join is a positional guess, and letting it fill a pane
    // that has already told us exactly which transcript it held is precisely how
    // the wrong conversation ends up in the wrong pane. So the titled set is
    // recorded and the weaker joins skip those blocks.
    let mut titled = vec![false; pir_blocks.len()];
    for (i, b) in pir_blocks.iter().enumerate() {
        if let Some(stem) = block_title_session(&b.text) {
            titled[i] = true;
            by_block[i] = log_for_stem(stem);
        }
    }

    // The join key for a record, preferring zellij's live answer and falling
    // back to the geometry snapshotted into the sidecar when the session was
    // alive. The recorded slot is what makes a *dead* session rebuildable:
    // there is no live session left to interrogate.
    let key_for = |pane_id: &str, log: &PathBuf| -> Option<PaneSlot> {
        live
            .and_then(|m| m.get(pane_id).cloned())
            .or_else(|| read_record(log).and_then(|r| r.slot()))
    };

    // Only a record we can place is a candidate; a record whose slot is
    // unknown cannot be paired with anything, and guessing is what put the
    // wrong conversation in the wrong pane to begin with.
    let mut placed: Vec<(PaneSlot, &PathBuf)> = Vec::new();
    for (key, log) in map.iter().filter(|(k, _)| k.0 == session) {
        if let Some(slot) = key_for(&key.1, log) {
            placed.push((slot, log));
        }
    }

    if !placed.is_empty() {
        for (i, b) in pir_blocks.iter().enumerate() {
            // A titled pane is settled; the geometry join must not second-guess
            // it, whether the title resolved or not.
            if titled[i] {
                continue;
            }
            // A block is filled only by the single record claiming its slot; two
            // records claiming one slot is a contradiction, so claim none.
            let claimants: Vec<&PathBuf> = placed
                .iter()
                .filter(|(s, _)| *s == b.slot)
                .map(|(_, l)| *l)
                .collect();
            if claimants.len() == 1 {
                by_block[i] = Some(claimants[0].clone());
            }
        }
        return by_block;
    }

    // No geometry anywhere (records written before the field existed): the old
    // positional guess, still gated on the counts agreeing pane for pane.
    let mut recorded: Vec<(u32, &PathBuf)> = map
        .iter()
        .filter(|(k, _)| k.0 == session)
        .map(|(k, v)| (k.1.parse::<u32>().unwrap_or(u32::MAX), v))
        .collect();
    recorded.sort_by_key(|(id, _)| *id);
    if recorded.len() == pir_blocks.len() {
        for (i, (_, log)) in recorded.into_iter().enumerate() {
            if !titled[i] {
                by_block[i] = Some(log.clone());
            }
        }
    }
    by_block
}

/// The outcome of a rebuild, for reporting.
#[derive(Debug, Clone)]
pub struct RebuildReport {
    pub session: String,
    pub layout_path: PathBuf,
    pub resumed_panes: usize,
    /// Recorded panes for this session that could **not** be matched to a layout
    /// block, and so were left to resume whatever the shell gives them.
    ///
    /// This exists because "restored 0 of 2" was indistinguishable from "there
    /// was nothing to restore": a pane whose recorded slot does not match any
    /// block is silently skipped, and the run still looks successful. Reporting
    /// the shortfall by name is what turns that silence into a diagnosis — and
    /// it is deliberately separate from `resumed_panes`, which stays an honest
    /// count of what was actually rewritten.
    pub unmatched_panes: usize,
    /// The tab names named by unmatched records that are **not tabs in this
    /// layout**, deduplicated and sorted (e.g. `["Tab#4"]` when zellij calls the
    /// tab `"Tab #4"`).
    ///
    /// This is the diagnosable subset of the shortfall: a pane whose recorded
    /// slot simply disagrees is a guess we declined to make, but a record naming
    /// a tab that does not exist is *stale data* — written when the session's
    /// tabs were different, or by a build that spelled the name differently. That
    /// distinction is the difference between "the rebuild was cautious" and "this
    /// record is wrong", and only the second is actionable.
    pub unknown_tabs: Vec<String>,
}

/// How long a detached `zellij` create may take before we stop waiting for it.
///
/// The client exits in well under a second once it has no terminal, but it is a
/// server-backed call on a machine that may be busy, so this is generous. It
/// exists to bound a *hang*, not to measure normal latency.
const CREATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Create a zellij session from `layout`, **detached**, and return once we know
/// whether it came up. Bounded in time: it never waits for a terminal attach.
///
/// The child gets `setsid()` and `/dev/null` on all three fds, and **both are
/// load-bearing**:
///
///   * **null stdio alone is not enough.** `/dev/null` on fds 0/1/2 removes the
///     *streams*, not the *terminal* — the process still has a controlling tty,
///     so zellij's client opens it, attaches, and blocks forever. Measured with
///     a real pty: still hung past a 10 s timeout with stdio redirected and no
///     `setsid`.
///   * **`setsid` is what removes the controlling terminal**, and with it the
///     client's reason to attach at all.
///
/// The client then exits ~101 (`could not enable raw mode`) *after* the server
/// has created the session. That is not a failure: the session and its panes
/// live in the server, and we verified they keep running after the client dies.
/// So the exit status is reported for diagnostics but is **not** the success
/// signal — [`session_is_live`] is.
///
/// `Ok(())` means the session was observed live. `Err` carries zellij's own
/// message where there is one (a bad layout is reported on stderr and is the
/// common real failure).
fn create_session_detached(layout: &Path, session: &str) -> Result<(), String> {
    let mut cmd = Command::new("zellij");
    cmd.arg("--new-session-with-layout")
        .arg(layout)
        .args(["--session", session])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Do not let the current pane's zellij identity leak into the new session's
    // creation — with it set, zellij reads the command as "act on this session".
    cmd.env_remove("ZELLIJ")
        .env_remove("ZELLIJ_SESSION_NAME")
        .env_remove("ZELLIJ_PANE_ID");
    // No controlling terminal in the child, or its client attaches and never
    // returns. See the module docs for the measurements.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid() in the child between fork and exec — the documented
        // `pre_exec` use. The closure does nothing but the one syscall, which is
        // async-signal-safe. `setsid` only fails with EPERM if this process is
        // already a process-group leader, which a freshly forked child is not;
        // either way the call below is bounded, so the result is irrelevant.
        unsafe {
            cmd.pre_exec(|| {
                let _ = libc::setsid();
                Ok(())
            });
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Err(format!("could not run zellij: {e}")),
    };
    // Drain both pipes on threads. The client's own output is a burst of setup
    // escapes plus, on the normal path, the raw-mode panic. Crucially the
    // *panes* the layout spawns can outlive the client while still holding these
    // pipes, so a blocking `read_to_end` here could hang even though the session
    // is up — which is why the wait below has its own deadline and the readers
    // are never joined.
    let mut piped: Vec<std::sync::mpsc::Receiver<Vec<u8>>> = Vec::new();
    let streams: Vec<Box<dyn std::io::Read + Send>> = [
        child.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    .collect();
    for mut f in streams {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = f.read_to_end(&mut b);
            let _ = tx.send(b);
        });
        piped.push(rx);
    }

    // Bound the wait. A poll loop is not enough: on this child a single
    // `wait()` can take arbitrarily long, so the deadline lives in the caller and
    // the wait is handed to a worker thread.
    let (tx, rx) = std::sync::mpsc::channel::<Option<std::process::ExitStatus>>();
    let waiter = std::thread::spawn(move || {
        let _ = tx.send(child.wait().ok());
    });
    let status = match rx.recv_timeout(CREATE_TIMEOUT) {
        Ok(s) => s,
        // Timed out: leave the client running. Killing it would be wrong — it is
        // the thing holding the new session open, and the whole point is that
        // the session survives. We simply stop caring about the client.
        Err(_) => {
            drop(waiter);
            None
        }
    };

    // Establish the outcome by probing, never by the exit status: 101 with a
    // "could not enable raw mode" panic is the *successful* path.
    if session_is_live(session) {
        return Ok(());
    }

    // It failed. Report zellij's own words where there are any — a malformed
    // layout is announced on stderr and that message is the whole diagnosis.
    // Each pipe is read with a short grace period: the client has just exited
    // (or timed out), so anything it said is already buffered, but we must not
    // wait on EOF a live pane may never send.
    let mut combined = String::new();
    for p in &piped {
        if let Ok(chunk) = p.recv_timeout(PIPE_GRACE) {
            combined.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
    let detail = refusal_line(combined.as_bytes())
        .or_else(|| first_meaningful_line(combined.as_bytes()))
        .unwrap_or_else(|| match status {
            Some(s) if !s.success() => {
                let code = s.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into());
                format!("exit {code}")
            }
            _ => "create did not produce a live session".to_string(),
        });
    Err(detail)
}

/// How long to wait for the client's own output once we know it has exited.
///
/// Purely for a good error message. It must not be generous enough to become a
/// second hang: the reader only finishes at EOF, and a pane holding the pipe can
/// keep it open indefinitely, so this is the real bound on the failure path.
const PIPE_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// True when `session` is a zellij session that is actually *running*.
///
/// This is the success signal for a detached create, and it is deliberately not
/// the client's exit code (see [`create_session_detached`]).
///
/// The probe is `zellij -s <name> action list-panes -a -j`. For a live session
/// it prints a JSON array; for anything else (no such name, a *dead* session)
/// it prints a human-readable listing of live sessions on stdout — the same
/// footgun as `list-panes` on a missing session, which is why success is
/// "parses as a JSON array" and not merely "exit 0". (Verified on 0.45.1: for a
/// missing session the command exits 0 while printing a session list.)
fn session_is_live(session: &str) -> bool {
    probe_panes(session).is_some()
}

/// Bounded, minimal-stdio wrapper around `zellij -s <name> action list-panes -a -j`.
///
/// `Some` only when the output parses as the JSON pane array, i.e. the session
/// exists and answers. Never blocks for long: the call is bounded by
/// [`PROBE_TIMEOUT`], because this is what decides whether a rebuild succeeded
/// and must not itself become the hang.
fn probe_panes(session: &str) -> Option<Vec<u8>> {
    let mut child = Command::new("zellij")
        .args(["-s", session, "action", "list-panes", "-a", "-j"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let buf = reader.join().ok()?;
                return parse_live_panes(&buf).map(|_| buf);
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
}

/// Upper bound on a single existence probe.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Rebuild the zellij session `session` from its saved layout, restoring each
/// recorded `pir` pane to the conversation it held.
///
/// Writes the rewritten layout to a temp file and starts a **new** session from
/// it with `zellij --new-session-with-layout <file> --session <name>`.
///
/// Three details worth knowing (all learned by experiment, not from `--help`):
///   * `zellij -s NAME` on its own *attaches*; it does not create. Only
///     `--new-session-with-layout` always creates.
///   * The environment must not advertise the session we are currently inside,
///     or zellij treats the invocation as an action on the *current* session.
///   * That same invocation *also* attaches, always — so it is spawned detached
///     and its success judged by probing the session. See
///     [`create_session_detached`] for the measurements.
pub fn rebuild(session: &str, new_name: Option<&str>) -> Result<RebuildReport, String> {
    let kdl = load_layout(session)?;
    let map = pane_map(&session_dirs());
    let inj = inject_resume_args(&kdl, session, &map);
    let (rewritten, resumed, unmatched, unknown_tabs) =
        (inj.kdl.clone(), inj.restored, inj.unmatched, inj.unknown_tabs);

    let dir = std::env::temp_dir().join("pir-zellij");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let layout_path = dir.join(format!("{}.kdl", sanitize(session)));
    std::fs::write(&layout_path, &rewritten)
        .map_err(|e| format!("write {}: {e}", layout_path.display()))?;

    // A session by this name may already be *running*. zellij then refuses the
    // duplicate with "Session with name … already exists" — and, importantly,
    // it prints that to **stdout and exits 0**, so a status check alone reports
    // a rebuild that never happened. Detect the clash and pick a free name
    // instead, so rebuilding from inside the live session does the useful
    // thing (resurrect it under a new name) rather than silently no-op.
    //
    // Only a *live* clash matters. A name that is merely exited is still
    // refused ("already exists, but is dead"), but it is a corpse with nothing
    // in it, so it is deleted first and the rebuild then takes the name the
    // operator actually asked for.
    let base = new_name.unwrap_or(session);
    let name = match running_sessions().iter().any(|s| s == base) {
        true => free_session_name(base),
        false => {
            if session_exists_dead(base) {
                let _ = Command::new("zellij")
                    .args(["delete-session", "--force", base])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
            base.to_string()
        }
    };
    create_session_detached(&layout_path, &name).map_err(|detail| {
        format!(
            "zellij could not create session '{name}': {detail} (layout written to {})",
            layout_path.display()
        )
    })?;
    Ok(RebuildReport {
        session: name,
        layout_path,
        resumed_panes: resumed,
        unmatched_panes: unmatched,
        unknown_tabs,
    })
}

/// True when zellij holds an *exited* session under `name` — a name it will
/// refuse to reuse until the corpse is deleted.
///
/// The tell is the persisted layout: an exited session leaves
/// `session_info/<name>/session-layout.kdl` on disk (that is what "attach to
/// resurrect" reads), so its presence means the name is taken by a dead
/// session. A name zellij has never heard of has no such file.
///
/// Paired with [`running_sessions`] this is exhaustive: taken-and-live goes
/// down the rename path, taken-and-dead is deleted first, and free falls
/// through. Verified against zellij 0.45.1's own two refusals —
/// `Session with name "x" already exists.` (live, rc 0) versus
/// `Session with name "x" already exists, but is dead. Use the attach command
/// to resurrect it or, the delete-session command to kill it …` (dead, rc 1).
fn session_exists_dead(name: &str) -> bool {
    exited_layout_path(name).exists()
}

/// zellij's "that name is already taken" refusal, if the output contains one.
fn refusal_line(stdout: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|l| l.trim().trim_start_matches('\u{1b}').trim())
        .find(|l| l.contains("already exists") || l.contains("Use attach command"))
        .map(str::to_string)
}

/// First line that carries actual text (skipping terminal escape sequences).
fn first_meaningful_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|l| l.trim().trim_start_matches('\u{1b}').trim())
        .find(|l| !l.is_empty() && l.chars().any(|c| c.is_alphanumeric()))
        .map(str::to_string)
}

/// Names of zellij sessions that are **running**, from a single call.
///
/// `--short` is not enough: it also lists *exited* sessions (verified on
/// 0.45.1 — `list-sessions --short` contains names whose long form says
/// `EXITED - attach to resurrect`). Treating those as running made a rebuild of
/// a session that had since exited skip its own name and invent a `-2` suffix,
/// for no reason. The unformatted long form (`-n`) marks the state inline, so
/// one call answers the question exactly:
///
/// ```text
/// exquisite-stegosaurus [Created 24days 4h ago] (EXITED - attach to resurrect)
/// rusty-apricot [Created 17m ago] (current)
/// ```
///
/// So a line containing `EXITED` is a corpse and is filtered out; everything
/// else is live. (`-n` is required — without it the same words are wrapped in
/// colour escapes.)
fn running_sessions() -> Vec<String> {
    let Ok(out) = Command::new("zellij").args(["list-sessions", "-n"]).output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.contains(DEAD_MARKER))
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .filter(|s| !s.is_empty())
        .collect()
}

/// How zellij marks an exited session in `list-sessions` output. Matched
/// without the surrounding parentheses, which differ between versions.
const DEAD_MARKER: &str = "EXITED";

/// `base` if free, else `base-2`, `base-3`, … until unused.
///
/// "Unused" means neither live nor a corpse: zellij refuses a name it holds in
/// either state, and the operator picking the 3rd rebuild would otherwise get
/// an unexplained `-2` (or worse, have us silently delete someone's dead
/// session) because `rusty-apricot-2` is listed but exited.
fn free_session_name(base: &str) -> String {
    let taken = running_sessions();
    let name_is_free = |c: &str| !taken.iter().any(|s| s == c) && !session_exists_dead(c);
    if name_is_free(base) {
        return base.to_string();
    }
    for n in 2..1000u32 {
        let candidate = format!("{base}-{n}");
        if name_is_free(&candidate) {
            return candidate;
        }
    }
    format!("{base}-{}", crate::term::epoch())
}

/// Make a session name safe to use in a filename.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// Zellij sessions that have at least one recorded pir pane, newest record
/// first. Used by the picker/listing to offer a rebuild.
pub fn rebuildable_sessions() -> Vec<(String, usize)> {
    let map = pane_map(&session_dirs());
    let mut counts: HashMap<String, usize> = HashMap::new();
    for (session, _) in map.keys() {
        *counts.entry(session.clone()).or_insert(0) += 1;
    }
    let mut out: Vec<(String, usize)> = counts.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(session: &str, pane: &str, cwd: &str) -> PaneRecord {
        PaneRecord {
            session: session.into(),
            pane_id: pane.into(),
            cwd: cwd.into(),
            captured: 1,
            tab: String::new(),
            index: 0,
        }
    }

    /// A trimmed but structurally real dump: two tabs, a plugin, a split, and
    /// the `swap_tiled_layout` boilerplate that must survive untouched.
    const DUMP: &str = r#"layout {
    cwd "/"
    tab name="Tab #1" hide_floating_panes=true {
        pane size=1 borderless=true {
            plugin location="zellij:tab-bar"
        }
        pane command="pir" cwd="home/ai/src/pir" {
            args "-r"
            start_suspended true
        }
        pane size=1 borderless=true {
            plugin location="zellij:status-bar"
        }
    }
    tab name="Tab #2" focus=true {
        pane split_direction="vertical" {
            pane command="pir" cwd="home/other" focus=true size="50%" {
                args "-r"
            }
            pane command="htop" size="50%" {
            }
        }
    }
    swap_tiled_layout name="vertical" {
        tab max_panes=5 {
            pane {
                pane split_direction="vertical" {
                    pane
                    pane
                }
            }
        }
    }
}"#;

    #[test]
    fn finds_only_leaf_command_panes() {
        let blocks = find_pane_blocks(DUMP);
        // Only panes that actually run a command are leaves worth rewriting:
        // tab-bar/status-bar plugins are *not* (no `command=`), and the split
        // containers are descended into. So: 1 pir (tab 1), 1 pir (tab 2 split),
        // 1 htop. The bare `pane` placeholders inside `swap_tiled_layout` are
        // containers and are skipped too.
        let texts: Vec<&str> = blocks.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(blocks.len(), 3, "got: {texts:#?}");
        assert!(texts.iter().filter(|t| t.contains("command=\"pir\"")).count() == 2);
        assert!(texts.iter().any(|t| t.contains("command=\"htop\"")));
        // Plugins are not `command=` panes and must not be returned.
        assert!(!texts.iter().any(|t| t.contains("zellij:tab-bar")));
    }

    /// Write a session log plus its zellij sidecar, returning the log path.
    pub(crate) fn write_recorded_log(dir: &Path, name: &str, session: &str, pane: &str, cwd: &str) -> PathBuf {
        let _ = std::fs::create_dir_all(dir);
        let log = dir.join(name);
        std::fs::write(&log, "{\"role\":\"user\",\"blocks\":[]}\n").unwrap();
        let mut r = rec(session, pane, cwd);
        r.captured = 1;
        std::fs::write(record_path(&log), serde_json::to_string(&r).unwrap()).unwrap();
        log
    }

    #[test]
    fn injection_touches_only_pir_panes() {
        let dir = std::env::temp_dir().join(format!("pir_zellij_inj_{}", std::process::id()));
        let log3 = write_recorded_log(&dir, "pir-a-sh1.jsonl", "sess", "3", "/home/ai/src/pir");
        let log4 = write_recorded_log(&dir, "pir-b-sh2.jsonl", "sess", "4", "/home/ai/src/pir");
        let mut map = HashMap::new();
        map.insert(("sess".to_string(), "3".to_string()), log3);
        map.insert(("sess".to_string(), "4".to_string()), log4);

        let Injection { kdl: out, restored: used, .. } = inject_resume_args(DUMP, "sess", &map);
        assert_eq!(used, 2, "both pir panes should be matched");

        // The pir panes gained the resume session.
        assert!(out.contains("args \"-r\" \"pir-a-sh1\""), "got:\n{out}");
        assert!(out.contains("args \"-r\" \"pir-b-sh2\""), "got:\n{out}");
        // Non-pir panes and boilerplate are untouched.
        assert!(out.contains("pane command=\"htop\" size=\"50%\" {"), "htop pane changed");
        assert!(out.contains("plugin location=\"zellij:tab-bar\""));
        assert!(out.contains("swap_tiled_layout name=\"vertical\""));
        assert_eq!(
            DUMP.matches("swap_tiled_layout").count(),
            out.matches("swap_tiled_layout").count()
        );
    }

    #[test]
    fn injection_rewrites_cwd_to_the_recorded_one() {
        let dir = std::env::temp_dir().join(format!("pir_zellij_cwd_{}", std::process::id()));
        let log3 = write_recorded_log(&dir, "pir-a-sh1.jsonl", "sess", "3", "/home/ai/NEW");
        let log4 = write_recorded_log(&dir, "pir-b-sh2.jsonl", "sess", "4", "/home/ai/NEW");
        let mut map = HashMap::new();
        map.insert(("sess".to_string(), "3".to_string()), log3);
        map.insert(("sess".to_string(), "4".to_string()), log4);

        let Injection { kdl: out, .. } = inject_resume_args(DUMP, "sess", &map);
        assert!(out.contains("cwd=\"/home/ai/NEW\""), "cwd not rewritten:\n{out}");
        // The original cwd must be gone from the panes we rewrote.
        assert!(!out.contains("cwd=\"home/ai/src/pir\""), "old cwd still present");
        assert!(!out.contains("cwd=\"home/other\""), "old cwd still present");
    }

    #[test]
    fn a_pane_without_pir_is_left_alone() {
        let kdl = r#"layout {
    tab {
        pane command="vim" {
        }
    }
}"#;
        let Injection { kdl: out, restored: used, .. } = inject_resume_args(kdl, "sess", &HashMap::new());
        assert_eq!(used, 0);
        assert_eq!(out, kdl, "a layout with no pir panes must be byte-identical");
    }

    /// `live_panes` runs on the prompt path, and `zellij action` against a
    /// session that no longer exists can block instead of failing. It must be
    /// bounded, or a stale `ZELLIJ_SESSION_NAME` hangs every prompt.
    #[test]
    fn a_nonexistent_session_does_not_hang_the_prompt_path() {
        let old = (
            std::env::var_os("ZELLIJ_SESSION_NAME"),
            std::env::var_os("ZELLIJ_PANE_ID"),
        );
        unsafe {
            std::env::set_var("ZELLIJ_SESSION_NAME", "pir-no-such-session-xyz");
            std::env::set_var("ZELLIJ_PANE_ID", "1");
        }
        let t = std::time::Instant::now();
        let _ = live_panes("pir-no-such-session-xyz");
        let elapsed = t.elapsed();
        match old.0 {
            Some(v) => unsafe { std::env::set_var("ZELLIJ_SESSION_NAME", v) },
            None => unsafe { std::env::remove_var("ZELLIJ_SESSION_NAME") },
        }
        match old.1 {
            Some(v) => unsafe { std::env::set_var("ZELLIJ_PANE_ID", v) },
            None => unsafe { std::env::remove_var("ZELLIJ_PANE_ID") },
        }
        // Whether zellij reports nothing for the session or blocks and we time
        // out, either is fine; hanging forever is not.
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "live_panes must be bounded, took {elapsed:?}"
        );
    }

    /// The regression this fixes: `capture()` runs on **every prompt** (via
    /// `record` → `mark_status`), and it used to call `live_panes` inline, holding
    /// the cache lock. With a stale `ZELLIJ_SESSION_NAME` that meant a `zellij`
    /// spawn (and up to a 2 s wait) on every single prompt.
    ///
    /// It must now return immediately no matter what zellij does: the lookup is a
    /// cached read and any refresh happens on a detached thread. This asserts the
    /// prompt path costs milliseconds, not seconds.
    #[test]
    fn capture_never_waits_on_zellij_on_the_prompt_path() {
        let old = (
            std::env::var_os("ZELLIJ_SESSION_NAME"),
            std::env::var_os("ZELLIJ_PANE_ID"),
        );
        unsafe {
            // A session that cannot exist: exactly the stale-env case that used
            // to hang the prompt path.
            std::env::set_var("ZELLIJ_SESSION_NAME", "pir-nonexistent-capture-xyz");
            std::env::set_var("ZELLIJ_PANE_ID", "1");
        }
        // Clear the cache so this is a cold lookup, the worst case.
        *SLOT_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;

        let t = std::time::Instant::now();
        let rec = capture().expect("inside zellij env, a record is built");
        let elapsed = t.elapsed();

        match old.0 {
            Some(v) => unsafe { std::env::set_var("ZELLIJ_SESSION_NAME", v) },
            None => unsafe { std::env::remove_var("ZELLIJ_SESSION_NAME") },
        }
        match old.1 {
            Some(v) => unsafe { std::env::set_var("ZELLIJ_PANE_ID", v) },
            None => unsafe { std::env::remove_var("ZELLIJ_PANE_ID") },
        }

        // A generous bound: the point is "does not wait for the 2 s subprocess
        // deadline", not micro-latency on a loaded CI box.
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "capture() must not wait on zellij (took {elapsed:?})"
        );
        // With a cold cache and no resolvable session there is no geometry yet —
        // that is fine (the rebuild treats it as unmatched); what matters is that
        // building the record did not block.
        assert_eq!(rec.session, "pir-nonexistent-capture-xyz");
        assert_eq!(rec.pane_id, "1");
    }

    /// A slow refresh must not accumulate one thread per prompt: a second
    /// `slot_cache` call while a refresh is in flight must start no second one.
    #[test]
    fn slot_refresh_is_single_flight() {
        use std::sync::atomic::Ordering;
        *SLOT_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // Simulate an in-flight refresh and confirm a lookup neither clears the
        // flag nor spawns (the lookup must still answer from cache).
        SLOT_REFRESH_IN_FLIGHT.store(true, Ordering::SeqCst);
        let t = std::time::Instant::now();
        let _ = slot_cache("some-session", "1");
        assert!(
            t.elapsed() < std::time::Duration::from_millis(200),
            "a lookup with a refresh in flight must not block"
        );
        assert!(
            SLOT_REFRESH_IN_FLIGHT.load(Ordering::SeqCst),
            "a lookup must not disturb an in-flight refresh"
        );
        SLOT_REFRESH_IN_FLIGHT.store(false, Ordering::SeqCst);
    }

    /// A record set that does **not** correspond one-to-one with the dump's pir
    /// panes is ambiguous: without a live session we cannot tell which layout
    /// block belongs to pane 3, and guessing would restore the *wrong*
    /// conversation into a pane. So the fallback declines and the layout is
    /// rebuilt unchanged.
    #[test]
    fn mismatched_record_count_restores_nothing() {
        let dir = std::env::temp_dir().join(format!("pir_zellij_unmatched_{}", std::process::id()));
        let log = write_recorded_log(&dir, "pir-y-sh1.jsonl", "sess", "3", "/x");
        let mut map = HashMap::new();
        map.insert(("sess".to_string(), "3".to_string()), log);
        // One record but TWO pir panes: pairing them positionally would put
        // pane 3's conversation into whichever block happens to be first.
        let Injection { kdl: out, restored: used, .. } = inject_resume_args(DUMP, "sess", &map);
        assert_eq!(used, 0, "an ambiguous mapping must restore nothing");
        assert_eq!(out, DUMP, "layout must be byte-identical when nothing is claimed");
    }

    #[test]
    fn records_roundtrip_and_skip_non_zellij() {
        let dir = std::env::temp_dir().join(format!("pir_zellij_rec_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let log = dir.join("pir-z-sh1.jsonl");
        let _ = std::fs::remove_file(record_path(&log));

        // With no zellij env, nothing is written.
        // SAFETY: test-local env manipulation; these vars are not read concurrently.
        let old = (
            std::env::var_os("ZELLIJ_SESSION_NAME"),
            std::env::var_os("ZELLIJ_PANE_ID"),
        );
        unsafe {
            std::env::remove_var("ZELLIJ_SESSION_NAME");
            std::env::remove_var("ZELLIJ_PANE_ID");
        }
        assert!(!in_zellij());
        record(&log);
        assert!(read_record(&log).is_none(), "must not record outside zellij");

        // With the zellij env, it round-trips.
        unsafe {
            std::env::set_var("ZELLIJ_SESSION_NAME", "my-session");
            std::env::set_var("ZELLIJ_PANE_ID", "7");
        }
        assert!(in_zellij());
        record(&log);
        let got = read_record(&log).expect("record written");
        assert_eq!(got.session, "my-session");
        assert_eq!(got.pane_id, "7");
        assert!(!got.cwd.is_empty());
        assert!(got.captured > 0);

        match old {
            (Some(a), Some(b)) => unsafe {
                std::env::set_var("ZELLIJ_SESSION_NAME", a);
                std::env::set_var("ZELLIJ_PANE_ID", b);
            },
            _ => unsafe {
                std::env::remove_var("ZELLIJ_SESSION_NAME");
                std::env::remove_var("ZELLIJ_PANE_ID");
            },
        }
        let _ = std::fs::remove_file(record_path(&log));
    }

    #[test]
    fn newest_record_per_pane_wins() {
        let dir = std::env::temp_dir().join(format!("pir_zellij_newest_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let old = dir.join("pir-old-sh1.jsonl");
        let new = dir.join("pir-new-sh2.jsonl");
        // A record only ever exists alongside its session log, so the fixture
        // must create both.
        std::fs::write(&old, "{\"role\":\"user\",\"blocks\":[]}\n").unwrap();
        std::fs::write(&new, "{\"role\":\"user\",\"blocks\":[]}\n").unwrap();
        let mut a = rec("sess", "3", "/old");
        a.captured = 100;
        let mut b = rec("sess", "3", "/new");
        b.captured = 200;
        std::fs::write(record_path(&old), serde_json::to_string(&a).unwrap()).unwrap();
        std::fs::write(record_path(&new), serde_json::to_string(&b).unwrap()).unwrap();

        let map = pane_map(&[dir.clone()]);
        let got = map.get(&("sess".to_string(), "3".to_string())).expect("mapped");
        assert_eq!(got, &new, "the freshest record must win");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_keeps_names_filename_safe() {
        assert_eq!(sanitize("vitreous-hill"), "vitreous-hill");
        assert_eq!(sanitize("a/b c"), "a_b_c");
    }

    /// zellij announces a taken session name on **stdout** and still exits 0,
    /// so a status check alone reports a rebuild that never happened. This is
    /// the exact text and exit behaviour observed from a live zellij 0.45.1.
    #[test]
    fn detects_a_taken_session_name_despite_a_zero_exit() {
        let out = b"Session with name \"vitreous-hill\" already exists. Use attach command to \
                    connect to it or specify a different name.\n";
        let line = refusal_line(out).expect("must be recognised as a refusal");
        assert!(line.contains("already exists"), "got: {line}");
    }

    /// A successful start emits a burst of terminal-setup escapes before
    /// anything else. That must not be mistaken for a refusal, and must not be
    /// surfaced as an error message.
    #[test]
    fn terminal_setup_escapes_are_not_a_refusal() {
        let out = b"\x1b[?1049h\x1b[?1l\x1b=\x1b[?1000l\x1b[?2031h\x1b[?996n";
        assert_eq!(refusal_line(out), None, "setup escapes must not read as a refusal");
    }

    /// `free_session_name` must not hand back a name that is already running.
    #[test]
    fn free_name_avoids_running_sessions() {
        // Pure check against a taken-list, so it does not depend on the host's
        // real zellij state.
        let taken = vec!["sess".to_string(), "sess-2".to_string(), "sess-3".to_string()];
        let next = |base: &str| {
            (2..1000u32)
                .map(|n| format!("{base}-{n}"))
                .find(|c| !taken.iter().any(|s| s == c))
                .unwrap()
        };
        assert_eq!(next("sess"), "sess-4");
    }

    /// The title is the *authoritative* pane→transcript key, so it must be read
    /// off the pane's own header — and a child's title must never leak upward.
    #[test]
    fn block_title_session_reads_only_our_marker() {
        assert_eq!(
            block_title_session("pane command=\"pir\" name=\"pir:s-123\" cwd=\"/x\" {\n}"),
            Some("s-123")
        );
        // A hand-set title is not ours: no match, so the pane is matched the old way.
        assert_eq!(block_title_session("pane command=\"pir\" name=\"logs\" {\n}"), None);
        assert_eq!(block_title_session("pane command=\"pir\" {\n}"), None);
        // Brace-less one-liner still has a header.
        assert_eq!(block_title_session("pane name=\"pir:s-9\" cwd=\"/y\""), Some("s-9"));
        // A CHILD's name= belongs to the child, not this container pane.
        let container = "pane split_direction=\"vertical\" {\n    pane command=\"pir\" name=\"pir:kid\" {\n    }\n}";
        assert_eq!(block_title_session(container), None);
    }

    /// A title must win over the geometry join, which is a guess by comparison.
    ///
    /// The title is read from real session directories, so the fixture log is
    /// written where `log_for_stem` looks and removed afterwards — putting it in
    /// the cwd's `.pir/sessions` is enough, and it is cleaned up on every path.
    #[test]
    fn titled_pane_is_not_overridden_by_geometry() {
        let dir = std::env::current_dir().unwrap().join(".pir").join("sessions");
        let _ = std::fs::create_dir_all(&dir);
        let log = dir.join("geometry-wins.jsonl");
        std::fs::write(&log, "{}\n").unwrap();
        let rec = PaneRecord {
            session: "sess".into(),
            pane_id: "9".into(),
            cwd: "/x".into(),
            captured: 1,
            tab: "Tab #1".into(),
            index: 0,
        };
        std::fs::write(record_path(&log), serde_json::to_string(&rec).unwrap()).unwrap();

        let kdl = concat!(
            "layout {\n",
            "  tab name=\"Tab #1\" {\n",
            "    pane command=\"pir\" name=\"pir:titled\" cwd=\"/x\" {\n",
            "      args \"-r\"\n",
            "    }\n",
            "  }\n",
            "}\n",
        );
        // A record whose recorded geometry points at this very pane. If the title
        // were ignored, the geometry join would fill the pane with
        // `geometry-wins` — the exact "wrong conversation in the wrong pane" bug.
        let mut map: HashMap<(String, String), PathBuf> = HashMap::new();
        map.insert(("sess".into(), "9".into()), log.clone());
        let Injection { kdl: out, restored: used, .. } = inject_resume_args(kdl, "sess", &map);
        assert_eq!(used, 0, "an unresolvable title must not fall through to the guess: {out}");
        assert!(!out.contains("geometry-wins"), "{out}");

        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_file(record_path(&log));
    }

    /// The end-to-end shape: a title that resolves gets exactly that transcript.
    #[test]
    fn titled_pane_restores_its_own_transcript() {
        let dir = std::env::current_dir().unwrap().join(".pir").join("sessions");
        let _ = std::fs::create_dir_all(&dir);
        let log = dir.join("mytalk.jsonl");
        std::fs::write(&log, "{}\n").unwrap();
        let rec = PaneRecord {
            session: "sess".into(),
            pane_id: "1".into(),
            cwd: "/x".into(),
            captured: 1,
            tab: String::new(),
            index: 0,
        };
        std::fs::write(record_path(&log), serde_json::to_string(&rec).unwrap()).unwrap();

        let kdl = format!(
            "layout {{\n  tab name=\"Tab #1\" {{\n    pane command=\"pir\" name=\"{}\" cwd=\"/x\" {{\n    }}\n  }}\n}}\n",
            pane_title("mytalk")
        );
        let Injection { kdl: out, restored: used, .. } = inject_resume_args(&kdl, "sess", &HashMap::new());
        assert_eq!(used, 1, "the titled pane must be restored: {out}");
        assert!(out.contains("args \"-r\" \"mytalk\""), "{out}");
        assert!(out.contains("cwd=\"/x\""), "the recorded cwd must still win: {out}");

        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_file(record_path(&log));
    }

    /// A title we do not own, and a title naming a transcript that is gone, must
    /// both leave the pane exactly as zellij dumped it.
    #[test]
    fn foreign_or_missing_titles_are_left_alone() {
        let kdl = concat!(
            "layout {\n",
            "  tab name=\"Tab #1\" {\n",
            "    pane command=\"pir\" name=\"operator-set\" {\n",
            "    }\n",
            "    pane command=\"pir\" name=\"pir:vanished\" {\n",
            "    }\n",
            "  }\n",
            "}\n",
        );
        let Injection { kdl: out, restored: used, .. } = inject_resume_args(kdl, "sess", &HashMap::new());
        assert_eq!(used, 0, "nothing resolvable, so nothing is rewritten: {out}");
        assert_eq!(out, kdl, "the dumped layout must survive byte-for-byte");
    }

    /// The startup read-back: a bare `pir` in a resurrected pane resolves the
    /// transcript its own `pir:<stem>` title names — the token plain `zellij
    /// attach` drops on the floor. This asserts on the value startup itself
    /// uses ([`resurrect_log_for_title`]), not a recomputation.
    ///
    /// The fixture lives where `log_for_stem` looks (the cwd-local sessions
    /// dir, like the title tests above) under a pid-unique stem, so no live
    /// zellij and no real transcript can interfere; everything is removed
    /// afterwards.
    #[test]
    fn startup_title_resumes_the_transcript_it_names() {
        let dir = std::env::current_dir().unwrap().join(".pir").join("sessions");
        let _ = std::fs::create_dir_all(&dir);
        let stem = format!("startup-title-resume-{}", std::process::id());
        let log = dir.join(format!("{stem}.jsonl"));
        std::fs::write(&log, "{}\n").unwrap();

        // Our own title, live transcript: startup must pick it up.
        let got = resurrect_log_for_title(&pane_title(&stem));
        assert_eq!(got, Some(log.clone()), "startup must resume the titled transcript");

        // A finished transcript stays finished: `/finished` means "do not come back".
        crate::session::mark_finished(&log);
        assert_eq!(
            resurrect_log_for_title(&pane_title(&stem)),
            None,
            "a finished session must not be resurrected by its title"
        );

        // A hand-set title is not ours, and a stem with no log is nothing.
        assert_eq!(resurrect_log_for_title("operator-set"), None);
        assert_eq!(resurrect_log_for_title("pir:vanished-startup-xyz"), None);
        assert_eq!(resurrect_log_for_title(""), None);

        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_file(crate::session::status_path(&log));
    }

    /// Repro: plain `zellij attach` resurrects bare `pir`, which starts a
    /// fresh session instead of `pir --session <stem>`.
    ///
    /// The attach path is title-driven: [`record`] claims the pane's title as
    /// `pir:<stem>`, zellij persists it in the layout's `name=`, and a bare
    /// `pir` at startup reads it back ([`resurrect_from_own_title`]). The bug
    /// was in the claim guard: a pane zellij just resurrected reports its
    /// title as the bare command name (`"pir"`) — zellij fills that in, the
    /// operator never typed it — and the guard treated any non-empty,
    /// non-ours title as operator-chosen and bailed. So no resurrected pane
    /// was ever claimed (measured live: every `pir` pane titled `"pir"`), no
    /// transcript was ever named, and every attach started fresh.
    ///
    /// This drives the claim decision itself — the value [`claim_pane_title`]
    /// consults — with the exact title a resurrected pane reports, so it fails
    /// while the guard bails on zellij's default. (Editing the persisted
    /// layout instead was tried and measured dead: the running server
    /// regenerates that file from live state every ~30 s and reverts any edit
    /// in under 10 s. The title is the only channel that sticks.)
    #[test]
    fn plain_attach_claims_a_resurrected_panes_default_title() {
        // What the operator typed is sacred; what zellij filled in is not.
        assert!(
            !is_default_pane_title("logs"),
            "an operator title must never read as default"
        );
        assert!(
            !is_default_pane_title("pir:pir-20260926-085546-sh945107"),
            "our own claim must never read as default"
        );
        assert!(
            !is_default_pane_title(""),
            "empty is unset, not default — handled by its own branch"
        );
        // What a resurrected pane actually reports (measured live).
        assert!(
            is_default_pane_title("pir"),
            "zellij's default title for a pir pane must be claimable"
        );
        assert!(
            is_default_pane_title("pir · pir"),
            "zellij's framed default must be claimable too"
        );

        // The guard decision itself, asserted on the value `claim_pane_title`
        // consults ([`title_is_claimable`]) — not a recomputation. A
        // resurrected pane's `"pir"` must fall on the claim side; that is the
        // whole bug.
        assert!(
            title_is_claimable("pir:abc"),
            "our title: already claimed, rename is a no-op"
        );
        assert!(title_is_claimable(""), "unset: claim");
        assert!(
            title_is_claimable("pir"),
            "resurrected default: must claim, not bail"
        );
        assert!(
            title_is_claimable("pir · pir"),
            "framed default: must claim, not bail"
        );
        assert!(!title_is_claimable("logs"), "operator title: hands off");
        assert!(!title_is_claimable("bash-o4"), "operator title: hands off");
        // A pir started from a shell still carries the shell's OSC title
        // (`🐧<dir>`, measured live as `🐧pir` on pane 13) when the first
        // prompt claims it — that is zellij/the shell talking, not the
        // operator, so it must fall on the claim side too.
        assert!(
            is_shell_title("🐧pir"),
            "shell OSC title must read as shell-emitted"
        );
        assert!(
            is_shell_title("🐧bash-o4"),
            "shell OSC title must read as shell-emitted"
        );
        assert!(!is_shell_title("logs"), "operator title is not shell OSC");
        assert!(!is_shell_title(""), "empty is unset, not shell OSC");
        assert!(
            title_is_claimable("🐧pir"),
            "shell-started pir: must claim, not bail"
        );
        assert!(
            title_is_claimable("🐧bash-o4"),
            "shell-started pir: must claim, not bail"
        );
    }

    /// Repro for the second half: zellij reuses small ids across plugins and
    /// terminals (measured live on this very session: id 0 is both the
    /// `zellij:link` plugin *and* a terminal, id 2 both `status-bar` and a
    /// terminal), and `list-panes -a -j` lists plugins first. So an unfiltered
    /// id match reads the *plugin's* title — `(.) - zellij:link`,
    /// `zellij:status-bar` — never the pane's. Neither is claimable, so the
    /// claim bailed on every low-id pane even though the pane itself carried a
    /// claimable title (`pir · bash-o4` on both).
    ///
    /// This parses the same shape `list-panes -a -j` emits and asserts on the
    /// value [`current_pane_title`] itself returns, not a recomputation: with
    /// a plugin shadowing the id, the pane's own title must still win.
    #[test]
    fn title_read_skips_plugin_entries_sharing_the_id() {
        // Exact shape measured live: plugin first, terminal second, same id.
        let out = serde_json::json!([
            { "id": 0, "is_plugin": true,  "title": "(.) - zellij:link" },
            { "id": 0, "is_plugin": false, "title": "pir · bash-o4" },
            { "id": 2, "is_plugin": true,  "title": "zellij:status-bar" },
            { "id": 2, "is_plugin": false, "title": "pir · bash-o4" },
        ]);
        let got = |pane: &str| title_of_pane(&out, pane);
        assert_eq!(got("0"), Some("pir · bash-o4".to_string()));
        assert_eq!(got("2"), Some("pir · bash-o4".to_string()));
        // And both of those are claimable (zellij's framed default), so the
        // claim proceeds instead of bailing on the plugin's title.
        assert!(title_is_claimable(&got("0").unwrap()));
        assert!(title_is_claimable(&got("2").unwrap()));
    }

    #[test]
    fn set_attribute_replaces_only_the_value() {
        let block = "pane command=\"pir\" cwd=\"old\" size=\"50%\" {\n}";
        let out = set_attribute(block, "cwd", "/new");
        assert_eq!(out, "pane command=\"pir\" cwd=\"/new\" size=\"50%\" {\n}");
        // Absent key is added after `pane`.
        let out = set_attribute("pane size=1 {\n}", "cwd", "/x");
        assert_eq!(out, "pane cwd=\"/x\" size=1 {\n}");
    }
}

#[cfg(test)]
mod real_dump_tests {
    //! Verify the KDL handling against a layout captured from a **real** zellij
    //! session (`zellij action dump-layout`), not a hand-written fixture: the
    //! real thing is ~375 lines, mostly `swap_tiled_layout` boilerplate, with 7
    //! `pir` panes across 4 tabs including vertical splits.
    use super::*;
    use super::tests::write_recorded_log;

    const REAL: &str = include_str!("../tests/fixtures/real-dump.kdl");

    #[test]
    fn scanner_finds_exactly_the_real_pir_panes() {
        let blocks = find_pane_blocks(REAL);
        let pir = blocks
            .iter()
            .filter(|b| b.text.contains("command=\"pir\""))
            .count();
        assert_eq!(pir, 7, "the real dump has 7 pir panes across 4 tabs");
        // Every returned block must be a command pane (plugins excluded).
        for b in &blocks {
            assert!(b.text.contains("command="), "non-command block returned: {}", b.text);
        }
    }

    /// The bug this replaces: panes were paired **positionally** (n-th recorded
    /// pane id ↔ n-th `pir` block in the dump). That is only right when the
    /// recorded pane set and the layout agree pane-for-pane and in order. When
    /// zellij tells us the real pane ids, the mapping must follow zellij, not
    /// document order.
    ///
    /// This layout has two pir panes in the same tab, split vertically. Pane id
    /// `7` is the *second* one by geometry but the *first* by id order, so the
    /// old code would have swapped the two conversations.
    #[test]
    fn live_pane_ids_beat_document_order() {
        let kdl = r#"layout {
    tab name="work" {
        pane split_direction="vertical" {
            pane command="pir" cwd="/a" {
            }
            pane command="pir" cwd="/b" {
            }
        }
    }
}"#;
        let blocks = find_pane_blocks(kdl);
        let pir_blocks: Vec<&PaneBlock> =
            blocks.iter().filter(|b| b.text.contains("command=\"pir\"")).collect();
        assert_eq!(pir_blocks.len(), 2);
        // Layout ordinals: left pane 0, right pane 1.
        assert_eq!(pir_blocks[0].slot, PaneSlot { tab: "work".into(), index: 0 });
        assert_eq!(pir_blocks[1].slot, PaneSlot { tab: "work".into(), index: 1 });

        let dir = std::env::temp_dir().join(format!("pir_zellij_live_{}", std::process::id()));
        // `zellij action list-panes -a -j` for this tab: pane 2 is on the RIGHT
        // (x=135) and pane 7 on the LEFT (x=0), the reverse of id order.
        let live_json = serde_json::json!([
            { "id": 0, "is_plugin": true,  "tab_name": "work", "pane_x": 0,   "pane_y": 1 },
            { "id": 2, "is_plugin": false, "tab_name": "work", "pane_x": 135, "pane_y": 1 },
            { "id": 7, "is_plugin": false, "tab_name": "work", "pane_x": 0,   "pane_y": 1 }
        ]);
        let live = parse_live_panes(live_json.to_string().as_bytes()).expect("parse");
        // Sorting by (y, x): pane 7 (x=0) is index 0, pane 2 (x=135) is index 1.
        assert_eq!(live.get("7"), Some(&PaneSlot { tab: "work".into(), index: 0 }));
        assert_eq!(live.get("2"), Some(&PaneSlot { tab: "work".into(), index: 1 }));

        // Record each pane id against its own conversation.
        let left = write_recorded_log(&dir, "pir-left.jsonl", "sess", "7", "/left");
        let right = write_recorded_log(&dir, "pir-right.jsonl", "sess", "2", "/right");
        let mut map = HashMap::new();
        map.insert(("sess".to_string(), "7".to_string()), left);
        map.insert(("sess".to_string(), "2".to_string()), right);

        let by_block = match_panes("sess", &map, &pir_blocks, Some(&live));
        let (out, used) = rewrite_with(kdl, &pir_blocks, &by_block);
        assert_eq!(used, 2);

        // The left-hand block must hold pane 7's conversation, the right pane
        // 2's — i.e. geometry, not id order, decided it.
        let left_block = out.find("cwd=\"/left\"").expect("left pane restored");
        let right_block = out.find("cwd=\"/right\"").expect("right pane restored");
        assert!(left_block < right_block, "left pane must come first in the layout");
        assert!(out.contains("args \"-r\" \"pir-left\""), "got:\n{out}");
        assert!(out.contains("args \"-r\" \"pir-right\""), "got:\n{out}");

        // The two strategies must actually disagree here, or this test proves
        // nothing. Pairing by numeric id order (2 then 7) would put pane 2's
        // conversation — `pir-right` — in the LEFT block; the live geometry map
        // puts it on the right, where pane 2 actually is.
        let by_id_order = match_panes("sess", &map, &pir_blocks, None);
        let naive = by_id_order
            .iter()
            .position(|l| {
                l.as_deref().and_then(|p| p.file_stem()).is_some_and(|s| s == "pir-right")
            });
        assert_eq!(naive, Some(0), "id-order pairing would wrongly put pir-right in the left block");

        // The live mapping is the opposite, and correct.
        let authoritative = by_block
            .iter()
            .position(|l| {
                l.as_deref().and_then(|p| p.file_stem()).is_some_and(|s| s == "pir-right")
            });
        assert_eq!(authoritative, Some(1), "live geometry must place pir-right in the right block");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The gap this closes: a session that has *exited* cannot be interrogated
    /// (`zellij action list-panes` only reports the session we are inside), so
    /// the record's own snapshotted slot must place the panes. Here the pane ids
    /// deliberately run counter to the layout order — id 7 is the left pane.
    #[test]
    fn recorded_slots_place_panes_of_a_dead_session() {
        let kdl = r#"layout {
    tab name="work" {
        pane split_direction="vertical" {
            pane command="pir" cwd="/a" {
            }
            pane command="pir" cwd="/b" {
            }
        }
    }
}"#;
        let dir = std::env::temp_dir().join(format!("pir_zellij_dead_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        // Write records carrying their geometry — exactly what `capture()` does
        // while the session is alive — with no live session to consult now.
        let write = |name: &str, pane: &str, cwd: &str, tab: &str, index: u32| {
            let log = dir.join(name);
            std::fs::write(&log, "{}\n").unwrap();
            let r = PaneRecord {
                session: "dead".into(),
                pane_id: pane.into(),
                cwd: cwd.into(),
                captured: 1,
                tab: tab.into(),
                index,
            };
            std::fs::write(record_path(&log), serde_json::to_string(&r).unwrap()).unwrap();
            log
        };
        let left = write("pir-left.jsonl", "7", "/left", "work", 0);
        let right = write("pir-right.jsonl", "2", "/right", "work", 1);
        let mut map = HashMap::new();
        map.insert(("dead".to_string(), "7".to_string()), left);
        map.insert(("dead".to_string(), "2".to_string()), right);

        let blocks = find_pane_blocks(kdl);
        let pir_blocks: Vec<&PaneBlock> =
            blocks.iter().filter(|b| b.text.contains("command=\"pir\"")).collect();

        // `live = None`: the session is gone, so only the recorded slots remain.
        let by_block = match_panes("dead", &map, &pir_blocks, None);
        let (out, used) = rewrite_with(kdl, &pir_blocks, &by_block);
        assert_eq!(used, 2, "a dead session must still restore both panes");
        assert!(out.contains("args \"-r\" \"pir-left\""), "got:\n{out}");
        assert!(out.contains("args \"-r\" \"pir-right\""), "got:\n{out}");
        // Geometry, not id order, decided it: the left block holds pane 7.
        let l = out.find("cwd=\"/left\"").expect("left restored");
        let r = out.find("cwd=\"/right\"").expect("right restored");
        assert!(l < r, "pane 7 (left) must be restored into the left block");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Old sidecars predate the `tab` field and must still parse (and simply
    /// carry no geometry) rather than failing the whole rebuild.
    #[test]
    fn a_legacy_sidecar_without_geometry_still_parses() {
        let legacy = r#"{"session":"s","pane_id":"3","cwd":"/x","captured":10}"#;
        let r: PaneRecord = serde_json::from_str(legacy).expect("legacy sidecar must parse");
        assert_eq!(r.session, "s");
        assert_eq!(r.pane_id, "3");
        assert_eq!(r.slot(), None, "no captured geometry means no slot");
    }

    /// A record whose `tab` is not a tab in the layout — a **stale sidecar**.
    ///
    /// This is the failure observed in the field: a record written by an older
    /// build carried `tab = "Tab#4"` while zellij names tabs `"Tab #4"` (with a
    /// space), so its slot matched no block and the pane silently came back
    /// fresh. `inject_resume_args` must therefore report the shortfall rather
    /// than let a partial rebuild read as a clean one.
    #[test]
    fn a_stale_tab_name_is_reported_as_unmatched() {
        let kdl = r#"layout {
    tab name="Tab #4" {
        pane command="pir" cwd="/work" {
        }
    }
}"#;
        let dir = std::env::temp_dir().join(format!("pir_zellij_stale_{}", std::process::id()));
        let log = super::tests::write_recorded_log(&dir, "pir-stale-sh1.jsonl", "s", "7", "/work");
        // A tab name no layout tab has: exactly the `Tab#4` vs `Tab #4` case.
        let mut rec = read_record(&log).expect("record");
        rec.tab = "Tab#4".into();
        rec.index = 0;
        std::fs::write(record_path(&log), serde_json::to_string(&rec).unwrap()).unwrap();

        let mut map: HashMap<(String, String), PathBuf> = HashMap::new();
        map.insert(("s".to_string(), "7".to_string()), log);
        let inj = inject_resume_args(kdl, "s", &map);
        assert_eq!(inj.restored, 0, "a stale tab name must not place a pane");
        assert_eq!(
            inj.unmatched, 1,
            "a stale record must be counted, so the partial rebuild is visible"
        );
        assert_eq!(
            inj.unknown_tabs,
            vec!["Tab#4".to_string()],
            "the stale tab must be named, since that is the actionable diagnosis"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The happy path must still report a clean run, so the new `unmatched`
    /// counter cannot cry wolf: a record whose slot *does* match is restored and
    /// counts as zero unmatched.
    #[test]
    fn a_placeable_record_reports_no_shortfall() {
        let kdl = r#"layout {
    tab name="Tab #4" {
        pane command="pir" cwd="/work" {
        }
    }
}"#;
        let dir = std::env::temp_dir().join(format!("pir_zellij_ok_{}", std::process::id()));
        let log = super::tests::write_recorded_log(&dir, "pir-ok-sh1.jsonl", "s", "7", "/work");
        let mut rec = read_record(&log).expect("record");
        rec.tab = "Tab #4".into();
        rec.index = 0;
        std::fs::write(record_path(&log), serde_json::to_string(&rec).unwrap()).unwrap();

        let mut map: HashMap<(String, String), PathBuf> = HashMap::new();
        map.insert(("s".to_string(), "7".to_string()), log);
        let inj = inject_resume_args(kdl, "s", &map);
        assert_eq!(inj.restored, 1, "a matching slot must restore");
        assert_eq!(inj.unmatched, 0, "a clean rebuild must not warn");
        assert!(inj.kdl.contains("args \"-r\" \"pir-ok-sh1\""), "{}", inj.kdl);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two records claiming one slot is a contradiction (the pane changed hands
    /// mid-capture, or a re-split invalidated the cache). Refuse to place
    /// either: guessing is what this whole change exists to prevent.
    #[test]
    fn contradictory_slots_restore_neither_pane() {
        let kdl = r#"layout {
    tab name="work" {
        pane command="pir" cwd="/a" {
        }
    }
}"#;
        let dir = std::env::temp_dir().join(format!("pir_zellij_dup_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let write = |name: &str, pane: &str| {
            let log = dir.join(name);
            std::fs::write(&log, "{}\n").unwrap();
            let r = PaneRecord {
                session: "s".into(),
                pane_id: pane.into(),
                cwd: "/x".into(),
                captured: 1,
                tab: "work".into(),
                index: 0,
            };
            std::fs::write(record_path(&log), serde_json::to_string(&r).unwrap()).unwrap();
            log
        };
        let a = write("pir-a.jsonl", "3");
        let b = write("pir-b.jsonl", "4");
        let mut map = HashMap::new();
        map.insert(("s".to_string(), "3".to_string()), a);
        map.insert(("s".to_string(), "4".to_string()), b);

        let blocks = find_pane_blocks(kdl);
        let pir_blocks: Vec<&PaneBlock> =
            blocks.iter().filter(|b| b.text.contains("command=\"pir\"")).collect();
        let by_block = match_panes("s", &map, &pir_blocks, None);
        let (out, used) = rewrite_with(kdl, &pir_blocks, &by_block);
        assert_eq!(used, 0, "a contested slot must restore nothing");
        assert_eq!(out, kdl, "layout must be byte-identical when nothing is claimed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A tab holding a `pir` pane and an `edit=` pane — the exact shape of the
    /// real host's Tab #3.
    ///
    /// This is the ordinal bug, and it is the reason a restore silently
    /// restores nothing. zellij reports the `edit=` pane as a *terminal* pane
    /// (`is_plugin: false`), so `parse_live_panes` counts it and gives the next
    /// pane ordinal 1. `find_pane_blocks` counts only leaf `command=` panes, so
    /// in the layout the `edit=` pane is not a leaf and the `pir` pane after it
    /// is numbered 0. The two sides therefore disagree about the ordinal of
    /// every pane that follows a non-`command=` terminal pane, so the recorded
    /// slot matches no block, `match_panes` places nothing, and the pane comes
    /// back un-restored — while the run still reports success.

    /// The real session's own `list-panes` output, so the `(y, x)` ordering is
    /// exercised against the messy real thing: several tabs, nested splits, and
    /// panes at different rows within one tab.
    #[test]
    fn real_live_panes_parse_into_ordered_slots() {
        let bytes = include_bytes!("../tests/fixtures/live-panes.json");
        let live = parse_live_panes(bytes).expect("real list-panes output must parse");

        // The `pir` tab: three panes, two on the top row and one below-right.
        // Ordered by (y, x): 2 (x=0), 3 (x=135), then 8 (y=36).
        assert_eq!(live.get("2"), Some(&PaneSlot { tab: "pir".into(), index: 0 }));
        assert_eq!(live.get("3"), Some(&PaneSlot { tab: "pir".into(), index: 1 }));
        assert_eq!(live.get("8"), Some(&PaneSlot { tab: "pir".into(), index: 2 }));

        // The `corro` tab spans three rows — the case document order gets wrong.
        assert_eq!(live.get("4"), Some(&PaneSlot { tab: "corro".into(), index: 0 }));
        assert_eq!(live.get("5"), Some(&PaneSlot { tab: "corro".into(), index: 1 }));
        assert_eq!(live.get("14"), Some(&PaneSlot { tab: "corro".into(), index: 2 }));
        assert_eq!(live.get("13"), Some(&PaneSlot { tab: "corro".into(), index: 3 }));
        assert_eq!(live.get("7"), Some(&PaneSlot { tab: "corro".into(), index: 4 }));

        // Plugins are excluded: the tab-bar/status-bar ids are not present.
        for plugin_id in ["1", "4"] {
            assert!(
                !live.contains_key(plugin_id) || live[plugin_id].tab != "Tab #1",
                "plugin pane {plugin_id} must not be numbered as a terminal pane"
            );
        }
        // Ordinals are per-tab, so they restart.
        assert_eq!(live.get("0"), Some(&PaneSlot { tab: "Tab #1".into(), index: 0 }));
        assert_eq!(live.get("1"), Some(&PaneSlot { tab: "Tab #2".into(), index: 0 }));
    }

    #[test]
    fn real_dump_rewrite_preserves_everything_but_pir_panes() {
        let dir = std::env::temp_dir().join(format!("pir_real_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut map = HashMap::new();
        for i in 0..7u32 {
            let log = dir.join(format!("pir-sess{i}-sh1.jsonl"));
            std::fs::write(&log, "{}\n").unwrap();
            let r = PaneRecord {
                session: "vitreous-hill".into(),
                pane_id: (i + 1).to_string(),
                cwd: "/restored".into(),
                captured: i as u64 + 1,
                // No geometry: this exercises the positional fallback.
                tab: String::new(),
                index: 0,
            };
            std::fs::write(record_path(&log), serde_json::to_string(&r).unwrap()).unwrap();
            map.insert(("vitreous-hill".to_string(), (i + 1).to_string()), log);
        }

        // Drive `match_panes` directly with no live map, so the test exercises
        // the deterministic fallback regardless of whether the test runner
        // happens to be inside a zellij session (where `inject_resume_args`
        // would consult the *real* session and find nothing matching).
        let blocks = find_pane_blocks(REAL);
        let pir_blocks: Vec<&PaneBlock> =
            blocks.iter().filter(|b| b.text.contains("command=\"pir\"")).collect();
        let by_block = match_panes("vitreous-hill", &map, &pir_blocks, None);
        let (out, used) = rewrite_with(REAL, &pir_blocks, &by_block);
        assert_eq!(used, 7, "all 7 pir panes should be restored");

        // Every pane now names its own conversation.
        for i in 0..7u32 {
            let want = format!("args \"-r\" \"pir-sess{i}-sh1\"");
            assert!(out.contains(&want), "missing {want}");
        }
        // Boilerplate is untouched, byte for byte.
        assert_eq!(REAL.matches("swap_tiled_layout").count(), out.matches("swap_tiled_layout").count());
        assert_eq!(REAL.matches("zellij:tab-bar").count(), out.matches("zellij:tab-bar").count());
        assert_eq!(REAL.matches("zellij:status-bar").count(), out.matches("zellij:status-bar").count());
        // Pane headers keep their structure — only the `cwd` value changes (to
        // the recorded one), so size/focus/split attributes must all survive.
        for line in REAL.lines().filter(|l| l.trim_start().starts_with("pane command=\"pir\"")) {
            let attrs: Vec<&str> = ["size=", "focus=", "split_direction="]
                .into_iter()
                .filter(|a| line.contains(a))
                .collect();
            for a in attrs {
                assert!(out.contains(a), "attribute {a} lost from pane header: {line}");
            }
            // The cwd is rewritten to the recorded value...
            assert!(line.contains("cwd=\""), "expected a cwd on: {line}");
        }
        assert_eq!(out.matches("cwd=\"/restored\"").count(), 7, "every pir pane takes the recorded cwd");
        // 5 of the 7 real panes are bare `pir` (no args) and gain an inserted
        // `args` block; the 2 that already had `args "-r"` are edited in place.
        // So the rewrite grows by exactly those 5 insertions.
        assert_eq!(out.lines().count(), REAL.lines().count() + 5);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// End-to-end coverage against a **real** zellij: the unit tests all stop at
/// `inject_resume_args`, so nothing would catch a change in the actual
/// `--new-session-with-layout` invocation, the name-clash handling, or zellij's
/// own willingness to load a rewritten layout.
///
/// Every test here skips itself when `zellij` is absent, so the suite still runs
/// on a host without it.
///
/// They take [`E2E_LOCK`] and run one at a time: zellij has a single per-user
/// server, so concurrent `new-session` calls contend and a session can lose the
/// race and never come up. (Not a fixable flakiness — it is the tool's design.)
#[cfg(test)]
mod e2e_tests {
    use super::*;

    /// Serialises the real-zellij tests against the shared per-user server.
    static E2E_LOCK: Mutex<()> = Mutex::new(());

    /// Whether a real zellij is available to drive. No session of our own is
    /// required: these tests create and kill their own.
    fn zellij_available() -> bool {
        Command::new("zellij")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Run zellij and wait for it, bounded. Returns `None` when it is not
    /// installed, could not be spawned, or did not finish in time — a missing
    /// zellij is the normal case on a host that does not use one, and must not
    /// turn into a suite failure. That is the whole reason these tests self-skip.
    ///
    /// **Everything here is load-bearing, and none of it is defensive.**
    /// zellij 0.45.1 has no `--detach`, and `--new-session-with-layout` *always*
    /// tries to attach. That creates a deadlock specific to this suite:
    ///
    ///   * With **no controlling tty** (how this runs under CI) zellij's client
    ///     panics on `could not enable raw mode`
    ///     (`zellij-client-0.45.1/src/os_input_output.rs:180`) *after* the
    ///     server has already created the session, then exits promptly.
    ///   * **With a tty** — i.e. an operator running `cargo test` from a
    ///     terminal — the client finds a terminal, attaches, and waits for a
    ///     human who will never arrive. The session is up, so the test *looks*
    ///     like it is progressing, but the call never returns.
    ///
    /// The second case is why the suite hung: the plain `Command::output()` here
    /// waits for exit that never came, so each run wedged the test binary and
    /// leaked a zellij server plus its client. Detaching the child with
    /// `setsid()` — the fix below — makes both cases identical.
    ///
    /// Two supporting details, both needed once the wait is bounded:
    ///
    /// 1. **The wait must be on a watchdog, not a poll loop.** `try_wait()` on
    ///    this child can take *seconds* per call (measured: 6 calls in 60 s), so
    ///    a deadline checked between polls overshoots by however long one call
    ///    takes. Hence: park the child on a worker thread and let the *caller*
    ///    time out.
    /// 2. **The pipes must not be drained on timeout.** The layout names `pir`
    ///    panes, and those children inherit the client's stdout/stderr, so the
    ///    write ends stay open even after the client dies. `read_to_end` never
    ///    sees EOF and joining the readers would hang in the same place.
    /// 3. **A timeout must not look like success.** The caller gets a
    ///    non-success status, because `rebuilding_a_running_session…` checks
    ///    the exit code and must not read a timeout as zellij's assent.
    fn run(args: &[&str]) -> Option<std::process::Output> {
        // Hand the unbounded wait to a worker and bound it from *here*. On this
        // child, `wait()` is the only call that terminates, and it can take
        // arbitrarily long (see the doc comment), so the deadline cannot live
        // inside a poll loop — a `try_wait()` round trip here takes seconds.
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut cmd = Command::new("zellij");
            cmd.args(&args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            // **The fix.** Give the child its own session, so it has no
            // controlling terminal. This is the whole bug: run from an
            // interactive shell, zellij's client *finds* a tty, decides it can
            // attach, and blocks in the attach — forever, because nobody is
            // there to detach. With no tty it takes the "no terminal" path
            // (panic on raw mode, session already created by the server) and
            // exits promptly, which is exactly the behaviour these tests need.
            //
            // Verified: the same binary, same layout, hung indefinitely when it
            // inherited `pts/N` and passed in 0.30 s under `setsid`.
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                // SAFETY: setsid() in the child between fork and exec, the
                // documented `CommandExt::pre_exec` use. The closure performs
                // only an async-signal-safe syscall.
                unsafe {
                    cmd.pre_exec(|| {
                        // Declared here rather than pulling in `libc` for one
                        // call: `setsid` is on every Unix this builds for, and
                        // its return value is all we need.
                        unsafe extern "C" {
                            #[link_name = "setsid"]
                            fn raw_setsid() -> i32;
                        }
                        // Failure (already a group leader) is not fatal: the
                        // watchdog still bounds this call.
                        let _ = raw_setsid();
                        Ok(())
                    });
                }
            }
            let Ok(mut child) = cmd.spawn() else { return };
            // Drain both pipes so a client blocked writing escapes rather than
            // wedging on a full buffer. On the success path the client exits and
            // closes them, so these finish and their output is what we send back.
            let out = child.stdout.take().map(|mut o| {
                std::thread::spawn(move || {
                    use std::io::Read;
                    let mut b = Vec::new();
                    let _ = o.read_to_end(&mut b);
                    b
                })
            });
            let err = child.stderr.take().map(|mut e| {
                std::thread::spawn(move || {
                    use std::io::Read;
                    let mut b = Vec::new();
                    let _ = e.read_to_end(&mut b);
                    b
                })
            });
            let Ok(status) = child.wait() else { return };
            // The client exited, so the pipes closed and both readers are
            // already at EOF — joining here cannot hang.
            let stdout = out.and_then(|h| h.join().ok()).unwrap_or_default();
            let stderr = err.and_then(|h| h.join().ok()).unwrap_or_default();
            let _ = tx.send(std::process::Output {
                status,
                stdout,
                stderr,
            });
        });

        match rx.recv_timeout(std::time::Duration::from_secs(ZELLIJ_CALL_TIMEOUT_SECS)) {
            Ok(out) => Some(out),
            Err(_) => {
                // The watchdog fired. Detach the worker: the client, the zellij
                // server and the `pir` panes holding the pipe all die with the
                // test process, and the session itself lives in the server,
                // which the caller's `KillOnDrop` removes.
                drop(worker);
                // A timeout must not read as success: the duplicate-name test
                // checks the exit code, and must not take this for zellij's
                // assent. `ExitStatus` has no public constructor, so borrow a
                // real failing one from `/bin/false`.
                let status = std::process::Command::new("false")
                    .status()
                    .expect("`false` exists on any host that has zellij");
                Some(std::process::Output {
                    status,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }
    }

    /// How long a single zellij call may take before we stop waiting.
    ///
    /// Generous enough for a real attach-less spawn (the server registers the
    /// session in well under a second) and short enough that a wedged client
    /// cannot stall the suite.
    const ZELLIJ_CALL_TIMEOUT_SECS: u64 = 15;

    /// The same bound `rebuild` uses, so this test fails fast against a
    /// regression rather than sitting in its own 15 s watchdog.
    const CREATE_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);

    fn is_running(name: &str) -> bool {
        running_sessions().iter().any(|s| s == name)
    }

    /// 1-minute load average, or 0.0 where it cannot be read.
    fn e2e_load1() -> f64 {
        std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|s| s.split_whitespace().next().map(|v| v.to_string()))
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    }

    /// Multiplier for the e2e "wait for the session to register" budget.
    ///
    /// Every one of these waits is satisfied by a *separate zellij server
    /// process* competing with everything else on the machine, so a flat budget
    /// reports a timing failure as a test failure. Scaling with load keeps the
    /// wait strict on a quiet box and patient on a busy one. Capped at 6x so a
    /// genuinely broken zellij still fails in bounded time.
    fn e2e_wait_factor() -> u64 {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let load1 = e2e_load1();
        if load1 > cores as f64 {
            ((load1 / cores as f64).ceil() as u64).clamp(1, 6)
        } else {
            1
        }
    }

    /// Guard that tears the session (and its stuck client) down on every exit
    /// path — including a panic in the body.
    ///
    /// Killing the *session* is what matters: the detached client exits once
    /// its server is gone (verified on zellij 0.45.1), so a server kill cleans
    /// up both. `delete-session --force` is preferred over `kill-session`
    /// because it removes the session outright rather than leaving a stopped
    /// entry in the operator's list, and it is not itself subject to the
    /// attach-and-hang behaviour.
    struct KillOnDrop<'a>(&'a str);
    impl Drop for KillOnDrop<'_> {
        fn drop(&mut self) {
            let _ = run(&["delete-session", "--force", self.0]);
        }
    }

    /// Create a session from `layout`, run `body`, then always tear it down.
    ///
    /// The exit status of the create call is not evidence either way: zellij
    /// creates the session in its server whether or not the client then manages
    /// to attach. The only reliable evidence that it came up is `is_running`,
    /// which is what the body and the up-wait check. (That same "says nothing
    /// useful" behaviour is the name-clash trap the other test pins.)
    ///
    /// The `KillOnDrop` is installed *before* the create call, so every exit
    /// path — a create that only partially registers, the up-assert below, or a
    /// panic in `body` — tears the session down. Leaking one is not cosmetic:
    /// an orphan is a real zellij *server* that keeps competing on the single
    /// shared per-user server, starving the sessions the remaining e2e tests
    /// are trying to create.
    fn with_session(name: &str, layout: &str, body: impl FnOnce()) {
        // One real-zellij test at a time: the server is per-user and shared.
        let _serial = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Per-*test*, not per-process. The old name was keyed on the pid alone,
        // so a re-run of the binary (or a second binary) reopened the same
        // directory and a leftover `.kdl`/socket from an interrupted run could
        // be handed to a fresh `--new-session-with-layout`.
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("pir_e2e_{}_{}", std::process::id(), seq));
        let _ = std::fs::create_dir_all(&dir);
        let kdl = dir.join(format!("{name}.kdl"));
        std::fs::write(&kdl, layout).unwrap();
        // Install the teardown guard *before* the create call, so a session that
        // comes up and then wedges — or a create that partially succeeds — is
        // still torn down. Previously the guard was installed after the
        // up-wait, so a session that registered between the create returning
        // and the guard being built survived the run: those orphans are real
        // zellij *servers* that keep competing on the one shared per-user
        // server, which is what makes the next `with_session` time out.
        let _kill = KillOnDrop(name);
        let _ = run(&[
            "--new-session-with-layout",
            kdl.to_str().unwrap(),
            "--session",
            name,
        ]);
        // Give the server a moment to register the session. The budget scales
        // with load (see `e2e_wait_factor`): registration is done by a server
        // process sharing this box, so a flat 5s failed regularly at load
        // average 30+ on 16 cores — a timing failure that says nothing about
        // what these tests assert.
        let tries = 50 * e2e_wait_factor();
        let mut up = false;
        for _ in 0..tries {
            if is_running(name) {
                up = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // `_kill` was installed before the create, so this assert and any panic
        // in `body` still tear the session down.
        assert!(
            up,
            "zellij session {name} never came up (waited {}ms, load {:.1} on {} cores)",
            tries * 100,
            e2e_load1(),
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
        );
        body();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A layout zellij must actually accept, with two `pir` panes in one tab.
    const TWO_PANE_LAYOUT: &str = r#"layout {
    cwd "/tmp"
    tab name="e2e" focus=true {
        pane size=1 borderless=true {
            plugin location="zellij:tab-bar"
        }
        pane split_direction="vertical" {
            pane command="pir" cwd="/a" {
                args "-r"
                start_suspended true
            }
            pane command="pir" cwd="/b" {
                start_suspended true
            }
        }
        pane size=1 borderless=true {
            plugin location="zellij:status-bar"
        }
    }
}
"#;

    /// The rewritten layout is not just syntactically plausible — zellij loads
    /// it and comes up with the expected number of panes. This is the check the
    /// pure-KDL tests cannot make.
    #[test]
    fn zellij_accepts_a_rewritten_layout() {
        if !zellij_available() {
            eprintln!("skipping: no zellij on this host");
            return;
        }
        let name = format!("pir-e2e-load-{}", std::process::id());
        with_session(&name, TWO_PANE_LAYOUT, || {
            // It is running: the session exists.
            assert!(is_running(&name), "session {name} should be running");
            // Poll the dump until the panes materialise. The session is
            // registered before its panes have spawned, so a single immediate
            // dump can legitimately come back with an empty tab — that is a
            // readiness race, not a layout problem, and it is exactly the kind of
            // thing a fixed sleep would paper over.
            let mut panes = 0usize;
            let mut last = String::new();
            for _ in 0..50 {
                let out = run(&["--session", &name, "action", "dump-layout"])
                    .expect("zellij_available() was checked before this ran");
                assert!(out.status.success(), "dump-layout failed");
                last = String::from_utf8_lossy(&out.stdout).to_string();
                panes = last.matches("command=\"pir\"").count();
                if panes > 0 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            assert_eq!(
                panes, 2,
                "both pir panes must survive the round trip, last dump:\n{last}"
            );
        });
    }

    /// The name-clash path: rebuilding a session that is *already running* must
    /// not silently report success. zellij prints "already exists" and exits 0,
    /// so this is exactly the case a naive status check gets wrong.
    #[test]
    fn rebuilding_a_running_session_does_not_report_a_false_success() {
        if !zellij_available() {
            eprintln!("skipping: no zellij on this host");
            return;
        }
        let name = format!("pir-e2e-clash-{}", std::process::id());
        with_session(&name, TWO_PANE_LAYOUT, || {
            assert!(is_running(&name));
            // `free_session_name` must hand back something different, because
            // zellij will refuse the duplicate.
            let free = free_session_name(&name);
            assert_ne!(free, name, "must not reuse a running session's name");
            assert!(!is_running(&free), "{free} should be free");
            // And the refusal really is visible to us: zellij prints
            // "already exists" on stdout while exiting 0.
            let dir = std::env::temp_dir().join(format!("pir_e2e_clash_{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            let kdl = dir.join("clash.kdl");
            std::fs::write(&kdl, TWO_PANE_LAYOUT).unwrap();
            let out = run(&[
                "--new-session-with-layout",
                kdl.to_str().unwrap(),
                "--session",
                &name,
            ])
            .expect("zellij_available() was checked before this ran");
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                refusal_line(combined.as_bytes()).is_some(),
                "zellij must visibly refuse a duplicate name so we can detect it; got: {combined:?}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// `running_sessions` underpins the clash check; it must actually list live
    /// sessions, and must not mistake a non-running name for one.
    #[test]
    fn running_sessions_lists_only_live_sessions() {
        if !zellij_available() {
            eprintln!("skipping: no zellij on this host");
            return;
        }
        let name = format!("pir-e2e-list-{}", std::process::id());
        assert!(!is_running(&name), "must not exist before we create it");
        with_session(&name, TWO_PANE_LAYOUT, || {
            let list = running_sessions();
            assert!(list.contains(&name), "created session missing from list: {list:?}");
            assert!(
                !list.iter().any(|s| s == "pir-e2e-definitely-not-here"),
                "phantom session listed"
            );
        });
        // Killed, so it must be gone.
        assert!(!is_running(&name), "session must not outlive the test");
    }

    /// The regression that made this suite unrunnable: `run()` used a bare
    /// `Command::output()`, and from a terminal zellij's client attaches to the
    /// inherited tty and waits for a human. `output()` never returns, so the
    /// test binary wedged and leaked a zellij server on every run.
    ///
    /// What this pins is the property that actually makes it safe: **creating a
    /// session returns in bounded time, and leaves nothing behind** — verified
    /// with a real controlling tty (`script(1)`) present, which is the only
    /// condition under which the old code hung. It also asserts the teardown
    /// path, since a helper that returns but fails to clean up is just as bad.
    #[test]
    fn creating_a_session_is_bounded_and_leaves_nothing_behind() {
        if !zellij_available() {
            eprintln!("skipping: no zellij on this host");
            return;
        }
        let _serial = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let name = format!("pir-e2e-bounded-{}", std::process::id());
        let dir = std::env::temp_dir().join(format!("pir_e2e_bounded_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let kdl = dir.join("bounded.kdl");
        std::fs::write(&kdl, TWO_PANE_LAYOUT).unwrap();
        let path = kdl.to_str().unwrap().to_string();
        assert!(!is_running(&name), "must not exist before we create it");
        // Teardown guard *before* the create. This test previously had none: it
        // only deleted the session at the very end, so the two asserts in
        // between could — and under load did — leak a real zellij *server*
        // that outlived the run and starved every later e2e test. The comment
        // at the bottom calls that leak "what accumulated across the original
        // broken runs"; the guard is what stops it recurring.
        let _kill = KillOnDrop(&name);

        let started = std::time::Instant::now();
        let _out = run(&["--new-session-with-layout", &path, "--session", &name]);
        let elapsed = started.elapsed();

        // The session really was created (the call did not simply bail out)...
        // Budget scales with load for the same reason as `with_session`: the
        // registering is done by a server process sharing this box.
        let tries = 50 * e2e_wait_factor();
        let mut up = false;
        for _ in 0..tries {
            if is_running(&name) {
                up = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(
            up,
            "the bounded call must still create the session (waited {}ms, load {:.1} on {} cores)",
            tries * 100,
            e2e_load1(),
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
        );

        // ...and the call returned on its own rather than riding the watchdog
        // to the deadline. Generous: the watchdog is 15 s, and a wedged attach
        // would be indistinguishable from "slower than that" otherwise.
        assert!(
            elapsed < std::time::Duration::from_secs(ZELLIJ_CALL_TIMEOUT_SECS),
            "creating a session took {elapsed:?}; it must not wait for an attach"
        );

        // Teardown must actually remove it — a leaked server outlives the run
        // and is what accumulated across the original broken runs.
        let _ = run(&["delete-session", "--force", &name]);
        let mut gone = false;
        for _ in 0..tries {
            if !is_running(&name) {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(gone, "session {name} outlived its teardown");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The regression for the `pir -r @` hang.**
    ///
    /// `pir -r @` used to call `rebuild()`, which ran
    /// `Command::output()` on `zellij --new-session-with-layout`. That invocation
    /// always attaches, so with the operator's terminal inherited as a
    /// controlling tty the client sat in the attach waiting for a human, and
    /// `output()` blocked forever. Reproduced on this host: the stuck `pir` was
    /// found under gdb in `poll()` inside `Command::output()`, with its child
    /// zellij client holding a socket to the new session.
    ///
    /// The fix is `create_session_detached`: `setsid()` + null stdio, bounded
    /// wait, success judged by probing the session. So this drives the **real
    /// production create path** — not a test helper that merely looks like it —
    /// under `script(1)`, which is the one condition that ever triggered the
    /// bug, and asserts the two properties that make it safe:
    ///
    /// 1. it returns in bounded time (no attach wait), and
    /// 2. the session it creates is genuinely there.
    ///
    /// It must not be a "does not hang" test alone: the tempting wrong fix is to
    /// kill the client, which would make it return instantly and leave no
    /// session at all. Hence (2).
    ///
    /// ### How a child gets a real tty
    /// `setsid` is what we are testing, so the test cannot set up its own
    /// conditions — the child has to inherit an operator's terminal. It does:
    /// `script(1)` allocates a pty, so inside that pty the child has a
    /// controlling terminal, exactly as a real `pir -r @` run does.
    #[test]
    fn rebuild_returns_with_a_real_session_even_with_a_controlling_tty() {
        // The child re-enters this same test with `PROBE` set; when it is set
        // this test *is* the probe, so it does the create and returns. The
        // handshake is through the environment because the test harness owns
        // argv and rejects unknown flags.
        if let Some(name) = std::env::var_os(PROBE_ENV) {
            let name = name.to_string_lossy().to_string();
            let layout = PathBuf::from(std::env::var_os(PROBE_LAYOUT_ENV).expect("probe layout"));
            let started = std::time::Instant::now();
            let result = create_session_detached(&layout, &name);
            println!(
                "{PROBE_MARKER} elapsed_ms={} ok={}",
                started.elapsed().as_millis(),
                result.is_ok()
            );
            return;
        }
        if !zellij_available() || which("script").is_none() {
            eprintln!("skipping: no zellij, or no script(1) for a real tty");
            return;
        }
        let _serial = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let name = format!("pir-e2e-tty-{}", std::process::id());
        let dir = std::env::temp_dir().join(format!("pir_e2e_tty_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let kdl = dir.join("tty.kdl");
        std::fs::write(&kdl, ONE_PANE_LAYOUT).unwrap();
        let _kill = KillOnDrop(&name);
        // Reap any corpse from an interrupted previous run, so this run's
        // name-collision handling starts from a known state.
        let _ = run(&["delete-session", "--force", &name]);

        // A layout zellij must actually accept, with one terminal pane.
        // zellij's `command="…"` is a whole argv, so the spaces in `sleep 30`
        // make it fail fast (exit 2) — the point of this pane is not to run, it
        // is to prove the *session* outlives the client that created it.
        let child = Command::new("script")
            .args(["-qec", &self_test_command(), &dir.join("child.log").to_string_lossy()])
            .env(PROBE_ENV, &name)
            .env(PROBE_LAYOUT_ENV, &kdl)
            .output()
            .expect("script(1) runs");
        let child_out = format!(
            "{}{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );

        // (1) Bounded. The old code never returned at all, so any return is
        // already progress; the bound is what separates "fixed" from "slow".
        // The child is this same test binary, so it prints its own measurement; read
        // that rather than guessing from wall time (which also covers harness
        // startup). Plain integer milliseconds — rust's `Duration` debug format
        // switches units mid-string (`161.79759ms`), which is not worth parsing.
        let wall_ms = child_out
            .lines()
            .find_map(|l| l.trim().strip_prefix(PROBE_MARKER))
            .and_then(|rest| rest.split("elapsed_ms=").nth(1))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|d| d.parse::<u64>().ok());
        assert!(
            child.status.code().is_some(),
            "the child must terminate at all — this is the hang being tested. Output:\n{child_out}"
        );
        assert!(
            wall_ms.is_some_and(|ms| ms < CREATE_BUDGET.as_millis() as u64),
            "the create itself took {wall_ms:?}ms with a real tty; it must not wait for \
             an attach. Output:\n{child_out}"
        );

        // (2) The session is real, and its panes came up. This is what a
        // kill-the-client "fix" would fail: no client, and killing the client is
        // the one thing we must not do.
        let tries = 50 * e2e_wait_factor();
        let mut up = false;
        for _ in 0..tries {
            if is_running(&name) {
                up = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(up, "rebuild must leave a live session {name}; output:\n{child_out}");
        assert!(
            probe_panes(&name).is_some(),
            "the reported session must answer a pane probe"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The command that re-runs *this test alone* in a child, so the probe
    /// branch above is what executes. `current_exe` is this test binary; the
    /// filter names this one test.
    fn self_test_command() -> String {
        let exe = std::env::current_exe().expect("test binary path");
        format!(
            "{} --exact --nocapture zellij::e2e_tests::rebuild_returns_with_a_real_session_even_with_a_controlling_tty",
            exe.display()
        )
    }

    /// Env set on the child to turn this test into the create probe.
    const PROBE_ENV: &str = "PIR_ZELLIJ_REBUILD_PROBE";
    /// Layout the probe creates a session from.
    const PROBE_LAYOUT_ENV: &str = "PIR_ZELLIJ_REBUILD_LAYOUT";
    /// Marker the probe prints so the parent can find its timing line.
    const PROBE_MARKER: &str = "pir-zellij-probe:";

    /// A layout zellij must actually accept, with one terminal pane.
    const ONE_PANE_LAYOUT: &str = r#"layout {
    cwd "/tmp"
    tab name="t" focus=true {
        pane size=1 borderless=true {
            plugin location="zellij:tab-bar"
        }
        pane command="sleep 30" focus=true {
        }
        pane size=1 borderless=true {
            plugin location="zellij:status-bar"
        }
    }
    new_tab_template {
        pane size=1 borderless=true {
            plugin location="zellij:tab-bar"
        }
        pane
        pane size=1 borderless=true {
            plugin location="zellij:status-bar"
        }
    }
}
"#;

    /// `which`-equivalent for a tool the test needs, without a dependency.
    fn which(tool: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|d| d.join(tool))
            .find(|c| c.is_file())
    }
}
