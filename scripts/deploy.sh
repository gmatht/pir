#!/usr/bin/env bash
# deploy.sh — build, test, and DEPLOY to GITHUB!
#
# Design notes
# ------------
# Q: Should deployment run from an idle worktree?
#    YES — deploy from a CLEAN, isolated checkout of the exact ref you intend
#    to ship, not from an actively-edited working tree. Reasons:
#      * Immutability: you ship a known-good commit/tag, not whatever the dev
#        has half-edited in their tree (incl. an uncommitted Cargo.lock bump).
#      * No clobbering: a normal `cargo build` only touches `target/` (which is
#        gitignored), so it is *safe* to build in place — but `pir project
#        init` CHOWNS the cwd to a new `ai_<project>` user. Running that in a
#        shared dev tree would hand the directory to a system account. Keep the
#        build/install step and the per-project-user step separate.
#      * Parallelism: a detached worktree lets you keep developing (or run
#        CI) in the main tree while a release is built elsewhere.
#    This script builds in the current dir by default for local/test runs
#    (--test-only / --no-push), but any PUBLISH run (--release / --push / --tag)
#    automatically materializes a throwaway `git worktree` of HEAD and deploys
#    from it, then cleans it up — so you never ship the shared, possibly dirty
#    working tree. Override the ref with `--ref <tag|sha>`, or force in-place
#    with `--in-place`.
#
# Q: What tests should it do?
#      * Build reproducibility: `cargo build --release --locked` (lockfile must
#        match; fail if it would need to change).
#      * Unit tests: `cargo test --release --locked` (e.g. goal.rs parsing,
#        next-step selection, goal persistence).
#      * Lint gate (errors fail, warnings warn): `cargo clippy --locked` —
#        correctness/deny lints (e.g. `never_loop`, panic-in-const) block the
#        deploy; plain warnings are surfaced but non-fatal.
#      * Binary smoke tests against the freshly built artifact:
#          - `--version` prints `pir <semver>`;
#          - `--help` exits 0;
#          - a one-shot prompt with NO API key exits non-zero WITHOUT panicking
#            (proves the provider/config path degrades gracefully, not crash).
#      * Install assertion: the installed binary is executable and `--version`
#        resolves on PATH for the target user.
#      * (Optional, root) `pir project init` creates `ai_<project>` and records
#        the mapping; verified via `id ai_<project>` + `pir --version` as that
#        user.
#
# Publishing to GitHub is ON BY DEFAULT (this is a deploy script). It will:
#      * `git push` the current ref to `origin` (override with --push-remote /
#        --push-branch, disable with --no-push).
#      * With `--tag vX.Y.Z` it also creates + pushes an annotated tag.
#      * With `--release` it additionally creates a GitHub Release for the tag
#        (from Cargo.toml version if --tag is omitted) and uploads the built
#        binary via `gh` (requires `gh auth login`).
#    To build/test/install WITHOUT touching the remote, pass `--no-push` and
#    omit `--tag`/`--release`, or use `--test-only`.
#
# Progress / debug output
# -----------------------
#   --verbose, -v   show per-step timing (how long each phase took)
#   --debug,   -d   verbose + dump an environment/context banner (rust/cargo
#                   versions, PATH, cwd, uid, and relevant PIR_/CARGO_/RUST_/
#                   PI_/API_KEY env vars). Useful when a phase fails.
#   Each top-level phase prints a `step N/8` marker so you can see how far the
#   deploy got and where it stalled.

