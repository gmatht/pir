//! Notification architecture (the "agent is done" feature).
//!
//! Model: the agent emits [`AgentEvent`]s at well-defined boundaries (end of a
//! turn, an error, idle at the REPL prompt). Every agent — the foreground
//! session and all background sessions — publishes its events to one shared
//! [`NotifyBus`]. The bus is the single sink for the whole `pir` process: it
//! fires the configured external notifiers (bell / desktop / file / sound /
//! webhook, gated by [`NotifyPolicy`]) **and** keeps a bounded ring buffer of
//! recent events that the active REPL screen drains between prompts, so the
//! user sees notifications from *all* agents on the one screen they're watching.
//!
//! This is intentionally the same "one trait, many backends" shape as
//! `plugin::Registry`: a [`Notifier`] is just a delivery channel. New channels
//! (Slack, Telegram, …) can later be added as drop-in extensions by extending
//! `build.rs` to emit `register_notifiers`, exactly like tools.

use crate::config;
use crate::term;
#[allow(dead_code)]
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// Discriminator for the kind of event, kept separate from the (shared)
/// descriptive fields so events can carry notification context (project /
/// prompt) without exploding every variant.
// `Hash` is needed by the notification coalescer, which keys a pending map by
// kind so a burst of same-kind events collapses to one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventKind {
    TurnDone,
    Error,
    Idle,
}

#[derive(Clone, Debug)]
pub struct AgentEvent {
    pub kind: EventKind,
    pub duration: Duration,
    pub in_tokens: u64,
    pub out_tokens: u64,
    pub message: String,
    /// Short project/cwd label (e.g. "rpi"), shown in notification titles so a
    /// pop-up identifies *which* project finished rather than just "pir".
    pub project: String,
    /// The last user prompt that produced this turn (truncated; may be empty),
    /// shown in the notification body so the user knows *what* finished.
    pub prompt: String,
}

impl AgentEvent {
    pub fn turn_done(
        duration: Duration,
        in_tokens: u64,
        out_tokens: u64,
        project: String,
        prompt: String,
    ) -> Self {
        AgentEvent {
            kind: EventKind::TurnDone,
            duration,
            in_tokens,
            out_tokens,
            message: String::new(),
            project,
            prompt,
        }
    }

    pub fn error(message: String, project: String, prompt: String) -> Self {
        AgentEvent {
            kind: EventKind::Error,
            duration: Duration::ZERO,
            in_tokens: 0,
            out_tokens: 0,
            message,
            project,
            prompt,
        }
    }

    pub fn idle(project: String, prompt: String) -> Self {
        AgentEvent {
            kind: EventKind::Idle,
            duration: Duration::ZERO,
            in_tokens: 0,
            out_tokens: 0,
            message: String::new(),
            project,
            prompt,
        }
    }

    /// Short human-readable line, used by most notifiers and the on-screen feed.
    pub fn summary(&self) -> String {
        match self.kind {
            EventKind::TurnDone => format!(
                "turn done in {:.1}s ({} in / {} out tokens)",
                self.duration.as_secs_f64(),
                self.in_tokens,
                self.out_tokens
            ),
            EventKind::Error => format!("error: {}", self.message),
            EventKind::Idle => "idle".into(),
        }
    }

