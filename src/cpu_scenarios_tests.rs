//! Realistic idle-CPU scenarios: drive the *actual* input paths pir uses while
//! waiting at the prompt (and while a turn runs) and assert the process never
//! burns more than [`crate::cpu_probe::CPU_BUDGET_PCT`] of a core.
//!
//! Every scenario here reproduces a shape that has historically spun:
//!
//! | stage | what spins if broken |
//! |---|---|
//! | `idle REPL at the prompt` | the idle wait loop |
//! | `turn running, stdin at EOF`  | `raw::wait_input` racing `readable()` (always-ready at EOF) against turn-completion, then draining a dead fd |
//! | `wait_input, pipe open but silent` | the smol reactor + the throttle window |
//! | `read_chunk on a silent fd` | the drain loop re-reading a non-blocking fd |
//! | `translate on a burst of control bytes` | per-byte handling of CSI/ESC |
//! | `ESC disambiguation` | `read_byte_timeout` polling for a CSI tail |
//!
//! # How a scenario is paced
//!
//! Each measured iteration runs **one REPL tick** — one call into the code path
//! under test — and the harness then sleeps the rest of the window
//! ([`crate::cpu_probe::IDLE_SLICE`]), exactly like a REPL that woke, found no
//! input and went back to sleep. What the assertion therefore catches is the
//! real bug class: work that no longer fits in the wakeup budget, i.e. a drain
//! path that spins instead of returning.
//!
//! The idle slice is derived from the code's own budget — the REPL must finish
//! a tick's work in well under one [`crate::term::raw::INPUT_POLL`] window —
//! so the percentages these tests report are directly comparable to the
//! "should be under 8%" rule of thumb, even though the *budget* asserted is
//! deliberately looser (see [`CPU_BUDGET_PCT`]).
//!
//! # Why the fd is redirected
//!
//! `wait_input` / `read_chunk` / `translate` all read **fd 0** directly (they
//! are the terminal layer; they do not take a reader). To make the scenarios
//! deterministic and hermetic they therefore install a controlled fd 0:
//! a pipe at EOF (the classic runaway-CPU trigger), a pipe held open but
//! silent, or a pipe pre-filled with a fixed byte stream. The swap is guarded
//! by a process-wide mutex and restored afterwards, and a drop guard restores
//! it even if an assertion fires — the test harness itself never reads stdin.
//!
//! The mutex is deliberately **poison-tolerant**: a failing scenario must not
//! cascade into `PoisonError` failures in every sibling test (that would hide
//! the real measurement behind noise).
#![cfg(all(test, unix))]

use crate::cpu_probe::{assert_idle_cpu, CPU_BUDGET_PCT};
use crate::term::raw::{wait_input, RawInput};
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Serializes every test that swaps fd 0 (and therefore the whole file): the
/// scenarios are only meaningful one at a time. Errors are ignored so one
/// failure cannot poison the rest of the suite.
fn fd_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// What the fake fd 0 should look like while a scenario runs.
enum FakeStdin {
    /// Pipe whose write end is closed: `readable()` is true *forever* and
    /// `read()` returns 0. This is the exact EOF shape that spun at 100% CPU.
    PipeAtEof,
    /// Pipe whose write end is held open but nothing is ever written: the
    /// reactor blocks (0% CPU) and the drain finds EAGAIN.
    PipeOpenSilent,
    /// Pipe pre-filled with `bytes`, write end closed. Every read then hits EOF.
    PipeWith(Vec<u8>),
}

/// Installs `fake` as fd 0 for the lifetime of the guard, restoring the real
/// fd 0 on drop (including on panic).
struct StdinGuard {
    saved: OwnedFd,
    writer: Option<OwnedFd>,
}