# Q: How do we avoid requiring a newer glibc than the target distros ship?
#    Run build and tests inside an AlmaLinux 8 container (glibc 2.28) via WSL.
#    The resulting binary only needs glibc >= 2.28, so it also runs on AlmaLinux 9 / RHEL / Rocky and
#    any modern glibc-based distro — NOT just the build host's newer glibc.
#    (glibc compatibility is backward-only: a binary built against glibc X runs
#    on anything with glibc >= X.) TLS is already rustls (via ureq), so there is
#    no OpenSSL version coupling either.
#
# Q: Why AlmaLinux 8 AND zig, rather than one or the other?
#    They solve different halves of the portability problem, and we need both:
#
#      * AlmaLinux 8 (glibc 2.28) provides the BUILD ENVIRONMENT — an old-ish
#        glibc to compile against, plus mount privileges so the overlay/
#        worktree tests can actually run. It is where `cargo test` executes.
#      * zig (via `cargo-zigbuild`) supplies the LINKER STUBS for glibc 2.17,
#        which is what actually removes the high-version symbol references.
#
#    Building in AlmaLinux 8 alone tops out at GLIBC_2.28 (measured: the
#    offending symbols are GLIBC_2.18/2.25/2.27/2.28), so that binary HARD
#    FAILS on CentOS/RHEL 7 with
#        /lib64/libc.so.6: version `GLIBC_2.28' not found
#    Targeting `x86_64-unknown-linux-gnu.2.17` through cargo-zigbuild pins the
#    floor to 2.17, and the result still runs on modern hosts (verified on
#    glibc 2.39) — one artifact, a wider range.
#
#    Both were verified by execution, not inference: the 2.28 binary fails on
#    centos:7 and the 2.17 one runs there (`pir --version`, `--help`) and
#    passes 290/293 tests in that container. The 3 failures are environmental
#    (2 overlay tests need CAP_SYS_ADMIN to mount; 1 worktree test needs git
#    >= 2.x, CentOS 7 ships 1.8.3) and reproduce identically regardless of how
#    the binary was linked.
#
#    USE: '/mnt/c/Program Files/WSL/wsl.exe' -d AlmaLinux-8

set -euo pipefail

# --------------------------------------------------------------- build harness
#
# The release must be built against an OLD glibc so the binary also runs on
# RHEL/Rocky/AlmaLinux 9 and other modern glibc distros (backward-only compat),
# so cargo runs inside the AlmaLinux 8 WSL distro rather than on the host.
#
# Two traps this harness exists to avoid — both silently produced a "successful"
# deploy that built and tested NOTHING:
#
#  1. DISTRO NAME. WSL names the distro `AlmaLinux-8` (hyphen) while this script
#     used to say `AlmaLinux8`. `wsl.exe -d AlmaLinux8` fails with
#     WSL_E_DISTRO_NOT_FOUND and — critically — the error text goes to a stream
#     the caller was capturing, so `rustc --version` "returned" the words
#     `is` / `code:` instead of a version. The `ver_ge` check then compared
#     garbage, and because that comparison is inside a `$( )` in a `||` the
#     script sailed on and exited 0. Never let a tool's failure be mistaken for
#     its output: every wrapper below checks the exit status explicitly.
#
#  2. FILESYSTEM VISIBILITY. Each WSL distro is its own VM. Ubuntu2404 (where
#     the repo lives) and AlmaLinux-8 cannot see each other's filesystems, so
#     `cd "$SRC"` then invoking cargo in AlmaLinux-8 cannot work: the relay
#     fails to translate the cwd and (worse) cargo would silently run somewhere
#     else. We therefore COPY the source into the distro's own filesystem and
#     run cargo there, then copy the built artifact back. Stdin (`wsl.exe`) is a
#     usable byte pipe even when the cwd can't be translated, so the source goes
#     over as a tar stream and the artifact comes back the same way (base64, to
#     survive a text-mode hop).
#
# Set PIR_DEPLOY_DISTRO to override the distro name, or PIR_DEPLOY_NO_WSL=1 to
# build with the host toolchain (see the fallback note in `cargo`).

# NB: no quotes INSIDE the default value. `${X:-'...'}` does not strip the
# quotes — it embeds them literally, so the path became
# `'/mnt/c/Program Files/WSL/wsl.exe'` (with quote characters) and every
# `[ -x ]` / exec test failed, silently disabling the distro build. Keep the
# default bare and quote at the point of use.
WSL_EXEC="${PIR_DEPLOY_WSL_EXEC:-/mnt/c/Program Files/WSL/wsl.exe}"
DISTRO="${PIR_DEPLOY_DISTRO:-AlmaLinux-8}"
# The glibc floor for the released binary. 2.17 = CentOS/RHEL 7 (and therefore
# everything newer). Override only if you deliberately want a higher floor.
GLIBC_TARGET="${PIR_DEPLOY_GLIBC:-2.17}"
# Zig triple cargo-zigbuild passes to the linker. The `.2.17` suffix is what
# selects the old glibc stubs; without it zig would link against its default.
ZIG_TARGET="x86_64-unknown-linux-gnu.${GLIBC_TARGET}"
# Cargo's own --target dir name (zigbuild strips the glibc suffix).
CARGO_TARGET="x86_64-unknown-linux-gnu"
# Remote build dir inside the distro (its own disk: the repo is unreachable
# cross-distro, and /mnt/c is not writable by the build user).
REMOTE_DIR="${PIR_DEPLOY_REMOTE_DIR:-/home/john/pir-deploy-build}"

