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
#      * Build reproducibility: `cargo build --release`(lockfile must
#        commitied if it would need to change).
#      * Unit tests: `cargo test --release` (e.g. goal.rs parsing,
#        next-step selection, goal persistence).
#      * Lint gate (errors fail, warnings warn): `cargo clippy` —
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
#    USE: '/mnt/c/Program Files/WSL/wsl.exe' -d AlmaLinux8

set -euo pipefail

# --------------------------------------------------------------- WSL build (portable glibc)
# Run cargo/rustc inside a WSL distro so the release binary targets an older
# glibc (AlmaLinux8 -> glibc 2.28) and runs on any modern distro. wsl.exe
# emits a stray NUL byte on stdout (a Windows pty quirk); the wrappers strip
# it so command substitution and the build logs stay clean, while preserving
# the real cargo/rustc exit code (via PIPESTATUS). Engaged only when wsl.exe
# is executable AND the distro is actually registered (tolerating the leading
# "*" default-distro marker in `wsl -l -v`).
WSL_EXEC='/mnt/c/Program Files/WSL/wsl.exe'
WSL_DISTRO="${WSL_DISTRO:-AlmaLinux8}"
_use_wsl=0
if [ "${PIR_NO_WSL:-0}" != "1" ] && [ -x "$WSL_EXEC" ]; then
  if "$WSL_EXEC" -l -v 2>/dev/null | awk -v d="$WSL_DISTRO" '
       NR>1 { for (i=1;i<=NF;i++) if (tolower($i)==tolower(d)) f=1 } END{exit f?0:1}'; then
    _use_wsl=1
  fi
fi

if [ "$_use_wsl" -eq 1 ]; then
  # Build inside the WSL distro; working dir is shared/mounted. Strip the NUL
  # that wsl.exe injects, and return cargo/rustc's true exit status.
  cargo() { "$WSL_EXEC" -d "$WSL_DISTRO" cargo "$@" 2>&1 | tr -d '\0'; return "${PIPESTATUS[0]}"; }
  rustc()  { "$WSL_EXEC" -d "$WSL_DISTRO" rustc "$@" 2>&1 | tr -d '\0'; return "${PIPESTATUS[0]}"; }
  say "using WSL distro '$WSL_DISTRO' for cargo/rustc builds"
else
  cargo() { command cargo "$@"; }
  rustc()  { command rustc "$@"; }
fi


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
_STEP_TOTAL=8

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

# --------------------------------------------------------------- external path deps
# (None currently: rustxWidgets was removed from this repo and is an optional,
# out-of-tree GUI dependency. See Cargo.toml. The default `pir` binary has no
# `path =` deps, so there is nothing to materialize here.)
step "external path dependencies"
step_done "external path dependencies materialized (none required)"

# --------------------------------------------------------------- tests + build
if [ "$TESTS" -eq 1 ]; then
  step "run unit tests (cargo test --release)"
  # Capture to a log file (not a live `| tail` pipe) so the run can't be
  # starved by an unrelated process holding the pipeline's write end open, and
  # so the output survives for debugging. We tail the file afterwards.
  _TLOG="$(mktemp "${TMPDIR:-/tmp}/pir-deploy-test.XXXXXX.log")"
  cargo test --release >"$_TLOG" 2>&1 || true
  dbg "cargo test log: $_TLOG ($(wc -l < "$_TLOG") lines)"
  tail -25 "$_TLOG"
  # cargo test fails the pipe with set -o pipefail only if it errors; assert exit:
  cargo test --release >/dev/null || die "unit tests failed"
  step_done "unit tests passed"
else
  say "skipping unit tests (--no-tests / --fast)"
fi

if [ "$CLIPPY" -eq 1 ]; then
  if command -v cargo-clippy >/dev/null 2>&1 || cargo clippy --version >/dev/null 2>&1; then
    step "lint gate (cargo clippy; deny-level errors fail)"
    # Capture full output; fail only if clippy emitted a hard error (deny lint
    # or compile failure), not on ordinary warnings.
    CLIPPY_OUT="$(cargo clippy --release 2>&1)"
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

step "build release (cargo build --release)"
cargo build --release || die "release build failed"
BIN="$SRC/target/release/pir"
[ -x "$BIN" ] || die "binary not produced at $BIN"
dbg "BIN=$BIN  size=$(stat -c%s "$BIN" 2>/dev/null || echo '?') bytes"
step_done "release build at $BIN"

# --------------------------------------------------------------- binary smoke
step "smoke tests on built binary"
V="$($BIN --version 2>&1)" || die "--version failed"
[[ "$V" =~ ^pir\ [0-9]+\.[0-9]+\.[0-9]+ ]] || die "unexpected --version output: $V"
say "  version ok: $V"

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


jj commit Cargo.lock -m "automatic Cargo.lock update" ||
	git commit Cargo.lock -m "automatic Cargo.lock update"


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
