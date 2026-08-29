# POLLING.md — polling / busy-wait inventory

This document is the result of an audit of every `thread::sleep`, `recv_timeout`,
`is_finished`, and busy-loop in the pir codebase, with the decision for each:
**replaced with a smol event-driven idiom**, or **kept as-is and why**.

The guiding rule: never spin a CPU core. A "polling loop" is only a problem if it
is a genuine busy-wait (no sleep, or a sleep that is effectively never waited on).
Every site here is either a bounded sleep (idle, near-0% CPU) or was converted to
block on the smol reactor.

## Replaced

### TUI idle line editor (`src/tui.rs`, `read_idle_line`)

**Was:** a tight `loop { nonblocking read; thread::sleep(30ms) }` — a periodic
30ms poll that both redrew the footer draft and checked stdin.

**Now:** the loop blocks once per tick in an event-driven race:
`smol::future::or(stdin.readable(), smol::Timer::after(30ms))`. A real keypress
wakes the reactor instantly; otherwise the 30ms timer fires to keep the footer
live. An EOF pipe would make `readable()` fire immediately forever (the runaway
CPU bug), so a no-input path that returns in under 30ms sleeps the remainder of
the tick (the same EOF-throttle pattern already used by `wait_input` /
`wait_raw_input`). Near-0% CPU when idle.

## Already event-driven (no change needed)

These were already smol-based and never spin:

- **Streaming REPL mid-turn wait** (`src/term.rs`, `wait_input`) and **TUI
  running-turn wait** (`src/tui.rs`, `wait_raw_input`): race
  `stdin.readable()` vs the turn-completion channel, plus an EOF-throttle so a
  closed-pipe stdin can't busy-spin. These two were the original 100%-CPU bug;
  both fixed and committed.
- **`is_finished()` reaping** (`src/main.rs` `reap` / `fg_handle`, `src/tui.rs`
  `reap_bg` / `fg_handle`): the outer REPL/TUI loop already blocks in an
  event-driven wait (above) and only checks `is_finished()` on the wakeup, so it
  is a post-wakeup check, not a poll loop.

## Kept (bounded sleeps / cannot be smol-based) — and why

### `src/provider.rs` — `send_cancelable` (10ms `is_finished()` poll)

Runs `ureq`'s blocking `send_json` (connect + status-line read) on a worker
thread and polls the join handle in 10ms slices so a `cancel` is observed
promptly. `ureq` has **no async transport** — there is no future to await, only a
blocking call on a thread and an `AtomicBool`. A smol `spawn_blocking` +
`select` would need ureq itself to be async-aware; the 10ms poll is the 
cheapest correct way to race a blocking syscall against an atomic flag. Bounded
sleep (idle between polls), not a busy-wait.

### `src/provider.rs` — retry backoff (100ms-slice sleep)

`chat`'s retry loop sleeps the (up to 240s) backoff in 100ms slices so a cancel
mid-backoff is honoured promptly. Same story as above: the cancel is an
`AtomicBool`, and the surrounding work is synchronous on the caller's thread
(the turn worker). An `AtomicBool` has no awaitable primitive; polling it in
slices is the standard pattern. Bounded sleep.

### `src/provider.rs` — `CancelableReader::read` (20ms `recv_timeout`)

Wraps a blocking `Read` (ureq's SSE body reader) with a pump thread into a
channel so cancellation is honoured between bytes. The consumer is a **blocking
`Read` impl**, not async — it cannot `await`. The 20ms `recv_timeout` poll is
how a synchronous reader observes an `AtomicBool`-driven cancel. Bounded sleep.

### `src/term.rs` `Spinner` / TUI spinner (80ms sleep)

Animation frames. An animation must tick on a schedule; sleeping 80ms per frame
is the event. Could be a `smol::Timer` loop, but the spinner runs on its own
`std::thread` (the agent's turn thread) with an `AtomicBool` stop flag — not an
async context — so a `thread::sleep` is the natural fit. Bounded sleep; ~1.3%
of a core.

### `src/term.rs` `read_byte_timeout` / `drain_csi_sequence`; `src/tui.rs`
`read_byte_timeout` (2ms micro-waits)

Per-byte disambiguation of a lone Esc vs the start of a CSI escape sequence on
an already non-blocking fd. Each iteration does one `libc::read(1 byte)` and, on
`EAGAIN`, sleeps 2ms up to a short deadline (25ms). This must stay synchronous
(the byte must be consumed before the next input poll) and is bounded by a
deadline; the 2ms sleep keeps it far below 100% CPU. Not worth a reactor.

### `src/main.rs` `spawn_model_broadcast_watcher` (2s file poll)

Cross-instance `/model*` broadcast: polls `~/.pi/agent/model-broadcast.json`
every 2s. There is no fs-event daemon running for the user, and the file is
written by an independent `pir` process — so a timer poll is the only portable
way to observe a change across processes. A smol `Timer::after(2s)` loop would
be equally correct but would still be a poll; no event source exists to await.
2s sleep; negligible CPU.

### Test-only servers (`src/provider.rs`, `#[cfg(test)]`)

Mock HTTP servers that `thread::sleep` to hold a socket open or time a slow
provider. Not production code; no reason to convert.

## Summary

- 1 genuine polling loop replaced: TUI idle line editor → smol event-driven.
- The original 100%-CPU bug (EOF-pipe stdin busy-spin in the running-turn
  waits) was already fixed and committed before this audit.
- Every remaining site is a **bounded sleep** on an `AtomicBool`/animation/
  per-byte-read/cross-process poll that has no async event source to await;
  converting them to smol would add no CPU win and, for the provider sites,
  would require an async HTTP stack that `ureq` doesn't provide.