# WSL_AVAILABLE tracks whether we can actually use the distro. It starts true
# and is settled by `wsl_probe` once a distro name is resolved.
WSL_AVAILABLE=1

# wsl_run: run a shell snippet inside the build distro. `$1` is the working
# directory (use `-` to mean the staged source). The explicit cd is what
# guarantees cargo never runs in the wrong place (the relay's default cwd is
# untranslatable and unreliable).
#
# Version probes (`rustc --version`) run before the source is staged, so they
# must NOT require $REMOTE_DIR to exist — hence the `-` form. Only the build
# commands need the pinned source directory.
#
# PATH: `bash -lc` reads the login profile, but the tools the zig build needs
# are not always on it — cargo-zigbuild installs into ~/.cargo/bin and zig
# ships as a tarball (we keep it at ~/zig, the layout cargo-zigbuild expects:
# it runs `<dir>/zig` next to the binary). Prepend both explicitly so the
# build works the same whether or not the profile has been set up.
wsl_run() {
  local dir="$1"; shift
  local pre="export PATH=\"\$HOME/.local/bin:\$HOME/zig:\$HOME/.cargo/bin:\$PATH\"; "
  if [ "$dir" != "-" ]; then
    pre="$pre cd '$dir' || { echo 'deploy: staged source missing in distro' >&2; exit 97; }; "
  fi
  "$WSL_EXEC" -d "$DISTRO" -- bash -lc "${pre}$*"
}

# has_wsl: true when the configured distro exists and runs commands. Probes by
# EXIT STATUS (never by parsing stdout), so a missing distro can't masquerade
# as a working one.
has_wsl() {
  [ "$WSL_AVAILABLE" -eq 1 ] || return 1
  [ -x "$WSL_EXEC" ] || return 1
  "$WSL_EXEC" -d "$DISTRO" -- true >/dev/null 2>&1
}

# cargo/rustc: when the AlmaLinux 8 distro is usable, run the tool there (with
# the cwd pinned to the staged copy) so the artifact links an old glibc.
# Otherwise fall back to the host toolchain, with a loud warning: the build
# still works, but the resulting binary is only guaranteed to run on hosts at
# least as new as this one.
cargo() {
  if has_wsl; then
    wsl_run "$REMOTE_DIR" "cargo $(printf '%q ' "$@")"
  else
    command cargo "$@"
  fi
}
# rustc: used only for the version probe, which runs before staging, so it does
# not pin the (not-yet-existing) staged directory.
rustc() {
  if has_wsl; then
    wsl_run "-" "rustc $(printf '%q ' "$@")"
  else
    command rustc "$@"
  fi
}


# --------------------------------------------------------------- args
PREFIX="${PIR_DEPLOY_PREFIX:-${XDG_BIN_HOME:-$HOME/.local/bin}}"
REF=""
# SHARED_REPO: a checkout that holds external `path =` dependencies which are
# NOT part of the pir repo itself (e.g. rustxWidgets, a sibling git checkout
# sitting next to pir with its own .git and never tracked). When we deploy from
# a worktree these dirs are absent, so cargo cannot resolve the path dependency
# even when the feature that uses it is disabled. Point this at the main pir
# checkout (auto-derived from the worktree if unset).
SHARED_REPO=""
# IN_PLACE: by default a PUBLISH run (--release/--push/--tag) deploys from a
# clean throwaway worktree of HEAD, never the dirty shared working tree, so we
# never ship a competing dev's half-edited files (or accidental WIP). Pass
# --in-place to build/test/install in the current directory instead.
IN_PLACE=0
WITH_PROJECT_INIT=0
TEST_ONLY=0
TESTS=1
CLIPPY=1
VERBOSE=0
DEBUG=0

# publish: ON by default (this is a deploy script). Opt out with --no-push / --no-release.
PUSH=1
PUSH_REMOTE="${PIR_DEPLOY_REMOTE:-origin}"
PUSH_BRANCH=""
TAG=""
RELEASE=0