impl StdinGuard {
    fn install(fake: FakeStdin) -> StdinGuard {
        let saved = unsafe { OwnedFd::from_raw_fd(libc::dup(0)) };
        let (reader, writer) = match fake {
            FakeStdin::PipeAtEof => {
                let (r, w) = pipe().expect("pipe");
                drop(w);
                (r, None)
            }
            FakeStdin::PipeOpenSilent => {
                let (r, w) = pipe().expect("pipe");
                (r, Some(w))
            }
            FakeStdin::PipeWith(bytes) => {
                let (r, w) = pipe().expect("pipe");
                write_all(w.as_raw_fd(), &bytes).expect("seed pipe");
                drop(w);
                (r, None)
            }
        };
        // dup2(reader, 0) — replaces fd 0, keeping it open for the raw readers.
        let rc = unsafe { libc::dup2(reader.as_raw_fd(), 0) };
        assert!(rc >= 0, "dup2 fake stdin onto fd 0 failed");
        // `reader` is not needed as a separate handle any more; fd 0 holds it.
        drop(reader);
        StdinGuard { saved, writer }
    }
}

impl Drop for StdinGuard {
    fn drop(&mut self) {
        // Restore the real stdin *first* so nothing observes a closed fd 0.
        let _ = unsafe { libc::dup2(self.saved.as_raw_fd(), 0) };
        // Closing the held-open write end would surface as EOF; drop it after.
        if let Some(w) = self.writer.take() {
            drop(w);
        }
    }
}

fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn write_all(fd: i32, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let n = unsafe { libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len()) };
        if n <= 0 {
            return Err(std::io::Error::last_os_error());
        }
        bytes = &bytes[n as usize..];
    }
    Ok(())
}

/// Put fd 0 into non-blocking mode the way `enable_raw` does, so the raw
/// readers behave exactly as they do in a live session.
fn set_nonblocking(fd: i32) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        assert!(flags >= 0, "fcntl(F_GETFL) failed");
        let rc = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        assert!(rc >= 0, "fcntl(F_SETFL) failed");
    }
}

fn typeahead() -> Arc<Mutex<String>> {
    Arc::new(Mutex::new(String::new()))
}

/// A `done` channel that is never signalled and stays open — "a turn that is
/// running but has nothing to report yet" (e.g. parked in a retry backoff).
fn pending_turn() -> smol::channel::Receiver<()> {
    let (tx, rx) = smol::channel::bounded::<()>(1);
    // Leak the sender: dropping it would *close* the channel and `wait_input`
    // would return immediately (the opposite of the scenario under test).
    std::mem::forget(tx);
    rx
}

// ── Scenarios ───────────────────────────────────────────────────────────────

/// A turn is running and stdin is a pipe at EOF: `readable()` fires forever
/// while the turn never completes. Without the throttle window in
/// `wait_input` this is a 100%-CPU spin; with it each tick sleeps ~`INPUT_POLL`
/// *and* the drain finds EOF immediately. This is *the* runaway-CPU scenario.
#[test]
fn wait_input_turn_running_stdin_at_eof_stays_idle() {
    let _serial = fd_lock();
    let _guard = StdinGuard::install(FakeStdin::PipeAtEof);
    set_nonblocking(0);
    let ta = typeahead();
    let done = pending_turn();

    assert_idle_cpu("turn running, stdin at EOF (one wait_input tick)", || {
        let mut buf = String::new();
        let mut recall = crate::term::HistRecall::default();
        let r = wait_input(&mut buf, &ta, &done, &mut recall);
        // The real REPL re-loops on `None`; assert the contract here so a
        // future change that returns input cannot silently make this vacuous.
        assert_eq!(r, RawInput::None, "a silent stdin must not produce input");
    });
}