    /// Stable machine name, used in file/JSON/webhook payloads.
    pub fn kind(&self) -> &'static str {
        match self.kind {
            EventKind::TurnDone => "turn-done",
            EventKind::Error => "error",
            EventKind::Idle => "idle",
        }
    }

    /// Title for desktop pop-ups, e.g. "pir · rpi". Falls back to "pir" when the
    /// project label is unavailable.
    pub fn desktop_title(&self) -> String {
        if self.project.is_empty() {
            "pir".to_string()
        } else {
            format!("pir · {}", self.project)
        }
    }

    /// The window/tab title: identity only — `pir · <dir>`.
    ///
    /// Deliberately *not* the event summary. The title sits in a fixed-width
    /// terminal tab (and zellij truncates it), so anything chatty is noise: it
    /// was writing lines like `pir · corro: turn done in 12.3s (1234 in / 567
    /// out tokens)` into the tab strip. Duration and token counts belong in the
    /// on-screen feed and the desktop pop-up, which have room for them.
    ///
    /// A long project path is shortened to its last two segments (`a/b/c` →
    /// `b/c`) so the title stays scannable when several pir panes are open.
    pub fn terminal_title(&self) -> String {
        if self.project.is_empty() {
            return "pir".to_string();
        }
        let project = self.project.trim();
        if project.is_empty() {
            return "pir".to_string();
        }
        let segs: Vec<&str> = project.split('/').filter(|s| !s.is_empty()).collect();
        if segs.is_empty() {
            return "pir".to_string();
        }
        // Drop a leading "/" rather than emitting a bare root path, and keep only
        // the last two segments so a deep path stays scannable next to other tabs.
        let short = if segs.len() > 2 {
            segs[segs.len() - 2..].join("/")
        } else {
            segs.join("/")
        };
        format!("pir · {short}")
    }

    /// Body for desktop pop-ups: the summary, plus the last prompt when we have
    /// one, so the user knows which task finished.
    pub fn desktop_body(&self) -> String {
        let summary = self.summary();
        if self.prompt.is_empty() {
            summary
        } else {
            format!("{}\n{}", summary, self.prompt)
        }
    }
}

