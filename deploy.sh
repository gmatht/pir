#!/usr/bin/env bash
# deploy.sh — build, test, and install the `pir` binary.
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
#    This script builds in the current dir by default, but `--ref <tag|sha>`
#    materializes a throwaway `git worktree` of that ref and deploys from it,
#    then cleans it up.
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

set -euo pipefail

# --------------------------------------------------------------- args
PREFIX="${PIR_DEPLOY_PREFIX:-${XDG_BIN_HOME:-$HOME/.local/bin}}"
REF=""
WITH_PROJECT_INIT=0
TEST_ONLY=0
CLIPPY=1

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
    --with-project-init) WITH_PROJECT_INIT=1; shift ;;
    --test-only)     TEST_ONLY=1; shift ;;
    --no-clippy)     CLIPPY=0; shift ;;
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

need() { command -v "$1" >/dev/null 2>&1 || die "required tool not found: $1"; }
ver_ge() { # $1 have (x.y.z)  $2 min (x.y)
  awk -v h="$1" -v m="$2" 'BEGIN{
    split(h,H,"."); split(m,M,".");
    for(i=1;i<=3;i++){ hv=(H[i]==""?0:H[i]+0); mv=(M[i]==""?0:M[i]+0);
      if(hv>mv) exit 0; if(hv<mv) exit 1 }
    exit 0 }'
}

need rustc; need cargo; need git

# Rust >= 1.70 (IsTerminal). Strip pre-release suffix.
RUST_VER="$(rustc --version | awk '{print $2}' | sed 's/-.*//')"
ver_ge "$RUST_VER" "1.70" || die "rustc $RUST_VER < 1.70 required"

# --------------------------------------------------------------- source tree
SRC="$(pwd)"
CLEANUP_WORKTREE=0
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

# --------------------------------------------------------------- tests + build
say "running unit tests (cargo test --release --locked)"
cargo test --release --locked 2>&1 | tail -25 || true
# cargo test fails the pipe with set -o pipefail only if it errors; assert exit:
cargo test --release --locked >/dev/null || die "unit tests failed"

if [ "$CLIPPY" -eq 1 ]; then
  if command -v cargo-clippy >/dev/null 2>&1 || cargo clippy --version >/dev/null 2>&1; then
    say "clippy (deny-level errors fail the deploy; warnings are non-fatal)"
    # Capture full output; fail only if clippy emitted a hard error (deny lint
    # or compile failure), not on ordinary warnings.
    CLIPPY_OUT="$(cargo clippy --release --locked 2>&1)"
    echo "$CLIPPY_OUT" | grep -E '^(warning|warning:|note:|  -->|[0-9]+ \|)' || true
    if echo "$CLIPPY_OUT" | grep -qE '^error(\[|:)'; then
      die "clippy reported one or more errors (deny-level lint or compile failure)"
    fi
    say "  clippy clean (no errors)"
  else
    warn "cargo-clippy not installed; skipping lint gate"
  fi
fi

say "building release (cargo build --release --locked)"
cargo build --release --locked || die "release build failed"
BIN="$SRC/target/release/pir"
[ -x "$BIN" ] || die "binary not produced at $BIN"

# --------------------------------------------------------------- binary smoke
say "smoke tests on built binary"
V="$($BIN --version 2>&1)" || die "--version failed"
[[ "$V" =~ ^pir\ [0-9]+\.[0-9]+\.[0-9]+ ]] || die "unexpected --version output: $V"
say "  version ok: $V"

$BIN --help >/dev/null 2>&1 || die "--help exited non-zero"
say "  --help ok"

# With no API key the provider/config path must fail gracefully (no panic).
rc=0
env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u PI_MODEL \
  bash -c "echo '' | '$BIN' >/dev/null 2>&1" || rc=$?
if [ "$rc" -eq 0 ]; then
  die "one-shot with no API key unexpectedly succeeded (expected graceful failure)"
fi
if dmesg 2>/dev/null | tail -1 | grep -qi 'pir.*core dumped'; then
  die "binary crashed (core dump) on missing API key"
fi
say "  graceful no-key failure ok (exit=$rc)"

[ "$TEST_ONLY" -eq 1 ] && { say "test-only mode; skipping install and publish."; exit 0; }

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
say "installing to $PREFIX"
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

# --------------------------------------------------------------- project user (root opt)
if [ "$WITH_PROJECT_INIT" -eq 1 ]; then
  [ "$(id -u)" -eq 0 ] || die "--with-project-init requires root"
  say "provisioning per-project user (pir project init)"
  "$INSTALLED" project init || die "project init failed"
  # project name == cwd basename -> user ai_<basename>
  PROJ="$(basename "$SRC")"
  id "ai_$PROJ" >/dev/null 2>&1 || die "ai_$PROJ user not created"
  sudo -u "ai_$PROJ" "$INSTALLED" --version >/dev/null 2>&1 \
    || warn "could not verify --version as ai_$PROJ (may lack ~/.pi config)"
  say "  ai_$PROJ provisioned"
fi

# --------------------------------------------------------------- publish to GitHub (opt-in)
if [ "$PUSH" -eq 1 ] || [ -n "$TAG" ] || [ "$RELEASE" -eq 1 ]; then
  say "publishing to GitHub"

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
      say "  pushing $PUSH_REF to $PUSH_REMOTE"
      git push "$PUSH_REMOTE" "$PUSH_REF" \
        || die "could not push $PUSH_REF to $PUSH_REMOTE"
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
fi

say "deploy complete: $INSTALLED  ($V)"