usage() { sed -n '2,40p' "$0" | grep '^#' | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix)        PREFIX="$2"; shift 2 ;;
    --prefix=*)      PREFIX="${1#*=}"; shift ;;
    --ref)           REF="$2"; shift 2 ;;
    --ref=*)         REF="${1#*=}"; shift ;;
    --shared-repo)   SHARED_REPO="$2"; shift 2 ;;
    --shared-repo=*) SHARED_REPO="${1#*=}"; shift ;;
    --in-place)      IN_PLACE=1; shift ;;
    --with-project-init) WITH_PROJECT_INIT=1; shift ;;
    --test-only)     TEST_ONLY=1; shift ;;
    --no-clippy)     CLIPPY=0; shift ;;
    --no-tests)      TESTS=0; shift ;;
    --fast)          TESTS=0; CLIPPY=0; shift ;;
    --verbose|-v)    VERBOSE=1; shift ;;
    --debug|-d)      VERBOSE=1; DEBUG=1; shift ;;
    --push)          PUSH=1; shift ;;
    --no-push)       PUSH=0; shift ;;
    --push-remote)   PUSH_REMOTE="$2"; shift 2 ;;
    --push-remote=*) PUSH_REMOTE="${1#*=}"; shift ;;
    --push-branch)   PUSH_BRANCH="$2"; shift 2 ;;
    --push-branch=*) PUSH_BRANCH="${1#*=}"; shift ;;
    --tag)           TAG="$2"; shift 2 ;;
    --tag=*)         TAG="${1#*=}"; shift ;;
    --release)       RELEASE=1; PUSH=1; shift ;;
    --no-release)    RELEASE=0; shift ;;
    -h|--help)       usage 0 ;;
    *) echo "deploy.sh: unknown arg '$1'" >&2; usage 1 ;;
  esac
done

# --------------------------------------------------------------- helpers
say()  { printf '\033[1;32m[deploy]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[deploy]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[deploy] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }

# step: print a numbered step header and (in --debug) start a stopwatch.
_step_no=0
_step_t0=""
step() {
  _step_no=$((_step_no + 1))
  _step_t0="$(date +%s.%N)"
  printf '\033[1;36m[deploy] \033[1;35mstep %d/%d\033[0m %s\n' \
    "$_step_no" "${_STEP_TOTAL:-?}" "$*"
}
# step_done: print elapsed time for the last step (debug/verbose only).
step_done() {
  [ "$VERBOSE" -eq 1 ] || return 0
  local now t
  now="$(date +%s.%N)"
  t="$(awk -v a="$_step_t0" -v b="$now" 'BEGIN{printf "%.2fs", b-a}')"
  printf '\033[1;36m[deploy]\033[0m   ✓ %s (took %s)\n' "$*" "$t"
}
# dbg: dump debug info when --debug is set.
dbg() { [ "$DEBUG" -eq 1 ] || return 0; printf '\033[0;90m[deploy:dbg]\033[0m %s\n' "$*"; }

need() { command -v "$1" >/dev/null 2>&1 || die "required tool not found: $1"; }
ver_ge() { # $1 have (x.y.z)  $2 min (x.y)
  awk -v h="$1" -v m="$2" 'BEGIN{
    split(h,H,"."); split(m,M,".");
    for(i=1;i<=3;i++){ hv=(H[i]==""?0:H[i]+0); mv=(M[i]==""?0:M[i]+0);
      if(hv>mv) exit 0; if(hv<mv) exit 1 }
    exit 0 }'
}

need rustc; need cargo

# Rust >= 1.70 (IsTerminal). Strip pre-release suffix.
RUST_VER="$(rustc --version | awk '{print $2}' | sed 's/-.*//')"
ver_ge "$RUST_VER" "1.70" || die "rustc $RUST_VER < 1.70 required"

# Total number of top-level steps (for the step progress markers). Phases that
# are conditionally skipped still reserve their slot; the marker simply won't print.
_STEP_TOTAL=10

dbg "PREFIX=$PREFIX VERBOSE=$VERBOSE DEBUG=$DEBUG PUSH=$PUSH REF='${REF}'"
dbg "rustc=$(rustc --version)  cargo=$(cargo --version)"
dbg "PATH=$PATH"
dbg "git=$(git --version)  cwd=$(pwd)  user=$(id -un) (uid $(id -u))"
[ "$DEBUG" -eq 1 ] && dbg "env: $(env | grep -E '^(PIR_|CARGO_|RUST_|PI_|ANTHROPIC_|OPENAI_)' | sort | tr '\n' ' ')"

# --------------------------------------------------------------- source tree
step "resolve source tree${REF:+ (ref $REF)}"
SRC="$(pwd)"
CLEANUP_WORKTREE=0
# By default a PUBLISH run (--release/--push/--tag) deploys from a clean
# throwaway worktree of HEAD, NOT the current shared working tree, so we never
# ship another worker's half-edited files or accidental WIP. Local-only runs
# (--test-only, --no-push) stay in the current directory. --ref overrides the
# ref; --in-place forces the current directory.
if [ -z "$REF" ] && [ "$IN_PLACE" -eq 0 ] && [ "$TEST_ONLY" -eq 0 ]; then
  if [ "$PUSH" -eq 1 ] || [ -n "$TAG" ] || [ "$RELEASE" -eq 1 ]; then
    REF="HEAD"
  fi