/// Clip a string to `n` chars, appending an ellipsis if truncated.
///
/// A **multiline** prompt is first collapsed to the one-line paste placeholder
/// (see [`crate::term::collapse_for_display`]) — a pasted block used to be echoed
/// verbatim here, so an 80-char, 12-line paste printed 12 lines into the notify
/// line and wrecked the feed. Collapsing happens before the length clip, so the
/// count and the ellipsis describe the placeholder the user will actually see.
fn clip(s: &str, n: usize) -> String {
    let collapsed = crate::term::collapse_for_display(s, None);
    let s = collapsed.as_deref().unwrap_or(s).trim();
    // A collapsed placeholder is emitted whole even if it exceeds `n`: truncating
    // it would cut the `N lines pasted ]` suffix that carries the count, and it is
    // already short by construction.
    if collapsed.is_some() {
        return s.to_string();
    }
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// A delivery channel. Implemented by each built-in notifier and (future)
/// by extension-provided notifiers. `Send + Sync` so a bus can be shared
/// (behind `Arc`) across the foreground REPL and background agent threads.
pub trait Notifier: Send + Sync {
    /// Human-readable channel name (used in diagnostics).
    #[allow(dead_code)]
    fn name(&self) -> &'static str;
    fn notify(&self, event: &AgentEvent);
}

/// Fan-out registry of notifiers.
pub struct NotifierHub {
    items: Vec<Box<dyn Notifier + Send + Sync>>,
}

impl NotifierHub {
    pub fn new() -> Self {
        NotifierHub { items: Vec::new() }
    }
    pub fn add(&mut self, n: Box<dyn Notifier + Send + Sync>) {
        self.items.push(n);
    }
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    pub fn fire(&self, event: &AgentEvent) {
        for n in &self.items {
            n.notify(event);
        }
    }

    /// Build a hub from a policy: instantiate each enabled method. Channels not
    /// listed in `policy.methods` are skipped. Safe to call with an empty
    /// method list (the hub simply fires nothing).
    pub fn from_policy(policy: &NotifyPolicy) -> Self {
        let mut hub = NotifierHub::new();
        for m in &policy.methods {
            match m.as_str() {
                "bell" => hub.add(Box::new(Bell)),
                "desktop" => hub.add(Box::new(Desktop)),
                "sound" => hub.add(Box::new(Sound)),
                "file" => hub.add(Box::new(FileStamp)),
                "webhook" => {
                    if let Some(url) = &policy.webhook {
                        hub.add(Box::new(Webhook::new(url.clone())))
                    }
                }
                other => eprintln!("pir: unknown notify method '{other}'"),
            }
        }
        hub
    }
}

impl Default for NotifierHub {
    fn default() -> Self {
        Self::new()
    }
}

/// Start the process-wide notification coalescer, driving a hub built from
/// `policy`. Called once at startup.
///
/// This is what makes a burst of agent turns cost one notifier spawn instead
/// of one per turn, and it is the only path that spawns a notifier child, so
/// it is also where children get reaped (see
/// [`crate::notify_coalesce::spawn_and_reap`]). Safe to call more than once:
/// the first call wins and later calls are ignored.
pub fn install_coalescer(policy: &NotifyPolicy) {
    let hub = Arc::new(NotifierHub::from_policy(policy));
    crate::notify_coalesce::install(Arc::new(move |e: &AgentEvent| hub.fire(e)));
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct NotifyPolicy {
    /// Channel names to enable: "bell", "desktop", "file", "sound", "webhook".
    pub methods: Vec<String>,
    /// Which events to consider: "turn-done" | "error" | "idle" | "all".
    pub on: String,
    /// Skip turn-done notifications shorter than this many seconds.
    pub min_seconds: u64,
    /// "always" | "background" | "oneshot".
    pub when: String,
    /// Target URL when "webhook" is enabled.
    pub webhook: Option<String>,
    /// Whether the "sound" channel is enabled.
    pub sound: bool,
}

impl Default for NotifyPolicy {
    fn default() -> Self {
        NotifyPolicy {
            methods: vec!["bell".into(), "desktop".into()],
            on: "all".into(),
            min_seconds: 8,
            when: "background".into(),
            webhook: None,
            sound: false,
        }
    }
}

impl NotifyPolicy {
    pub fn from_json(v: &Value) -> Self {
        let mut p = NotifyPolicy::default();
        if let Some(m) = v.get("methods").and_then(|m| m.as_array()) {
            p.methods = m.iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
        }
        if let Some(s) = v.get("on").and_then(|x| x.as_str()) {
            p.on = s.to_string();
        }
        if let Some(n) = v.get("min_seconds").and_then(|x| x.as_u64()) {
            p.min_seconds = n;
        }
        if let Some(s) = v.get("when").and_then(|x| x.as_str()) {
            p.when = s.to_string();
        }
        if let Some(s) = v.get("webhook").and_then(|x| x.as_str()) {
            p.webhook = Some(s.to_string());
        }
        if let Some(b) = v.get("sound").and_then(|x| x.as_bool()) {
            p.sound = b;
        }
        p
    }

    /// True if this event should actually be delivered under this policy.
    /// `oneshot` marks events emitted as a process is about to exit (one-shot
    /// / background completion); `for_screen` marks events destined for the
    /// active REPL screen feed (always shown there, independent of `when`).
    pub fn allows(&self, e: &AgentEvent, oneshot: bool, for_screen: bool) -> bool {
        if for_screen {
            // The on-screen feed always shows everything the user asked for
            // (idle is suppressed — it's just "back at the prompt").
            return !matches!(e.kind, EventKind::Idle);
        }
        // Event-type gate.
        let ok_type = match self.on.as_str() {
            "turn-done" => matches!(e.kind, EventKind::TurnDone),
            "error" => matches!(e.kind, EventKind::Error),
            "idle" => matches!(e.kind, EventKind::Idle),
            _ => true, // "all" and anything unknown
        };
        if !ok_type {
            return false;
        }

        // Timing gate for turn-done.
        if let EventKind::TurnDone = e.kind
            && e.duration.as_secs() < self.min_seconds {
                return false;
            }

        // Delivery-context gate.
        match self.when.as_str() {
            "oneshot" => {
                if !oneshot {
                    return false;
                }
            }
            "background" => {
                // In an interactive REPL the user is watching; only ping if the
                // turn took a while or output is not attached to a terminal
                // (e.g. piped / backgrounded).
                let long = matches!(e.kind, EventKind::TurnDone
                    if e.duration.as_secs() >= self.min_seconds);
                if !oneshot && term::is_terminal() && !long {
                    return false;
                }
            }
            _ => {}
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Shared bus (single sink for every agent in the process)
// ---------------------------------------------------------------------------

/// Process-wide notification bus. Every agent publishes events here; the bus
/// fans them out to the configured external notifiers and records them in a
/// bounded ring buffer that the active REPL drains to its screen, so the user
/// sees notifications from *all* sessions — foreground and background — on the
/// one screen they're watching.
pub struct NotifyBus {
    /// Policy + its wired-up external notifiers, behind an `RwLock` so
    /// `/reload` can swap in a freshly loaded settings.json `notify` block
    /// without restarting pir. The lock is only ever *contended* in principle:
    /// publishers take a read lock (uncontended, cheap) and only `/reload` —
    /// dispatched from the idle prompt — takes the write lock.
    config: RwLock<(NotifyPolicy, NotifierHub)>,
    /// Bounded recent-event log for on-screen rendering. Guarded so background
    /// agents can append concurrently with the REPL draining it.
    feed: Mutex<Vec<AgentEvent>>,
    max_feed: usize,
}

impl NotifyBus {
    /// Build a bus from a policy, wiring up the enabled external notifiers.
    pub fn new(policy: NotifyPolicy) -> Self {
        let external = NotifierHub::from_policy(&policy);
        NotifyBus {
            config: RwLock::new((policy, external)),
            feed: Mutex::new(Vec::new()),
            max_feed: 64,
        }
    }

    /// Load the policy from settings and build a bus.
    pub fn from_settings() -> Self {
        NotifyBus::new(config::load_notify_policy())
    }

    /// Swap in a freshly loaded policy (`/reload`), rebuilding the external
    /// notifier hub so a changed `notify` block in settings.json takes effect
    /// without restarting pir. The on-screen feed is *kept* — reloading config
    /// must not discard notifications the user hasn't seen yet.
    pub fn reload_policy(&self, policy: NotifyPolicy) {
        let external = NotifierHub::from_policy(&policy);
        match self.config.write() {
            Ok(mut g) => *g = (policy, external),
            // A poisoned lock means a publisher panicked mid-fire; the policy
            // is still structurally sound, so recover rather than dropping the
            // user's `/reload` on the floor.
            Err(e) => *e.into_inner() = (policy, external),
        }
    }

    /// Publish an event. Fires external notifiers (gated by policy) and appends
    /// to the on-screen feed (gated by policy for screen). `oneshot` marks an
    /// exit-time event (one-shot / background completion).
    ///
    /// External delivery goes through the coalescing thread
    /// ([`crate::notify_coalesce`]) rather than firing inline, so a burst of
    /// turns from a long-running agent loop collapses into one notifier spawn
    /// instead of one per event. The on-screen feed is NOT coalesced: it is a
    /// cheap in-process ring buffer and the user may want to see each event.
    pub fn publish(&self, event: AgentEvent, oneshot: bool) {
        // One read lock for both gates + the fire, so a concurrent `/reload`
        // can never interleave a policy swap halfway through publishing.
        let g = match self.config.read() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        let (policy, external) = &*g;
        if policy.allows(&event, oneshot, false) {
            // Hand the event to the coalescing thread when one is installed:
            // it collapses a burst of turns into a single notifier spawn. The
            // thread is configured at startup with its own hub, so nothing
            // borrowed from this guard crosses the thread boundary.
            //
            // If no coalescer is installed (tests, embedded uses), deliver
            // inline exactly as before — the feature is additive, never a
            // reason to drop a notification.
            if crate::notify_coalesce::is_installed() {
                crate::notify_coalesce::queue(event.clone());
            } else {
                external.fire(&event);
            }
        }
        if policy.allows(&event, oneshot, true) {
            drop(g);
            if let Ok(mut feed) = self.feed.lock() {
                feed.push(event);
                if feed.len() > self.max_feed {
                    let drop = feed.len() - self.max_feed;
                    feed.drain(0..drop);
                }
            }
        }
    }

    /// Drain pending on-screen notifications, returning them for the caller to
    /// render above the prompt. The feed is cleared. Safe to call from the REPL
    /// loop between prompts; background agents keep appending concurrently.
    pub fn drain_feed(&self) -> Vec<AgentEvent> {
        match self.feed.lock() {
            Ok(mut feed) => std::mem::take(&mut *feed),
            Err(_) => Vec::new(),
        }
    }

    /// The current policy, cloned. Used by the REPL to show/report the active
    /// notification settings (and asserted by the `/reload` tests).
    pub fn policy(&self) -> NotifyPolicy {
        match self.config.read() {
            Ok(g) => g.0.clone(),
            Err(e) => e.into_inner().0.clone(),
        }
    }

    /// Announce that pir has stopped and is waiting for the operator.
    ///
    /// Rings the terminal bell on *every* turn boundary, deliberately
    /// bypassing the [`NotifyPolicy`] gates: `min_seconds` / `when` / `on` all
    /// exist to keep a *long-running agent loop* (or a background worker) from
    /// firing a notifier on every short turn, and applying them here is exactly
    /// what would silence the bell an interactive operator is waiting for. The
    /// bell is the "your turn" signal, not a notification *about* a turn, so it
    /// is unconditional; only the "no terminal attached" case suppresses it
    /// (handled inside [`ring`]).
    ///
    /// Ring the bell here rather than from the worker thread so it fires at the
    /// true prompt boundary — after any follow-up/continuation turn has also
    /// finished — instead of once per internal turn.
    pub fn waiting_for_input(&self) {
        ring();
    }
}

/// Cheap, clonable handle to the shared bus, passed to every agent (foreground
/// and background) so they all publish to the same sink.
pub type SharedBus = Arc<NotifyBus>;

/// Convenience: build the shared bus behind an `Arc` for the whole process.
///
/// Also installs the process-wide notification coalescer on the first call, so
/// a burst of turns from a long-running agent loop costs one notifier spawn
/// rather than one per turn. The bus still delivers inline when no coalescer
/// is installed, so this is purely an optimisation at startup.
pub fn shared_bus() -> SharedBus {
    let bus = Arc::new(NotifyBus::from_settings());
    install_coalescer(&bus.policy());
    bus
}

// ---------------------------------------------------------------------------
// Built-in notifiers
// ---------------------------------------------------------------------------

/// Terminal bell + title flash. Suppressed when not attached to a terminal.
pub struct Bell;
impl Notifier for Bell {
    fn name(&self) -> &'static str {
        "bell"
    }
    fn notify(&self, e: &AgentEvent) {
        if !term::is_terminal() {
            return;
        }
        let _ = std::io::stderr().write_all(b"\x07");
        let _ = std::io::stderr()
            .write_all(format!("\x1b]0;{}\x07", e.terminal_title()).as_bytes());
        let _ = std::io::stderr().flush();
    }
}

/// Ring the terminal bell (BEL, `\x07`) — the "pir has stopped and is waiting
/// for you" signal. Suppressed when stdout isn't a terminal (a piped/headless
/// run has nobody to hear it).
///
/// This is deliberately a bare write rather than a `Notifier` on the bus: the
/// turn-boundary bell must not be gated by `min_seconds` / `when` (those exist
/// to keep *long-running agent loops* from pinging on every short turn), so it
/// does not go through [`NotifyPolicy::allows`]. See
/// [`NotifyBus::waiting_for_input`].
pub fn ring() {
    if !term::is_terminal() {
        return;
    }
    let _ = std::io::stderr().write_all(b"\x07");
    let _ = std::io::stderr().flush();
}

/// Desktop pop-up via the platform notifier. Best-effort; silently skips
/// unsupported platforms.
///
/// The notifier subprocess runs with fully detached stdio (redirected to
/// `/dev/null`), so a missing backend or an absent bus (the common case on a
/// headless server or inside WSL2 without a bridge) can never leak error text
/// onto the user's terminal — that was the source of the spurious
/// "Could not connect: No such file or directory" lines.
///
/// Reaping: every child spawned here is waited on via
/// [`crate::notify_coalesce::spawn_and_reap`], so a pir that notifies
/// repeatedly cannot accumulate zombies. Combined with the coalescing thread
/// (one notifier per burst rather than one per event), this is what keeps a
/// long-running agent loop from churning a process per turn.
pub struct Desktop;
impl Notifier for Desktop {
    fn name(&self) -> &'static str {
        "desktop"
    }
    fn notify(&self, e: &AgentEvent) {
        let title = e.desktop_title();
        let body = e.desktop_body();

        // On macOS, use the native AppleScript notification.
        #[cfg(target_os = "macos")]
        {
            let mut c = Command::new("osascript");
            c.args(["-e", &format!("display notification {body:?} with title {title:?}")])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            crate::notify_coalesce::spawn_and_reap(&mut c);
            return;
        }

        // On Linux (incl. WSL2), there is no desktop notification if there's no
        // usable D-Bus session bus (headless / SSH / container). Probe it once
        // and skip the spawn entirely so we neither error nor spam the
        // terminal. Under WSL2 we additionally try to reach the *Windows*
        // notification center, since the Linux D-Bus bus is not bridged to it.
        #[cfg(target_os = "linux")]
        {
            if running_under_wsl()
                && notify_via_windows(&title, &body) {
                    return;
                }
                // Only fall through to notify-send if a Linux bus actually
                // exists; otherwise we'd spawn a subprocess that prints
                // "Could not connect: No such file or directory" onto the
                // terminal (the source of the spurious leak).
            if !has_session_bus() {
                return;
            }
            let mut c = Command::new("notify-send");
            c.args([&title, &body])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            crate::notify_coalesce::spawn_and_reap(&mut c);
        }
    }
}

/// True when running inside Windows Subsystem for Linux (WSL1 or WSL2), where
/// the host is Windows and `/proc/version` mentions Microsoft. Cheap and
/// best-effort (returns false if it can't be determined).
#[cfg(target_os = "linux")]
fn running_under_wsl() -> bool {
    if let Ok(v) = std::fs::read_to_string("/proc/version") {
        let l = v.to_ascii_lowercase();
        return l.contains("microsoft") || l.contains("wsl");
    }
    false
}

/// Try to raise a Windows toast from inside WSL2, either via the `wsl-notify-
/// send` libnotify bridge (if installed) or via `powershell.exe` + BurntToast.
/// Returns true if we launched a backend (best-effort; we don't verify success
/// — all subprocess stdio is detached so any failure is silent). Returns false
/// if no Windows-side backend is reachable.
#[cfg(target_os = "linux")]
fn notify_via_windows(title: &str, body: &str) -> bool {
    // Preferred: wsl-notify-send — a drop-in libnotify that forwards to the
    // Windows Action Center. If present (and it actually succeeds), use it.
    if Command::new("wsl-notify-send")
        .args([title, body])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return true;
    }
    // Fallback: drive the Windows-side PowerShell host. BurntToast must be
    // installed once on Windows (`Install-Module BurntToast -Force`). Escape
    // double quotes / backticks so the message can't break out of the string.
    // We check the *exit status* (not just spawn success): a missing BurntToast
    // exits non-zero, and we must then fall through to notify-send rather than
    // return `true` and silently skip the Linux fallback.
    let safe = body.replace('`', "``").replace('"', "`\"");
    let script = format!(
        "Import-Module BurntToast -ErrorAction SilentlyContinue; \
         New-BurntToastNotification -AppLogo None -Text '{title}',\"{safe}\" \
         -ErrorAction SilentlyContinue"
    );
    if Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return true;
    }
    false
}

/// True when a D-Bus session bus is actually reachable (DBUS_SESSION_BUS_ADDRESS
/// is set *and* the socket file/abstract path exists). Best-effort: if the
/// address is just inherited from root and the socket isn't there for this
/// user, we skip the spawn rather than letting `notify-send` print an error.
#[cfg(target_os = "linux")]
fn has_session_bus() -> bool {
    let addr = match std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        Ok(a) if !a.is_empty() => a,
        _ => return false,
    };
    // `unix:path=/run/user/1000/bus` — a real socket file.
    if let Some(path) = addr.strip_prefix("unix:path=") {
        return std::path::Path::new(path).exists();
    }
    // `unix:abstract=/tmp/dbus-XXXX` — abstract socket; can't easily probe, so
    // permit it (notify-send will simply no-op if absent).
    if addr.starts_with("unix:abstract=") {
        return true;
    }
    // `autolaunch:` or anything else — be conservative and skip.
    false
}

