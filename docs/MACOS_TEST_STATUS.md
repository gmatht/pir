# macOS (imac) build & test status

Verified by running the suite over ssh on `imac` (macOS 10.13.6, x86_64,
`x86_64-apple-darwin`, rustc 1.99.0). Tree lives at `~/pir-final` on that box.

> **Date note.** The counts below are a snapshot of one verified run on that
> tree. The Linux column has since moved: this working copy reports **780
> passed, 0 failed** (`cargo test --release`) and **36** for `zellij::`, up from
> 774 / 29 as later tests landed. The macOS figures were produced against the
> rsync'd tree of that session and have not been re-measured here — re-run the
> suite on `imac` before quoting them.

## Result — green on both platforms

| | Linux | macOS (imac) |
|---|---|---|
| `cargo build --release` | ✅ | ✅ |
| `cargo test --release` (default parallelism) | ✅ **774 passed, 0 failed** | ✅ **749 passed, 0 failed** |
| `--test-threads=1` | — | ✅ 0 failed |
| `--test-threads=4` | — | ✅ 749 passed, 0 failed (×3 runs) |
| `zellij::` (the pane-title work) | ✅ | ✅ 29 passed |

The macOS count is lower than Linux's because the platform-gated tests below
are compiled out there, not because anything is missing.

**Sync note:** the tree is copied with `rsync -azc … --delete --exclude target
--exclude .git --exclude .pir`. The `-c` (checksum) matters: a plain `-a` skips
a file whose mtime is newer on the destination, which silently tested a stale
`src/term.rs` and produced a macOS-only failure that did not exist locally.

## Second round — the macOS-only hangs and heap corruption (this session)

With the tree finally reaching imac intact, the *build* was green but the suite
was not. Four independent defects, all macOS-only, all fixed:

1. **`git worktree remove` does not exist in git 2.14** (`extensions/wt`).
   macOS 10.13 ships Apple Git 2.14.3, whose `worktree` subcommands are only
   `add/list/lock/prune/unlock`; any other subcommand prints the `worktree add`
   usage and exits 129. `remove_worktree` now falls back to removing the
   directory and running `git worktree prune`. (`create` uses `worktree add`,
   which 2.14 does have.)

2. **Mock HTTP servers read a `WouldBlock` socket immediately** (`src/provider`).
   `accept_within` set the *listener* non-blocking; on BSD/macOS the accepted
   socket **inherits `O_NONBLOCK`** (Linux does not), so every mock server's
   first `read` returned `EAGAIN` and it tore the connection down before the
   client could drain the body — surfacing as `curl error 56` / `WouldBlock` in
   several provider tests. `accept()` now clears non-blocking on the accepted
   stream.

3. **libcurl was never `curl_global_init`'d** (`vendor/lsb-curl`). libcurl is
   explicit that letting `curl_easy_init` perform the implicit global init is
   **not thread-safe**. `Curl::load` runs on many threads (one per request, far
   more under `cargo test`), so concurrent first-calls corrupted libcurl's
   global state — the heap then aborted with `pointer being realloc'd was not
   allocated` / `double free` / SIGSEGV. This was the intermittent
   crash-that-moved-around at default parallelism; it reproduced at
   `--test-threads=4` and up. Now guarded by a process-wide `Once`.

4. **Two tests mutated process-global state without isolating it.**
   - `provider::tests::debug_payload_dump_roundtrips` set `PIR_DEBUG_PAYLOAD`
     (read by the *real* request path in concurrent tests) and read the file
     back; split the write out as `dump_payload_to(path, body)` so the test
     needs no global, plus a small test that the env wrapper still resolves it.
   - `agent::goal_bootstrap_tests::taste_tool_records_reinforces_and_validates`
     ran in the repo's cwd, so it read and wrote the **real** project taste store
     (`<cwd>/.pir/taste/…`) and was not idempotent: a stray entry made the next
     run's first `remember` say "Reinforced" instead of "Recorded". It now runs
     in a throwaway cwd.

## What "make #cfg mac where appropriate" actually produced (first round)

Of the 21 original macOS failures, **6 were real bugs and 15 were tests that
were wrong off Linux.** Neither group is now failing.

### Real product bugs — fixed, not gated

