//! Notification coalescing and child-process reaping.
//!
//! Two problems this solves, both observed on a box running seven agent loops
//! concurrently (76 zombies, `dbus-daemon` carrying 28 GB of notification
//! traffic, on a disk that could not keep up):
//!
//! 1. **No coalescing.** Every [`AgentEvent`] spawned its own notifier
//!    subprocess. A loop running many short turns fired many `notify-send` /
//!    `powershell.exe` processes back to back, each one a fork, a D-Bus round
//!    trip and an exit. Individually cheap; collectively a sustained process
//!    churn that competes with the very disk writes the notifications were
//!    meant to inform you about.
//!
//! 2. **No reaping.** The `spawn()` sites dropped the `Child` immediately. The
//!    only `waitpid` in the crate was for a foreground command in `term.rs`.
//!    A `pir` that spawns and never waits accumulates one zombie per
//!    notification until the process exits — 76 of them were observed.
//!
//! The fix is a single long-lived notifier thread. It owns a queue, collapses
//! bursts of events into one delivery, spawns the backend, and — crucially —
//! reaps it. One thread, one place that can block on a child, and no zombies.
//!
//! Coalescing policy: events arriving within [`COALESCE_WINDOW`] of each other
//! collapse to the most recent one *per kind*, because a user cares that
//! "turn done" happened once, not that it happened eleven times in two
//! seconds. The most recent event wins (it carries the newest prompt/duration),
//! so nothing is reported that is not still true. A trailing timer guarantees a
//! single pending event is still delivered even if nothing follows it.

// This is a top-level module (sibling of `notify`), not a child of it, so the
// event types are addressed via the crate path rather than `super::`.
use crate::notify::{AgentEvent, EventKind};
use std::collections::HashMap;
use std::process::Command;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Events arriving within this window of each other collapse into one
/// delivery. Long enough to absorb a burst of short turns from a loop, short
/// enough that a genuine "turn done" still feels immediate.
pub const COALESCE_WINDOW: Duration = Duration::from_millis(750);

/// How the backend is invoked. Passed in so this module stays testable
/// without spawning real `notify-send` / `powershell.exe` processes.
type Spawner = Arc<dyn Fn(&AgentEvent) + Send + Sync>;

/// A running coalescing notifier. Dropping the handle stops the thread.
pub struct Coalescer {
    tx: Option<Sender<AgentEvent>>,
}

impl Coalescer {
    /// Start a notifier thread that collapses events and calls `spawner` for
    /// each surviving one.
    pub fn start(spawner: Spawner) -> Self {
        let (tx, rx) = channel::<AgentEvent>();
        std::thread::Builder::new()
            .name("pir-notify".into())
            .spawn(move || run(rx, &*spawner))
            .ok();
        Coalescer { tx: Some(tx) }
    }

    /// Queue an event. Non-blocking and never fails: a notification is never
    /// important enough to stall a turn, so a full/closed channel is ignored.
    pub fn send(&self, e: AgentEvent) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(e);
        }
    }
}

impl Drop for Coalescer {
    fn drop(&mut self) {
        // Closing the channel lets the thread drain and exit.
        self.tx.take();
    }
}

/// The notifier thread: collapse bursts, then deliver.
///
/// One clean state machine:
///   * block for the next event, or for the earliest pending window to expire;
///   * on an event, record it as the pending event for its kind (replacing any
///     earlier one of the same kind, which is the coalescing);
///   * on an expired window, deliver every event that is due;
///   * on a closed channel, deliver everything still pending and exit.
fn run(rx: Receiver<AgentEvent>, spawner: &dyn Fn(&AgentEvent)) {
    // Newest event per kind, and when its window started.
    let mut pending: HashMap<EventKind, (AgentEvent, Instant)> = HashMap::new();
    loop {
        let deadline = pending.values().map(|(_, t)| *t + COALESCE_WINDOW).min();
        let received = match deadline {
            // Nothing pending: block until an event arrives or the sender goes.
            None => match rx.recv() {
                Ok(e) => Some(e),
                Err(_) => {
                    // Channel closed with nothing pending: done.
                    return;
                }
            },
            // Something pending: wait out the window, or take a new event.
            Some(d) => {
                let now = Instant::now();
                if d <= now {
                    None // window already expired -> deliver due events below
                } else {
                    match rx.recv_timeout(d - now) {
                        Ok(e) => Some(e),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => {
                            // Sender gone: deliver the rest and exit.
                            flush(&mut pending, spawner);
                            return;
                        }
                    }
                }
            }
        };

        if let Some(e) = received {
            pending.insert(e.kind, (e, Instant::now()));
        } else {
            // The window expired: deliver whatever is due.
            flush_due(&mut pending, spawner);
        }
    }
}