/// Write a small JSON stamp for scripting/CI. Stamps go under
/// `~/.pi/agent/notify/` by default; if that isn't writable (e.g. when pir is
/// running as a sandboxed `ai_*` project user without access to the invoking
/// user's home), fall back to a per-uid temp dir under `/tmp` so notifications
/// still work.
pub struct FileStamp;
impl Notifier for FileStamp {
    fn name(&self) -> &'static str {
        "file"
    }
    fn notify(&self, e: &AgentEvent) {
        let dir = notify_dir();
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let ts = term::epoch();
        let path = dir.join(format!("pir-{ts}.json"));
        let body = json!({
            "ts": ts,
            "event": e.kind(),
            "summary": e.summary(),
            "project": e.project,
            "prompt": e.prompt,
        });
        let _ = serde_json::to_string_pretty(&body)
            .ok()
            .and_then(|s| std::fs::write(&path, s).ok());
    }
}

/// Resolve the directory for file-stamp notifications, falling back to a
/// `/tmp` dir when the user's `~/.pi` is not writable (sandboxed `ai_*`
/// execution users). Appends the process id so concurrent agents don't collide.
fn notify_dir() -> PathBuf {
    let home = config::pi_dir().join("agent").join("notify");
    if std::fs::create_dir_all(&home).is_ok() {
        return home;
    }
    PathBuf::from(format!("/tmp/pir-notify-{}", std::process::id()))
}