/// A turn is running and stdin is an open pipe nobody writes to: the reactor
/// blocks on readability (the healthy, fully event-driven path — genuinely 0%
/// CPU, no polling). We therefore complete the turn from another thread, which
/// is exactly what a real worker does when it finishes, so each measured tick
/// returns after a bounded block and the drain path is exercised.
#[test]
fn wait_input_turn_running_stdin_open_and_idle_stays_idle() {
    let _serial = fd_lock();
    let _guard = StdinGuard::install(FakeStdin::PipeOpenSilent);
    set_nonblocking(0);
    let ta = typeahead();

    assert_idle_cpu("turn running, stdin open but silent (one wait_input tick)", || {
        // Signal turn completion after a short block, so this tick ends without
        // waiting on input that will never arrive.
        let (tx, rx) = smol::channel::bounded::<()>(1);
        let done_tx = tx.clone();
        let waker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2));
            let _ = smol::block_on(done_tx.send(()));
        });
        let mut buf = String::new();
        let mut recall = crate::term::HistRecall::default();
        let r = wait_input(&mut buf, &ta, &rx, &mut recall);
        let _ = waker.join();
        assert_eq!(r, RawInput::None, "a silent stdin must not produce input");
    });
}

/// Typed-ahead input arriving while a turn runs: a tick must deliver the queued
/// `Line` cheaply (the bytes are already buffered, so no fd peeking is needed).
#[test]
fn wait_input_typed_ahead_line_is_cheap() {
    let _serial = fd_lock();
    let _guard = StdinGuard::install(FakeStdin::PipeWith(b"queued prompt\n".to_vec()));
    set_nonblocking(0);
    let ta = typeahead();
    let done = pending_turn();
    let mut seen_line = false;
    // A fresh buffer per tick so the first tick's `Line` is delivered again
    // every iteration (the pipe stays at EOF once drained, so later ticks are
    // the silent path — measured together, as the REPL experiences them).
    assert_idle_cpu("typed-ahead line while a turn runs (wait_input ticks)", || {
        let mut buf = String::new();
        let mut recall = crate::term::HistRecall::default();
        if wait_input(&mut buf, &ta, &done, &mut recall)
            == RawInput::Line("queued prompt".to_string())
        {
            seen_line = true;
        }
    });
    assert!(seen_line, "the pre-filled line must be delivered as a queued prompt");
}

/// The drain path itself, on the exact state the REPL sits in between
/// keystrokes: a non-blocking fd with nothing buffered. One `read_chunk` tick
/// must be a single `read` that EAGAINs — never a retry loop.
#[test]
fn read_chunk_on_silent_fd_is_cheap() {
    let _serial = fd_lock();
    let _guard = StdinGuard::install(FakeStdin::PipeOpenSilent);
    set_nonblocking(0);
    let ta = typeahead();
    assert_idle_cpu("read_chunk on a silent non-blocking fd (one tick)", || {
        let mut buf = String::new();
        let mut recall = crate::term::HistRecall::default();
        let r = crate::term::raw::read_chunk_for_test(&mut buf, &ta, &mut recall);
        assert_eq!(r, RawInput::None);
    });
}

/// Same drain path, EOF flavour: a closed/EOF fd is *permanently* readable, so
/// retrying on `read() == 0` is an infinite hot loop. One tick must return.
#[test]
fn read_chunk_on_eof_fd_is_cheap() {
    let _serial = fd_lock();
    let _guard = StdinGuard::install(FakeStdin::PipeAtEof);
    set_nonblocking(0);
    let ta = typeahead();
    assert_idle_cpu("read_chunk on an EOF fd (one tick)", || {
        let mut buf = String::new();
        let mut recall = crate::term::HistRecall::default();
        let r = crate::term::raw::read_chunk_for_test(&mut buf, &ta, &mut recall);
        assert_eq!(r, RawInput::None);
    });
}