/// Deliver every event whose window has expired, dropping it from `pending`.
/// Returns true if anything was delivered.
fn flush_due(
    pending: &mut HashMap<EventKind, (AgentEvent, Instant)>,
    spawner: &dyn Fn(&AgentEvent),
) -> bool {
    let now = Instant::now();
    let due: Vec<EventKind> = pending
        .iter()
        .filter(|(_, (_, t))| now.duration_since(*t) >= COALESCE_WINDOW)
        .map(|(k, _)| *k)
        .collect();
    let mut any = false;
    for k in due {
        if let Some((e, _)) = pending.remove(&k) {
            spawner(&e);
            any = true;
        }
    }
    any
}

/// Deliver everything, regardless of window. Used on shutdown.
fn flush(pending: &mut HashMap<EventKind, (AgentEvent, Instant)>, spawner: &dyn Fn(&AgentEvent)) {
    let all: Vec<EventKind> = pending.keys().copied().collect();
    for k in all {
        if let Some((e, _)) = pending.remove(&k) {
            spawner(&e);
        }
    }
}

/// A process-wide coalescer, created lazily on the first [`notify`] call.
///
/// One per process on purpose: seven agent loops inside one `pir` would
/// otherwise each start their own thread and each spawn its own backend per
/// event, reintroducing the churn this exists to remove.
static COALESCER: Mutex<Option<Arc<Coalescer>>> = Mutex::new(None);

/// Install the process-wide coalescing notifier. Called once at startup.
///
/// The spawner is the only place a notification backend is invoked, so this
/// is also the single place that spawns (and reaps) a child process. An
/// already-installed coalescer is kept, so repeated calls are harmless.
pub fn install(spawner: Spawner) {
    let mut g = COALESCER.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        *g = Some(Arc::new(Coalescer::start(spawner)));
    }
}

/// Whether a coalescer has been installed.
pub fn is_installed() -> bool {
    COALESCER.lock().map(|g| g.is_some()).unwrap_or(false)
}

/// Queue an event on the process-wide coalescer. A no-op if [`install`] was
/// never called (so tests and embedded uses stay quiet by default).
pub fn queue(e: AgentEvent) {
    let g = COALESCER.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = g.as_ref() {
        c.send(e);
    }
}