/// Play a short system sound (opt-in). Subprocess stdio is detached so a
/// missing sound file / audio backend can't leak output onto the terminal.
pub struct Sound;
impl Notifier for Sound {
    fn name(&self) -> &'static str {
        "sound"
    }
    fn notify(&self, _e: &AgentEvent) {
        let mut cmd = if cfg!(target_os = "macos") {
            let mut c = Command::new("afplay");
            c.arg("/System/Library/Sounds/Glass.aiff");
            c
        } else if cfg!(target_os = "linux") {
            let mut c = Command::new("paplay");
            c.arg("/usr/share/sounds/freedesktop/stereo/complete.oga");
            c
        } else {
            return;
        };
        let _ = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

/// POST the event as JSON to a webhook. Runs off-thread so it never blocks
/// the agent's return to the prompt.
pub struct Webhook {
    url: String,
}
impl Webhook {
    pub fn new(url: String) -> Self {
        Webhook { url }
    }
}
impl Notifier for Webhook {
    fn name(&self) -> &'static str {
        "webhook"
    }
    fn notify(&self, e: &AgentEvent) {
        let url = self.url.clone();
        let payload = json!({
            "event": e.kind(),
            "summary": e.summary(),
        });
        std::thread::spawn(move || {
            // Webhook POST via the host libcurl (no static TLS): TLS comes
            // from the host's libcurl.so.4. Best-effort fire-and-forget.
            let _ = crate::provider::lsb_json(
                lsb_curl::Method::POST,
                &url,
                &[],
                Some(&payload),
                Duration::from_secs(15),
                Duration::from_secs(30),
            );
        });
    }
}