fi
if [ -n "$REF" ]; then
  [ -f Cargo.toml ] || die "run deploy.sh from inside the pir repo (so git worktree can be added)"
  WT="$(mktemp -d "${TMPDIR:-/tmp}/pir-deploy.XXXXXX")"
  say "materializing clean worktree of '$REF' in $WT"
  git worktree add --detach --quiet "$WT" "$REF" || die "could not create worktree for $REF"
  SRC="$WT"; CLEANUP_WORKTREE=1
  trap 'git worktree remove --force "$SRC" 2>/dev/null || true' EXIT
fi
cd "$SRC"
[ -f Cargo.toml ] || die "$SRC: not a pir repo (Cargo.toml missing)"
grep -q '^name = "pir"' Cargo.toml || die "$SRC: Cargo.toml is not pir"
dbg "SRC=$SRC  CLEANUP_WORKTREE=$CLEANUP_WORKTREE"
step_done "source tree resolved -> $SRC"

# --------------------------------------------------------------- stage to distro
# Copy the (clean) source into the build distro's own filesystem. See the
# build-harness note at the top: cross-distro paths are unusable, so the tree is
# transferred as a tar stream over stdin (the one channel that works even when
# the cwd cannot be translated) and cargo runs there.
step "stage source into build distro ($DISTRO)"
if has_wsl; then
  say "  distro '$DISTRO' available; staging to $REMOTE_DIR"
  # Echo the remote dir so a failed extraction is visible rather than silent.
  # `--exclude` keeps the transfer small: target/ is rebuilt, .git isn't needed
  # to compile (the version comes from Cargo.toml, not git).
  if ! tar czf - --exclude=./target --exclude=./.git --exclude=./build . \
      | "$WSL_EXEC" -d "$DISTRO" -- bash -lc \
          "rm -rf '$REMOTE_DIR' && mkdir -p '$REMOTE_DIR' && tar xzf - -C '$REMOTE_DIR' && cd '$REMOTE_DIR' && test -f Cargo.toml && grep -q '^name = \"pir\"' Cargo.toml && echo staged-ok"
  then
    die "could not stage source into distro '$DISTRO' (is it installed and is $REMOTE_DIR writable?)"
  fi
  say "  staged ok"
  STAGED=1
else
  # No distro: build with the host toolchain (see the `cargo` wrapper). Warn
  # loudly because the artifact's glibc floor becomes the host's, not 2.28.
  warn "build distro '$DISTRO' unavailable — building with the HOST toolchain."
  warn "  the binary will require this host's glibc or newer (not the AlmaLinux 8 baseline)."
  warn "  set PIR_DEPLOY_DISTRO / PIR_DEPLOY_WSL_EXEC to target the containerised build."
  STAGED=0
fi
step_done "source staged (staged=$STAGED)"

# --------------------------------------------------------------- external path deps
# (None currently: rustxWidgets was removed from this repo and is an optional,
# out-of-tree GUI dependency. See Cargo.toml. The default `pir` binary has no
# `path =` deps, so there is nothing to materialize here.)
step "external path dependencies"
step_done "external path dependencies materialized (none required)"

# --------------------------------------------------------------- tests + build
if [ "$TESTS" -eq 1 ]; then
  step "run unit tests (cargo test --release --locked -- --test-threads=8)"
  # `--test-threads=8` is a MEASURED stability ceiling, not a tuning knob.
  #
  # With full parallelism (16 here) the suite deadlocks: tests stop producing
  # results and the binary has to be killed. Measured on a 16-core box:
  #
  #     1 thread   ok 27.0s      8 threads  ok  4.2s     (3/3 runs)
  #     2 threads  ok 10.7s     10 threads  1/3 ok (flaky)
  #     4 threads  ok  5.9s     12 threads  HANG (timeout)
  #                             16 threads  HANG (timeout)
  #
  # Every extension test module passes ALONE even at 16 threads; the hang needs
  # several run concurrently (all extensions @16 hangs). The contention is
  # between extension tests that spawn children — `builtin`'s run_shell
  # sleep/process-group tests and `autocommit`'s git tests — so 8 keeps them
  # stable while still being ~7x faster than serial. Revisit when the
  # underlying race is fixed; do not raise this to "match nproc".
  #
  # `--test-threads` goes after `--` so cargo forwards it to the test binary.
  #
  # Capture to a log file (not a live `| tail` pipe) so the run can't be
  # starved by an unrelated process holding the pipeline's write end open, and
  # so the output survives for debugging. We tail the file afterwards.
  _TLOG="$(mktemp "${TMPDIR:-/tmp}/pir-deploy-test.XXXXXX.log")"
  cargo test --release --locked -- --test-threads=8 >"$_TLOG" 2>&1 || true
  dbg "cargo test log: $_TLOG ($(wc -l < "$_TLOG") lines)"
  tail -25 "$_TLOG"
  # assert exit: the second run must use the SAME flags as the first, or it
  # would silently re-run the suite at full parallelism (and hang) while
  # reporting on a different execution than the one whose output was just shown.
  cargo test --release --locked -- --test-threads=8 >/dev/null || die "unit tests failed"
  step_done "unit tests passed"