/// Spawn a backend for one event and **reap it**, so it cannot become a
/// zombie. Used by the coalescing thread; the blocking `wait` is safe there
/// because it is a dedicated thread and only ever holds one child at a time.
pub fn spawn_and_reap(cmd: &mut Command) {
    match cmd.spawn() {
        Ok(mut child) => {
            // wait() reaps. Without this the Child is dropped and the process
            // becomes a zombie until pir exits.
            let _ = child.wait();
        }
        Err(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A recording spawner that returns a handle to the count and the events.
    fn recorder() -> (Spawner, Arc<AtomicUsize>, Arc<Mutex<Vec<AgentEvent>>>) {
        let n = Arc::new(AtomicUsize::new(0));
        let got = Arc::new(Mutex::new(Vec::new()));
        let (n2, g2) = (n.clone(), got.clone());
        let s: Spawner = Arc::new(move |e: &AgentEvent| {
            n2.fetch_add(1, Ordering::SeqCst);
            g2.lock().unwrap().push(e.clone());
        });
        (s, n, got)
    }

    fn ev(kind: EventKind, prompt: &str) -> AgentEvent {
        AgentEvent {
            kind,
            duration: Duration::from_secs(1),
            in_tokens: 0,
            out_tokens: 0,
            message: String::new(),
            project: "t".into(),
            prompt: prompt.into(),
        }
    }

    #[test]
    fn a_burst_collapses_to_one_delivery_keeping_the_newest() {
        let (spawner, n, got) = recorder();
        let c = Coalescer::start(spawner);
        // 20 turn-done events in a tight burst, each with a distinct prompt.
        for i in 0..20 {
            c.send(ev(EventKind::TurnDone, &format!("p{i}")));
        }
        // Wait past the window so the coalescer flushes.
        std::thread::sleep(COALESCE_WINDOW + Duration::from_millis(400));
        assert_eq!(n.load(Ordering::SeqCst), 1, "a burst must collapse to one delivery");
        let g = got.lock().unwrap();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].prompt, "p19", "the newest event must be the one delivered");
        drop(c);
    }

    #[test]
    fn different_kinds_are_coalesced_separately() {
        let (spawner, n, got) = recorder();
        let c = Coalescer::start(spawner);
        c.send(ev(EventKind::TurnDone, "a"));
        c.send(ev(EventKind::Error, "b"));
        c.send(ev(EventKind::TurnDone, "c")); // same kind -> replaces "a"
        std::thread::sleep(COALESCE_WINDOW + Duration::from_millis(400));
        assert_eq!(n.load(Ordering::SeqCst), 2, "two kinds -> two deliveries");
        let g = got.lock().unwrap();
        let kinds: Vec<EventKind> = g.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&EventKind::TurnDone));
        assert!(kinds.contains(&EventKind::Error));
        // The turn-done delivered is the newer "c", not the older "a".
        let td = g.iter().find(|e| e.kind == EventKind::TurnDone).unwrap();
        assert_eq!(td.prompt, "c");
        drop(c);
    }

    #[test]
    fn a_lone_event_is_still_delivered_via_the_trailing_timer() {
        let (spawner, n, _got) = recorder();
        let c = Coalescer::start(spawner);
        c.send(ev(EventKind::Idle, "only"));
        // No further events: the trailing timer must still fire.
        std::thread::sleep(COALESCE_WINDOW + Duration::from_millis(400));
        assert_eq!(n.load(Ordering::SeqCst), 1, "a single event must not be swallowed");
        drop(c);
    }

    #[test]
    fn events_far_apart_are_each_delivered() {
        let (spawner, n, _got) = recorder();
        let c = Coalescer::start(spawner);
        c.send(ev(EventKind::TurnDone, "first"));
        std::thread::sleep(COALESCE_WINDOW + Duration::from_millis(300));
        c.send(ev(EventKind::TurnDone, "second"));
        std::thread::sleep(COALESCE_WINDOW + Duration::from_millis(400));
        assert_eq!(n.load(Ordering::SeqCst), 2, "separated events are two deliveries");
        drop(c);
    }

    #[test]
    fn dropping_the_handle_flushes_pending_events_and_exits_the_thread() {
        let (spawner, n, _got) = recorder();
        let c = Coalescer::start(spawner);
        c.send(ev(EventKind::TurnDone, "last"));
        drop(c); // must flush "last" rather than discard it
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(n.load(Ordering::SeqCst), 1, "shutdown must flush, not drop");
    }

    #[test]
    fn spawn_and_reap_leaves_no_zombie() {
        // On unix we can check /proc directly. The Child is reaped by wait(), so
        // the process must not linger as a zombie.
        #[cfg(unix)]
        {
            let before = std::fs::read_dir("/proc/self/task").map(|r| r.count()).unwrap_or(0);
            let mut cmd = Command::new("/bin/sh");
            cmd.args(["-c", "exit 0"]);
            // Fire a few; without wait() each would be a zombie.
            for _ in 0..5 {
                spawn_and_reap(&mut cmd);
            }
            std::thread::sleep(Duration::from_millis(100));
            // Count zombie children of this process.
            let mut zombies = 0;
            if let Ok(rd) = std::fs::read_dir("/proc") {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if !name.chars().all(|c| c.is_ascii_digit()) {
                        continue;
                    }
                    if let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) {
                        // state field is the 3rd parenthesised group; Z = zombie
                        if let Some(close) = stat.rfind(')') {
                            let rest = &stat[close + 1..];
                            let mut it = rest.split_whitespace();
                            let _pid = it.next();
                            if let Some(state) = it.next() {
                                if state == "Z" {
                                    zombies += 1;
                                }
                            }
                        }
                    }
                }
            }
            let _ = before;
            assert_eq!(zombies, 0, "spawn_and_reap must leave no zombies");
        }
    }
}