/// Render a list of events as dim lines for the on-screen notification feed.
/// Returns an empty string when there's nothing to show.
pub fn render_feed(events: &[AgentEvent]) -> String {
    if events.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for e in events {
        let tag = match e.kind {
            EventKind::Error => term::red("✗"),
            _ => term::cyan("●"),
        };
        let proj = if e.project.is_empty() {
            String::new()
        } else {
            format!(" {}", term::bold(&e.project))
        };
        let prompt = if e.prompt.is_empty() {
            String::new()
        } else {
            format!(" — {}", term::dim(&clip(&e.prompt, 80)))
        };
        out.push_str(&format!(
            "{} {} {}{}{}\n",
            term::dim("notify"),
            tag,
            term::dim(&e.summary()),
            proj,
            prompt,
        ));
    }
    out
}

#[cfg(test)]
mod title_tests {
    use super::*;
    fn ev(project: &str) -> AgentEvent {
        AgentEvent::turn_done(Duration::from_secs(1), 0, 0, project.into(), "hi".into())
    }

    /// The terminal title is identity only: `pir · <dir>`. It used to append the
    /// whole event summary, so a zellij tab strip read
    /// `pir · corro: turn done in 12.3s (1234 in / 567 out tokens)` — a fixed,
    /// usually-truncated strip with no room for it.
    #[test]
    fn terminal_title_carries_identity_not_the_event_summary() {
        let e = ev("corro");
        assert_eq!(e.terminal_title(), "pir · corro");
        // The chatty parts exist, and are deliberately kept out of the title.
        let summary = e.summary();
        assert!(summary.contains("turn done"), "sanity: summary is still verbose");
        assert!(!e.terminal_title().contains("turn done"), "summary leaked into the title");
        assert!(!e.terminal_title().contains("tokens"), "token counts leaked into the title");
        assert!(e.terminal_title().chars().count() < 20, "title must stay short");
    }