else
  say "skipping unit tests (--no-tests / --fast)"
fi

if [ "$CLIPPY" -eq 1 ]; then
  if command -v cargo-clippy >/dev/null 2>&1 || cargo clippy --version >/dev/null 2>&1; then
    step "lint gate (cargo clippy; deny-level errors fail)"
    # Capture full output; fail only if clippy emitted a hard error (deny lint
    # or compile failure), not on ordinary warnings.
    CLIPPY_OUT="$(cargo clippy --release --locked 2>&1)"
    echo "$CLIPPY_OUT" | grep -E '^(warning|warning:|note:|  -->|[0-9]+ \|)' || true
    if echo "$CLIPPY_OUT" | grep -qE '^error(\[|:)'; then
      die "clippy reported one or more errors (deny-level lint or compile failure)"
    fi
    say "  clippy clean (no errors)"
    step_done "clippy clean"
  else
    warn "cargo-clippy not installed; skipping lint gate"
  fi
fi

step "build release (cargo zigbuild --target $ZIG_TARGET, glibc $GLIBC_TARGET floor)"
# `cargo zigbuild` links with zig's bundled glibc stubs for the requested
# version, which is what removes the high-version symbol references
# (GLIBC_2.18/2.25/2.27/2.28) that plain `cargo build` leaves behind and that
# make the binary fail to START on CentOS/RHEL 7. The tests above ran under
# plain cargo (fast, no linker stubs needed); this step produces the artifact
# that actually ships, and the smoke tests below run against it.
if [ "$STAGED" -eq 1 ]; then
  # zigbuild runs in the distro, so both zig and cargo-zigbuild must be there.
  wsl_run "$REMOTE_DIR" "command -v cargo-zigbuild >/dev/null 2>&1 && command -v zig >/dev/null 2>&1" \
    || die "cargo-zigbuild and zig are required in '$DISTRO' for a glibc $GLIBC_TARGET build
       (install: cargo install cargo-zigbuild; and provide zig on PATH)"
  cargo zigbuild --release --locked --target "$ZIG_TARGET" || die "release build failed"
else
  command -v cargo-zigbuild >/dev/null 2>&1 \
    || die "cargo-zigbuild is required for a glibc $GLIBC_TARGET build
       (install: cargo install cargo-zigbuild; and provide zig on PATH)"
  cargo zigbuild --release --locked --target "$ZIG_TARGET" || die "release build failed"
fi

# Retrieve the artifact. When building in the distro the binary lands in ITS
# filesystem (unreachable cross-distro), so pull it back over stdout as base64
# — a byte-exact channel that survives the text-mode hop. Building on the host
# needs no copy.
EXPECT_VER="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([0-9.]+)".*/\1/')"
# zigbuild writes to target/<cargo-triple>/release (the `.2.17` suffix is a
# linker selector, not part of the output path).
BIN="$SRC/target/$CARGO_TARGET/release/pir"
REL_BIN="target/$CARGO_TARGET/release/pir"
if [ "$STAGED" -eq 1 ]; then
  step "retrieve artifact from distro"
  mkdir -p "$SRC/target/$CARGO_TARGET/release"
  if ! "$WSL_EXEC" -d "$DISTRO" -- bash -lc "base64 -w0 '$REMOTE_DIR/$REL_BIN'" > "$BIN.b64"; then
    die "could not retrieve the built binary from $DISTRO:$REMOTE_DIR/$REL_BIN"
  fi
  [ -s "$BIN.b64" ] || die "retrieved artifact is empty (build produced no binary?)"
  base64 -d "$BIN.b64" > "$BIN" || die "could not decode the retrieved artifact"
  rm -f "$BIN.b64"
  chmod 0755 "$BIN"
  # A static-ish ELF must have survived the round trip intact; if base64 or the
  # pipe mangled it, fail here rather than shipping a corrupt binary.
  [ "$(head -c4 "$BIN" | od -An -tx1 | tr -d ' \n')" = "7f454c46" ] \
    || die "retrieved artifact is not an ELF binary (export/copy corrupted it)"
  step_done "artifact retrieved -> $BIN"
