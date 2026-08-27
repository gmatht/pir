# FUZZTEST — fuzzing `pir`'s input/terminal layer

`pir` spends most of an interactive turn in **raw terminal mode**, parsing
stdin bytes on the main thread while a foreground agent turn streams on a
worker thread (`term::raw::read_chunk` in `src/term.rs`). That byte parser is
exactly where *crash*, *jank*, and “can't quit / can't pause” bugs hide — so
it's the first thing we fuzz.

## Tool

Coverage-guided, in-process fuzzing with **libFuzzer + AddressSanitizer +
UndefinedBehaviorSanitizer**. libFuzzer mutates inputs toward new coverage, so
it hammers the exact byte sequences that drive new branches in the parser (the
“smart” part). The `cargo fuzz` component isn't installed in this toolchain,
so the harness is a small, self-contained C++ mirror of the Rust control flow
compiled directly with `clang++`.

## Build

```sh
clang++ -std=c++17 -g -O1 -fsanitize=fuzzer,address,undefined \
    fuzz/fuzz_parser.cc -o fuzz/fuzz_parser
```

## Run

```sh
mkdir -p fuzz/corpus
# seeds target ctrl-D / ctrl-Z / ESC / backspace / ctrl-C / line+ctrl-D combos
./fuzz/fuzz_parser fuzz/corpus               # grows corpus, finds crashes
./fuzz/fuzz_parser fuzz/corpus -runs=200000
```

## What it checks (oracles)

* **Memory safety** — ASan/UBSan catch any OOB / pop-on-empty / UB in the parser.
* **ctrl-D always quits** — if a `0x04` byte appears with no ctrl-C after it,
  the parse must report `Eof` (never `None`, which would leave the REPL stuck
  with no way to quit). Traps if violated.
* **ctrl-Z** — currently ignored by the parser (the “can't pause” gap). The
  harness keeps the seed so any future change is fuzzed too.

## Results (initial run)

* 700k+ executions, **0 crashes / 0 UB**.
* ctrl-D-quit invariant holds for every sequence libFuzzer found.
* Confirmed live: **idle ctrl-D quits cleanly**; **ctrl-Z did not pause**
  (at idle the process exited instead of suspending — rustyline-owned; mid-turn
  the byte was silently dropped). Fixed by adding `RawInput::Suspend` to
  `src/term.rs` / `src/main.rs` (raise `SIGTSTP`, drop + restore raw mode,
  preserve the partial input line).

## Keeping it honest

This C harness *mirrors* `read_chunk`. When you change the Rust parser, update
the C mirror to match, or (better) add a `#[cfg(test)]` Rust oracle that drives
`term::raw::read_chunk` directly once it's `pub(crate)`-testable. The mirror is
a stand-in for a `cargo fuzz` target that would fuzz the real Rust code
in-process; enable `cargo +nightly fuzz` and move this into
`fuzz/fuzz_targets/` once the component is installed.

## Related: network/retry fuzzing

The HTTP request/retry path (`src/provider.rs`) has its own hardening — a
cancellation flag checked between reads, a stall watchdog, and retryable-error
classification (`is_retryable`). It is covered by `#[cfg(test)]` unit tests
(`retryable_http_codes`, `non_retryable_http_codes`, `retryable_transport_errors`,
`backoff_grows_and_caps`, `timeout_constants_sane`, and `cancel_aborts_inflight_stream`).
See `src/provider.rs` for the status-line timeout fix (generous initial read
timeout that doubles each retry).