    #[test]
    fn terminal_title_falls_back_to_bare_pir() {
        assert_eq!(ev("").terminal_title(), "pir");
        assert_eq!(ev("   ").terminal_title(), "pir");
    }

    /// A deep path is shortened so the title stays scannable next to other tabs.
    #[test]
    fn terminal_title_shortens_long_paths() {
        assert_eq!(ev("home/ai_pir/src/pir").terminal_title(), "pir · src/pir");
        assert_eq!(ev("corro").terminal_title(), "pir · corro");
        assert_eq!(ev("/corro").terminal_title(), "pir · corro");
        // The desktop pop-up keeps the full path — it has room for it.
        assert_eq!(ev("home/ai_pir/src/pir").desktop_title(), "pir · home/ai_pir/src/pir");
    }
}

#[cfg(test)]
mod reload_policy_tests {
    use super::*;

    fn ev() -> AgentEvent {
        AgentEvent::turn_done(Duration::from_secs(1), 0, 0, "proj".into(), "hi".into())
    }

    // `/reload` swaps the policy in place, and the *new* policy governs the
    // external-notifier gate immediately (no restart). The on-screen feed is
    // policy-independent, so the swap is observed through `policy()` plus the
    // `allows()` gate that `publish` consults for external delivery.
    #[test]
    fn reload_policy_takes_effect_immediately() {
        let bus = NotifyBus::new(NotifyPolicy::default());
        // Default: `when: background`, `min_seconds: 8` — a 1s foreground
        // turn-done is not worth an external ping.
        assert!(
            !bus.policy().allows(&ev(), false, false),
            "default policy must not fire an external ping for a short foreground turn"
        );

        // Reload to "always / 0s": the very same event now passes the gate.
        let p = NotifyPolicy { when: "always".into(), min_seconds: 0, ..Default::default() };
        bus.reload_policy(p);
        assert_eq!(bus.policy().when, "always", "reload must swap the policy");
        assert_eq!(bus.policy().min_seconds, 0);
        assert!(
            bus.policy().allows(&ev(), false, false),
            "after reload the new policy must govern external delivery"
        );
    }