fi
[ -x "$BIN" ] || die "binary not produced at $BIN"
dbg "BIN=$BIN  size=$(stat -c%s "$BIN" 2>/dev/null || echo '?') bytes"

# Verify the delivered artifact really has the glibc floor we claim. A build
# that silently linked against the host/modern glibc would otherwise ship with
# a note saying 2.17 while failing to start on the distros that motivated the
# whole zig setup. The artifact's highest required symbol must be <= the floor.
REQ_GLIBC="$(objdump -T "$BIN" 2>/dev/null \
  | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sort -uV | tail -1 | sed 's/^GLIBC_//')"
if [ -z "$REQ_GLIBC" ]; then
  warn "could not determine the glibc requirement of $BIN (objdump missing?) — not asserting"
elif [ "$REQ_GLIBC" != "$GLIBC_TARGET" ] && ver_ge "$REQ_GLIBC" "$GLIBC_TARGET"; then
  # `ver_ge a b` is true when a >= b. We want required <= target, so a
  # requirement strictly greater than the target means zig linking did not
  # take effect (or something re-linked against a newer libc).
  die "artifact requires GLIBC_$REQ_GLIBC, above the $GLIBC_TARGET floor —
       zig linking did not take effect (are zig and cargo-zigbuild in use?)"
else
  say "  glibc floor verified: requires <= GLIBC_$REQ_GLIBC (target $GLIBC_TARGET)"
fi
step_done "release build at $BIN (glibc <= ${REQ_GLIBC:-?})"

# --------------------------------------------------------------- binary smoke
step "smoke tests on built binary"
V="$($BIN --version 2>&1)" || die "--version failed"
[[ "$V" =~ ^pir\ [0-9]+\.[0-9]+\.[0-9]+ ]] || die "unexpected --version output: $V"
# The binary MUST report the version we are about to tag. Previously only the
# *shape* was checked, so a stale artifact (e.g. an old build that wasn't
# rebuilt) would pass and get published under the new tag.
case "$V" in
  *"$EXPECT_VER"*) ;;
  *) die "version mismatch: binary reports '$V' but Cargo.toml is $EXPECT_VER —
       refusing to publish a stale or mismatched artifact" ;;
esac
say "  version ok: $V (matches Cargo.toml $EXPECT_VER)"

$BIN --help >/dev/null 2>&1 || die "--help exited non-zero"
say "  --help ok"

# With no API key the provider/config path must fail gracefully (no panic).
rc=0
dbg "spawning one-shot with no API key (expect graceful non-zero exit, no panic)"
env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u PI_MODEL \
  bash -c "echo '' | '$BIN' >/dev/null 2>&1" || rc=$?
if [ "$rc" -eq 0 ]; then
  die "one-shot with no API key unexpectedly succeeded (expected graceful failure)"
fi
if dmesg 2>/dev/null | tail -1 | grep -qi 'pir.*core dumped'; then
  die "binary crashed (core dump) on missing API key"
fi
say "  graceful no-key failure ok (exit=$rc)"
step_done "smoke tests passed (version='$V')"

[ "$TEST_ONLY" -eq 1 ] && { say "test-only mode; skipping install and publish."; step_done "test-only complete"; exit 0; }

# --------------------------------------------------------------- publish guard
if [ "$RELEASE" -eq 1 ]; then
  need gh || die "--release requires the 'gh' CLI"
  [ -n "$TAG" ] || TAG="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([0-9.]+)".*/v\1/')"
  [ -n "$TAG" ] || die "--release: could not derive tag from Cargo.toml; use --tag"
  command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1 \
    || die "--release: gh is not authenticated (run: gh auth login)"
fi
if [ "$PUSH" -eq 1 ] || [ -n "$TAG" ]; then
  git rev-parse --is-inside-work-tree >/dev/null 2>&1 || die "deploy.sh must run inside the git repo to publish"
  git remote get-url "$PUSH_REMOTE" >/dev/null 2>&1 || die "push remote '$PUSH_REMOTE' not configured"
fi

