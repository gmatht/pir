//! In-process CPU-utilisation measurement for tests (unix).
//!
//! Several runaway-CPU bugs in pir's history were *idle* spins: the REPL/TUI
//! input wait, the stdin reader and the streaming-provider parsers could burn
//! a whole core while doing nothing at all. Those bugs are invisible to
//! ordinary unit tests (everything still passes — it just pegs the CPU), so we
//! need an assertion that can actually measure how much processor time a code
//! path consumes.
//!
//! # Per-thread, not per-process
//!
//! The sampler reads **the calling thread's** consumed CPU time
//! (`CLOCK_THREAD_CPUTIME_ID`, falling back to `CLOCK_PROCESS_CPUTIME_ID` then
//! `getrusage`). This is essential for a test suite: `cargo test` runs hundreds
//! of tests as threads in *one* process, so a process-wide counter would
//! attribute every other test's work to whichever scenario is being measured
//! (observed: a scenario that is genuinely 0.1% reporting 187%). Per-thread
//! accounting makes the measurement attributable to the code under test no
//! matter how loaded the rest of the suite is.
//!
//! # Budget
//!
//! The tests assert "under [`CPU_BUDGET_PCT`] percent". The budget is set above
//! the 8% the operator cares about so the suite is not flaky on a loaded
//! machine, while still being far below the ~100% a spin produces. Each
//! scenario additionally reports its measured figure, so the actual number is
//! visible in the test log rather than being hidden behind a pass/fail.
#![cfg(all(test, unix))]

use std::time::{Duration, Instant};

/// Percentage-of-one-core ceiling the CPU tests assert. Anything at or above
/// this in *every* sampled window is treated as a real spin (a busy loop pegs a
/// core at ~100%; the guardrail here is comfortably above the 8% the operator
/// flagged so scheduler noise on a shared CI box cannot flake the suite).
pub const CPU_BUDGET_PCT: f64 = 25.0;

/// The idle slice — the rest of a REPL poll window. This is the code's own
/// budget ([`crate::term::raw::INPUT_POLL`]), not a tuning knob: it is exactly
/// how long the REPL sleeps between wakeups while nothing is happening, so the
/// percentages these tests report are comparable to the operator's "should be
/// under 8% while idle" rule of thumb. A tick that stays within its wakeup
/// budget reports single-digit percentages; a spinning drain reports ~100%.
pub const IDLE_SLICE: Duration = Duration::from_millis(crate::term::raw::INPUT_POLL);

/// Wall-clock length of a single measurement window. Long enough that the
/// sampler's own overhead is noise, short enough that the whole suite stays
/// fast.
pub const SAMPLE_WINDOW: Duration = Duration::from_millis(120);

/// Number of windows per run; the minimum is reported.
pub const SAMPLES: usize = 3;

/// CPU time (user + system) consumed by **this thread**, in nanoseconds.
/// Returns `None` when no platform clock is available.
fn thread_cpu_nanos() -> Option<u64> {
    // Linux: precise, per-thread.
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    let r = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    if r == 0 && (ts.tv_sec > 0 || ts.tv_nsec > 0) {
        return Some(ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64);
    }
    process_cpu_nanos()
}

/// Fallback: whole-process CPU time. Only used when the per-thread clock is
/// unavailable; measurements then need a quiet machine (see module docs).
fn process_cpu_nanos() -> Option<u64> {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    if unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) } == 0
        && (ts.tv_sec > 0 || ts.tv_nsec > 0)
    {
        return Some(ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64);
    }
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } == 0 {
        let user = ru.ru_utime.tv_sec as u64 * 1_000_000_000 + ru.ru_utime.tv_usec as u64 * 1_000;
        let sys = ru.ru_stime.tv_sec as u64 * 1_000_000_000 + ru.ru_stime.tv_usec as u64 * 1_000;
        return Some(user + sys);
    }
    None
}

/// Run `body` for at least `window` of wall time and report how much CPU this
/// thread burned, as a percentage of that wall time (100.0 == one full core).
///
/// Each iteration runs `body` once and then sleeps `idle` — the "woke up, did
/// the work, went back to sleep" shape of a real REPL tick. `body` therefore
/// only needs to do *one* unit of work per call; the loop provides the pacing.
pub fn measure_pct(window: Duration, idle: Duration, mut body: impl FnMut()) -> Option<f64> {
    let before = thread_cpu_nanos()?;
    let start = Instant::now();
    while start.elapsed() < window {
        body();
        if !idle.is_zero() {
            std::thread::sleep(idle);
        }
    }
    let wall = start.elapsed();
    let after = thread_cpu_nanos()?;
    let cpu = after.saturating_sub(before) as f64;
    let wall = wall.as_secs_f64().max(f64::EPSILON);
    Some(cpu / 1_000_000_000.0 / wall * 100.0)
}

/// Take [`SAMPLES`] windows of `body` and return the lowest CPU percentage
/// observed. The minimum (rather than the mean) keeps a one-off scheduler blip
/// from failing an otherwise idle code path, while a genuine spin is ≥ 100% in
/// every window and so can never pass.
///
/// `body` need not be `Copy`: a single closure is re-invoked for every window.
pub fn min_cpu_pct(mut body: impl FnMut()) -> Option<f64> {
    let mut best: Option<f64> = None;
    for _ in 0..SAMPLES {
        let pct = measure_pct(SAMPLE_WINDOW, IDLE_SLICE, &mut body)?;
        best = Some(match best {
            Some(b) => b.min(pct),
            None => pct,
        });
    }
    best
}

/// Assert that driving `body` for several windows never exceeds
/// [`CPU_BUDGET_PCT`] percent of a core. `stage` labels the failing scenario so
/// the message says which usage pattern spun — e.g.
/// `"idle REPL, turn running, stdin at EOF"`.
///
/// The measured figure is printed either way: the operator cares about CPU
/// regressions, so the number should be visible in the test log even on a pass.
#[track_caller]
pub fn assert_idle_cpu(stage: &str, body: impl FnMut()) {
    let Some(pct) = min_cpu_pct(body) else {
        eprintln!("cpu: {stage}: no CPU clock on this platform — skipped");
        return;
    };
    eprintln!("cpu: {stage}: {pct:.1}% of one core (budget {CPU_BUDGET_PCT:.0}%)");
    assert!(
        pct <= CPU_BUDGET_PCT,
        "CPU regression at stage '{stage}': measured {pct:.1}% of one core \
         (budget {CPU_BUDGET_PCT:.0}%). A busy/idle spin burns ~100%.",
    );
}