    // The on-screen feed always records non-idle events, and a reload must not
    // discard notifications already queued (the user has not seen them yet).
    #[test]
    fn reload_policy_preserves_pending_notifications() {
        let bus = NotifyBus::new(NotifyPolicy::default());
        bus.publish(ev(), false);
        assert_eq!(bus.drain_feed().len(), 1, "turn-done reaches the screen feed");

        bus.publish(ev(), false);
        // Reload to a *stricter* policy: the queued event must survive.
        let strict = NotifyPolicy { on: "error".into(), ..Default::default() };
        bus.reload_policy(strict);
        assert_eq!(
            bus.drain_feed().len(),
            1,
            "reload must not discard notifications already in the feed"
        );
        // And the feed is still policy-independent for non-idle events.
        bus.publish(ev(), false);
        assert_eq!(bus.drain_feed().len(), 1, "screen feed ignores the `on` gate");
    }
}

#[cfg(test)]
mod clip_tests {
    use super::*;

    #[test]
    fn a_short_prompt_is_returned_as_is() {
        assert_eq!(clip("fix the parser", 80), "fix the parser");
    }

    #[test]
    fn a_long_single_line_is_clipped_with_an_ellipsis() {
        let long = "x".repeat(120);
        let got = clip(&long, 80);
        assert!(got.ends_with('…'), "got: {got}");
        assert_eq!(got.chars().count(), 81, "80 chars + the ellipsis");
    }

    /// The bug this fixes: a multiline paste used to be printed verbatim into the
    /// notify line — an 80-char, 12-line paste dumped 12 lines into the feed. It must
    /// come back as the one-line placeholder instead.
    #[test]
    fn a_multiline_prompt_is_collapsed_to_one_line() {
        let pasted = "error: bad thing\n  at line 1\n  at line 2\n  at line 3\n  at line 4";
        let got = clip(pasted, 80);
        assert!(got.contains("lines pasted"), "got: {got}");
        assert!(!got.contains('\n'), "a collapsed prompt must be a single line: {got:?}");
        assert!(!got.contains("at line 2"), "the pasted body leaked into the notify line");
    }

    /// A collapsed placeholder is emitted whole: truncating it would cut the
    /// `N lines pasted ]` suffix that carries the count.
    #[test]
    fn a_collapsed_placeholder_is_never_truncated() {
        let long_first = "y".repeat(60);
        let pasted = format!("{long_first}\nb\nc\nd");
        let got = clip(&pasted, 20);
        assert!(got.ends_with("lines pasted ]"), "the count suffix must survive: {got}");
    }
}