#[cfg(test)]
mod live_reap_tests {
    use super::*;
    use crate::notify::{EventKind, AgentEvent};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Count zombie children of this process.
    fn zombies() -> usize {
        let me = std::process::id();
        std::fs::read_dir("/proc")
            .map(|rd| {
                rd.flatten()
                    .filter(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .chars()
                            .all(|c| c.is_ascii_digit())
                    })
                    .filter(|e| {
                        std::fs::read_to_string(e.path().join("stat"))
                            .ok()
                            .and_then(|s| {
                                let close = s.rfind(')')?;
                                let rest = &s[close + 1..];
                                let mut it = rest.split_whitespace();
                                it.next()?; // pid
                                let state = it.next()?;
                                let ppid: u32 = it.next()?.parse().ok()?;
                                Some(state == "Z" && ppid == me)
                            })
                            .unwrap_or(false)
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    /// The real shape of the bug: many notification events, each spawning a
    /// backend, with the Child dropped (the pre-fix behaviour). The coalescer
    /// must collapse the burst AND the spawner must reap, so the process is
    /// left with no zombies.
    #[test]
    fn a_burst_of_notifications_leaves_no_zombies() {
        let before = zombies();
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        // This is exactly what the production install() wires up: spawn a real
        // child per delivered event, and reap it.
        let spawner: Spawner = Arc::new(move |_e: &AgentEvent| {
            n2.fetch_add(1, Ordering::SeqCst);
            let mut c = Command::new("/bin/sh");
            c.args(["-c", "exit 0"]);
            spawn_and_reap(&mut c);
        });
        let c = Coalescer::start(spawner);
        for _ in 0..30 {
            c.send(AgentEvent {
                kind: EventKind::TurnDone,
                duration: Duration::from_millis(10),
                in_tokens: 0,
                out_tokens: 0,
                message: String::new(),
                project: "t".into(),
                prompt: "p".into(),
            });
        }
        std::thread::sleep(COALESCE_WINDOW + Duration::from_millis(400));
        drop(c);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            zombies(),
            before,
            "coalescing + reaping must leave no zombie children"
        );
    }

    /// The control case: prove the coalescing actually reduces spawns. 30
    /// events in one burst must cost far fewer than 30 backend processes —
    /// this is the write/process-churn saving, not just the zombie fix.
    #[test]
    fn coalescing_actually_reduces_the_number_of_spawns() {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let spawner: Spawner = Arc::new(move |_e: &AgentEvent| {
            n2.fetch_add(1, Ordering::SeqCst);
        });
        let c = Coalescer::start(spawner);
        for _ in 0..30 {
            c.send(AgentEvent {
                kind: EventKind::TurnDone,
                duration: Duration::from_millis(10),
                in_tokens: 0,
                out_tokens: 0,
                message: String::new(),
                project: "t".into(),
                prompt: "p".into(),
            });
        }
        std::thread::sleep(COALESCE_WINDOW + Duration::from_millis(400));
        let spawned = n.load(Ordering::SeqCst);
        drop(c);
        assert!(
            spawned < 30,
            "a 30-event burst must collapse below 30 spawns, got {spawned}"
        );
    }
}