/// A bare ESC at the end of the buffer makes `read_chunk` peek the fd for a
/// possible CSI tail (`read_byte_timeout`, a 2ms-granularity poll). On a
/// non-blocking fd at EOF that peek must give up quickly, not busy-loop.
#[test]
fn lone_escape_peek_terminates_cheaply() {
    let _serial = fd_lock();
    let _guard = StdinGuard::install(FakeStdin::PipeWith(vec![0x1b]));
    set_nonblocking(0);
    let ta = typeahead();
    let done = pending_turn();
    let mut saw_cancel = false;

    assert_idle_cpu("lone ESC disambiguation (one wait_input tick)", || {
        let mut buf = String::new();
        let mut recall = crate::term::HistRecall::default();
        if wait_input(&mut buf, &ta, &done, &mut recall) == RawInput::Cancel {
            saw_cancel = true;
        }
    });
    assert!(saw_cancel, "a lone ESC must cancel, not hang");
}

/// `translate` is the shared byte-level front-end for the TUI's reader; feed it
/// a realistic keystroke burst (a few arrows, a paste, backspaces, some text —
/// the most a user produces inside one poll window) and confirm a tick stays
/// cheap. The synthetic 0.5ms work slice models the display/redraw cost that
/// shares the window, so the tick as a whole must still fit the poll budget.
#[test]
fn translate_control_byte_burst_is_cheap() {
    let _serial = fd_lock();
    let _guard = StdinGuard::install(FakeStdin::PipeAtEof);
    let mut stream: Vec<u8> = Vec::new();
    for _ in 0..8 {
        stream.extend_from_slice(b"\x1b[A"); // Up
        stream.extend_from_slice(b"\x1b[B"); // Down
        stream.extend_from_slice(b"\x1b[H"); // Home
        stream.extend_from_slice(b"\x1b[F"); // End
        stream.extend_from_slice(b"\x1b[200~pasted\nblock\x1b[201~");
        stream.extend_from_slice(b"typing\x7f\x7f");
    }
    let ta = typeahead();
    assert_idle_cpu("translate() on a keystroke burst (one tick)", || {
        // Stand in for the rest of a tick's work (spinner repaint etc.).
        let spin = std::time::Instant::now() + Duration::from_micros(500);
        while std::time::Instant::now() < spin {
            std::hint::black_box(1u64.wrapping_mul(3));
        }
        let mut buf = String::new();
        let r = crate::term::raw::translate(&mut buf, &ta, &stream);
        // No Enter outside a paste and no ctrl-c/ctrl-d ⇒ no terminal outcome.
        assert_eq!(r, RawInput::None, "the burst must not end the line");
    });
}

/// The throttle constants are what keep the idle paths bounded. Guard the
/// invariants: a 0ms poll window busy-spins, a huge one lags typing, and a
/// budget at/above a full core would make every scenario vacuous.
#[test]
fn idle_poll_window_and_budget_are_bounded() {
    const _: () = assert!(crate::term::raw::INPUT_POLL > 0, "a 0ms window busy-spins");
    const _: () = assert!(
        crate::term::raw::INPUT_POLL <= 250,
        "a >250ms window would visibly lag typing"
    );
    assert!(
        CPU_BUDGET_PCT < 100.0,
        "CPU_BUDGET_PCT must sit below a full core, got {CPU_BUDGET_PCT}"
    );
}

/// The measurement harness itself must actually work: a deliberate spin has to
/// read as a spin, i.e. its CPU cost must swamp the paced idle sleep. Without
/// this, every `assert_idle_cpu` above could pass by accident.
#[test]
fn probe_detects_a_deliberate_spin() {
    let _serial = fd_lock();
    let pct = crate::cpu_probe::measure_pct(
        Duration::from_millis(120),
        Duration::from_millis(1),
        || {
            // ~5ms of solid work per tick against a 1ms idle sleep.
            let spin = std::time::Instant::now() + Duration::from_millis(5);
            while std::time::Instant::now() < spin {
                std::hint::black_box(1u64.wrapping_mul(3));
            }
        },
    )
    .expect("process CPU clock available in tests");
    assert!(
        pct > CPU_BUDGET_PCT,
        "the CPU probe failed to detect a deliberate spin: {pct:.1}% (budget {CPU_BUDGET_PCT:.0}%)"
    );
}