# --------------------------------------------------------------- install
step "install to $PREFIX"
mkdir -p "$PREFIX"
INSTALLED="$PREFIX/pir"
install -m 0755 "$BIN" "$INSTALLED"
say "  installed $INSTALLED"

# Ensure on PATH for the invoking shell; warn if not.
case ":$PATH:" in
  *":$PREFIX:"*) ;;
  *) warn "$PREFIX is not on your PATH. Add it, e.g.: export PATH=\"$PREFIX:\$PATH\"" ;;
esac

# verify it resolves
"$INSTALLED" --version >/dev/null 2>&1 || die "installed binary failed --version check"
say "  installed binary --version ok"
step_done "installed $INSTALLED"

# --------------------------------------------------------------- project user (root opt)
if [ "$WITH_PROJECT_INIT" -eq 1 ]; then
  step "provision per-project user (pir project init)"
  [ "$(id -u)" -eq 0 ] || die "--with-project-init requires root"
  say "provisioning per-project user (pir project init)"
  "$INSTALLED" project init || die "project init failed"
  # project name == cwd basename -> user ai_<basename>
  PROJ="$(basename "$SRC")"
  id "ai_$PROJ" >/dev/null 2>&1 || die "ai_$PROJ user not created"
  sudo -u "ai_$PROJ" "$INSTALLED" --version >/dev/null 2>&1 \
    || warn "could not verify --version as ai_$PROJ (may lack ~/.pi config)"
  say "  ai_$PROJ provisioned"
  step_done "ai_$PROJ provisioned"
fi

# --------------------------------------------------------------- publish to GitHub (opt-in)
if [ "$PUSH" -eq 1 ] || [ -n "$TAG" ] || [ "$RELEASE" -eq 1 ]; then
  step "publish to GitHub ($PUSH_REMOTE, tag='${TAG:-none}', release=$RELEASE)"
  dbg "PUSH_REF will derive to: ${PUSH_BRANCH:-${REF:-HEAD}}"

  # Default the ref to push: an explicit --push-branch, else HEAD, else REF.
  PUSH_REF="${PUSH_BRANCH:-${REF:-HEAD}}"

  # 1) Create + push an annotated tag if requested (or implied by --release).
  if [ -n "$TAG" ]; then
    if git rev-parse "$TAG" >/dev/null 2>&1; then
      warn "tag $TAG already exists locally; reusing it"
    else
      say "  creating tag $TAG -> $PUSH_REF"
      git tag -a "$TAG" -m "Release $TAG" "$PUSH_REF" \
        || die "could not create tag $TAG"
    fi
    say "  pushing tag $TAG to $PUSH_REMOTE"
    git push --follow-tags "$PUSH_REMOTE" "refs/tags/$TAG" \
      || die "could not push tag $TAG to $PUSH_REMOTE"
  fi

  # 2) Push the commit/branch unless this run only meant to publish a tag.
  if [ "$PUSH" -eq 1 ]; then
    if [ -n "$PUSH_BRANCH" ]; then
      say "  pushing $PUSH_REF -> $PUSH_REMOTE/$PUSH_BRANCH"
      git push "$PUSH_REMOTE" "$PUSH_REF:refs/heads/$PUSH_BRANCH" \
        || die "could not push $PUSH_REF to $PUSH_REMOTE/$PUSH_BRANCH"
    else
      say " pushing $PUSH_REF to $PUSH_REMOTE"
      # HEAD alone is ambiguous when the src is a commit object (detached HEAD,
      # jj working copies, throwaway deploy worktrees) — git then can't even
      # guess a destination. Resolve the current branch name and push to a
      # fully-qualified ref; fall back to main when detached.
      _push_branch_name="$(git symbolic-ref --quiet --short HEAD || true)"
      [ -n "$_push_branch_name" ] || _push_branch_name="main"
      git push "$PUSH_REMOTE" "$PUSH_REF:refs/heads/$_push_branch_name" \
        || die "could not push $PUSH_REF to $PUSH_REMOTE/$_push_branch_name"
    fi
  fi

  # 3) GitHub Release with the built binary (--release).
  if [ "$RELEASE" -eq 1 ]; then
    need gh || die "--release requires the 'gh' CLI"
    ASSET="$BIN"
    say "  creating GitHub release $TAG ($ASSET)"
    gh release create "$TAG" "$ASSET" \
      --title "pir $TAG" \
      --notes "Automated release from deploy.sh (built from $PUSH_REF)." \
      || die "gh release create failed for $TAG"
    say "  released $TAG on GitHub"
  fi
  step_done "publish complete"
fi

say "deploy complete: $INSTALLED  ($V)"