1. **`pir` could not compile on macOS at all.** Three separate defects:
   - `staged_system_write_notice` was missing from the non-Linux overlay stub
     (`src/security.rs`), though `Agent::finish_turn` calls it every turn on
     every platform → `error[E0425]`.
   - `set_project_quarantine_engaged` / `set_system_quarantine_engaged` missing
     from that same stub, called by the policy tests' `OverlayEngaged` guard →
     the *test* build failed off Linux.
   - `libc::openpty`'s last two arguments are `*const` on glibc and `*mut` on
     BSD/macOS (`src/sixel.rs`), so `null()` compiles on Linux and is
     `error[E0308]` on macOS. Split into `cfg`-gated helpers.

2. **No HTTP on macOS.** `lsb-curl` only ever tried `libcurl.so.4`,
   `libcurl.so`, `libcurl-gnutls.so.4` — *none of which exist on macOS*, which
   ships `libcurl.4.dylib`. `Curl::load` therefore failed on every macOS host
   and every provider request errored with `lsb-curl backend unavailable`. Now
   platform-gated in `vendor/lsb-curl/src/lib.rs::candidates()`. This alone
   fixed 6 of the 21 failures; `lsb-curl`'s own 14 tests (including real HTTP)
   now pass on macOS.

3. **The `pkill -u <me>` safety guard did not fire for real macOS users.**
   `login_name_for_uid` read only `/etc/passwd`, which on macOS is a ~100-line
   stub of classic system accounts that **omits logged-in users** (a uid-501
   login has no line). The guard silently failed for exactly the users it exists
   to protect. Now falls back to `getpwuid(3)` (Directory Service).

4. **Two tests compared paths against a non-canonical prefix.** On macOS
   `temp_dir()` is `/tmp`, a symlink to `/private/tmp`, so
   `context_files(&deep)` (which canonicalizes) returned `/private/tmp/…` while
   the fixture's prefix was `/tmp/…` — the filter matched nothing and the test
   saw zero files. Fixed by canonicalizing the fixture root *after* creating it
   (`src/agent.rs`, `extensions/builtin/src/lib.rs::tmpdir`). Same class of bug
   in two places.

### Tests that are only meaningful on Linux — gated, with the cause recorded

| test | why |
|---|---|
| `resource::live_probe::report_this_host` | reads `/proc/meminfo` + `/sys/fs/cgroup`; neither exists off Linux |
| `security::mitigate::tests::redirect_onto_existing_adds_journal_copy` | asserts `cp -a --reflink=auto`; `--reflink` is GNU-only (BSD `cp`: `illegal option`) |
| `lints::tests::the_gcc_fallback_command_actually_reports_a_real_bug` | needs GCC's `-fanalyzer`; macOS `/usr/bin/gcc` is Apple clang (`unknown argument: '-fanalyzer'`) |
| `ext_tests::…::merge_lock_is_exclusive` | spawns the `flock` *command* (util-linux); macOS has `flock(2)` but no such binary |
| 5 × `agent::{goal_bootstrap,real_turn_nudge}_tests` | call the real `load_providers()`, which needs a configured model catalog; the iMac has none, so `fake/slow` is absent and `select()` fails for reasons unrelated to what is under test |

### Tests skipped on a missing tool rather than gated by OS

`grep_finds_content_and_skips_ignored_and_deps` and `grep_modes_and_flags` now
`return` early when ripgrep is absent. They assert that a `.gitignore`d file is
excluded — but `fallback_grep` is *documented* as having no gitignore awareness
("Slower and without gitignore awareness"), so on the iMac (no `rg`) returning
`ignored.txt` is correct behaviour, not a regression. A capability check is the
honest gate here; an OS check would wrongly exclude a Linux box without ripgrep.

## Still open

`vendors/rustxWidgets/rustxwidgets/Cargo.toml` hardcodes absolute **Linux**
paths (`/home/ai_pir/…`, `/root/tmp/gtk4-rs/…`). Cargo resolves optional path
deps eagerly, so this breaks resolution even though `gui`/`pancurses` are off and
the default build never links them; the `gtk4-rs` paths are not in git at all.
The local Linux build works only because those paths exist on this host.

**Not fixed** — it needs a decision: make them relative (as that file's own
comment argues), or keep `rustxwidgets` from being resolved unless the feature is
requested. For the macOS runs they were repointed in that copy only.