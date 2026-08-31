#!/usr/bin/env bash
# build-gui.sh — build the `pir` GUI (GTK / pancurses) binary inside a WSL
# "AlmaLinux 8" distro so the resulting artifact is linked against glibc 2.28
# and therefore runs unchanged on AlmaLinux / RHEL / Rocky 8+ and any modern
# glibc-based Linux.
#
# HOW WE DRIVE THE BUILD
# ----------------------
# We invoke the Windows WSL launcher and ask it to RUN commands in the distro:
#
#     '/mnt/c/Program Files/WSL/wsl.exe' -d '<distro>' <command...>
#
# IMPORTANT — flag case matters in WSL:
#   -d / --distribution <Distro>   -> RUN a command inside the distro  (what we want)
#   -D / --terminate    <Distro>   -> TERMINATE the distro, runs nothing
# So this script uses lowercase `-d`. (The task text said `-D AlmaLinux 8`; that
# would only shut the distro down, so it is intentionally corrected to `-d` here.)
# If your registered distro name really contains a space (e.g. "AlmaLinux 8")
# keep it quoted; this repo's deploy.sh instead used "AlmaLinux8" — set
# WSL_DISTRO accordingly to match whatever `wsl -l` reports on your machine.
#
# WHAT IT BUILDS
# --------------
# A Linux ELF binary (NOT a Windows .exe) named `pir`, with the requested
# feature(s): `gui` (GTK REPL via rustxWidgets' dlopen'd GTK backend) and/or
# `pancurses` (curses REPL). To actually *run* the GUI you need GTK3 present at
# runtime (see --with-gtk-runtime). The binary is portable to any glibc >= 2.28.
#
# The `gui`/`pancurses` features and the `rustxwidgets` optional dependency live
# commented out in Cargo.toml. This script temporarily enables them, builds, then
# restores Cargo.toml / Cargo.lock so your working tree stays clean (pass
# --keep-manifest to leave them enabled, e.g. if you intend to commit the GUI).
#
# WHERE IT BUILDS
# ----------------
# AlmaLinux-8 (launched via wsl.exe) can only see paths on a Windows-mounted
# drive (/mnt/c, /mnt/d, ...). If you run this script from a path the distro
# cannot read (e.g. the Ubuntu rootfs like /home/ai_pir/...), the script
# auto-stages a copy to a temp dir under /mnt/c and builds there, then copies
# the finished binary back to your original tree.
#
# USAGE (run from anywhere on the Windows side)
#     ./build-gui.sh                                  # build --features gui
#     ./build-gui.sh --features gui,pancurses         # also build pancurses REPL
#     ./build-gui.sh --with-gtk-runtime               # install GTK3 in the distro
#     ./build-gui.sh --distro AlmaLinux-8             # override the distro name
#     ./build-gui.sh --keep-manifest                  # leave Cargo.toml/Cargo.lock edited
#     ./build-gui.sh --out /tmp/pir-gui               # also copy the binary out
#
set -euo pipefail

# ----------------------------------------------------------- configuration
WSL_EXEC='/mnt/c/Program Files/WSL/wsl.exe'
WSL_DISTRO="${WSL_DISTRO:-AlmaLinux 8}"
WSL_USER=""
FEATURES="gui"
INSTALL_GTK_RUNTIME=0
KEEP_MANIFEST=0
OUT_DIR=""

while [ $# -gt 0 ]; do
  case "$1" in
    --distro)        WSL_DISTRO="$2"; shift 2 ;;
    --distro=*)      WSL_DISTRO="${1#*=}"; shift ;;
    --user)          WSL_USER="$2"; shift 2 ;;
    --user=*)        WSL_USER="${1#*=}"; shift ;;
    --features)      FEATURES="$2"; shift 2 ;;
    --features=*)    FEATURES="${1#*=}"; shift ;;
    --with-gtk-runtime) INSTALL_GTK_RUNTIME=1; shift ;;
    --keep-manifest) KEEP_MANIFEST=1; shift ;;
    --out)           OUT_DIR="$2"; shift 2 ;;
    --out=*)         OUT_DIR="${1#*=}"; shift ;;
    -h|--help)       sed -n '3,40p' "$0" | grep '^#' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "build-gui.sh: unknown arg '$1' (try --help)" >&2; exit 2 ;;
  esac
done

# Need ncurses-devel at build time when the pancurses feature is requested.
NEED_NCURSES=0
case ",$FEATURES," in
  *,pancurses,*) NEED_NCURSES=1 ;;
esac

# Resolve the project root (where this script lives) and cd there, so the build
# happens in the right tree regardless of where the script was invoked from.
ORIG_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT_DIR="$ORIG_DIR"
cd "$SCRIPT_DIR"

step() { echo "==> $*"; }

# ----------------------------------------------------------- auto-stage to /mnt/c
# AlmaLinux-8 can only read Windows-mounted drives. If we are NOT on one, copy a
# (symlink-resolved) snapshot to a temp dir under /mnt/c and build there.
STAGE=""
case "$SCRIPT_DIR" in
  /mnt/[a-zA-Z]*) ;;   # already Windows-visible; build in place
  *)
    if [ -d /mnt/c ]; then
      STAGE="$(mktemp -d /mnt/c/pir-gui-stage.XXXXXX)"
      step "project not on a Windows-visible path; staging snapshot to $STAGE"
      tar -h -cf - --exclude=target --exclude=.git --exclude=.jj \
          --exclude=.pir --exclude=.rustxWidgets-upstream -C "$ORIG_DIR" . \
        | tar -xf - -C "$STAGE"
      cd "$STAGE"
      SCRIPT_DIR="$STAGE"
    else
      echo "build-gui.sh: cwd ($ORIG_DIR) is not Windows-visible and /mnt/c is missing;" >&2
      echo "   AlmaLinux-8 cannot read it. Run from a /mnt/c path or mount the drive." >&2
      exit 1
    fi ;;
esac

# ----------------------------------------------------------- sanity checks
[ -f Cargo.toml ] || { echo "build-gui.sh: Cargo.toml not found in $PWD" >&2; exit 1; }

if [ ! -f rustxWidgets/rustxwidgets/Cargo.toml ]; then
  echo "build-gui.sh: rustxWidgets sibling not found at ./rustxWidgets/rustxwidgets" >&2
  echo "   The GUI build needs rustxWidgets checked out next to this repo." >&2
  echo "   (See the comment above [dependencies] in Cargo.toml.)" >&2
  exit 1
fi

if [ ! -x "$WSL_EXEC" ]; then
  echo "build-gui.sh: WSL launcher not found at '$WSL_EXEC'." >&2
  echo "   Run this script from the Windows side (Git Bash / WSL / PowerShell)," >&2
  echo "   where the Windows filesystem mounts WSL's launcher." >&2
  exit 1
fi

# Confirm the distro is registered/runnable before we touch anything.
step "checking WSL distro '$WSL_DISTRO' ..."
if ! "$WSL_EXEC" -d "$WSL_DISTRO" ${WSL_USER:+-u "$WSL_USER"} true 2>/dev/null; then
  echo "build-gui.sh: cannot run commands in distro '$WSL_DISTRO'." >&2
  echo "   Register/install it, or pass --distro <name> to match 'wsl -l'." >&2
  exit 1
fi

# ----------------------------------------------------------- enable the GUI
# Cargo.toml keeps the GUI opt-in commented out. Back up the manifest + lock,
# uncomment the two lines we need, build, then restore (unless --keep-manifest).
BACKUP_TOML="$(mktemp)"
BACKUP_LOCK="$(mktemp)"
cp Cargo.toml  "$BACKUP_TOML"
cp Cargo.lock  "$BACKUP_LOCK"

restore_manifest=1
[ "$KEEP_MANIFEST" -eq 1 ] && restore_manifest=0

cleanup() {
  if [ "$restore_manifest" -eq 1 ]; then
    cp "$BACKUP_TOML" Cargo.toml
    cp "$BACKUP_LOCK" Cargo.lock
  fi
  rm -f "$BACKUP_TOML" "$BACKUP_LOCK" "${INNER_FILE:-}" 2>/dev/null || true
  [ -n "${STAGE:-}" ] && rm -rf "$STAGE" 2>/dev/null || true
}
trap cleanup EXIT

step "enabling [dependencies].rustxwidgets + features.gui in Cargo.toml (temporary)"
sed -i 's|^# \(rustxwidgets = { path = "rustxWidgets/rustxwidgets"\)|\1|' Cargo.toml
sed -i 's|^# \(gui = \["dep:rustxwidgets", "rustxwidgets/gtk"\]\)|\1|'     Cargo.toml

grep -q '^rustxwidgets = { path = "rustxWidgets/rustxwidgets"' Cargo.toml \
  || { echo "build-gui.sh: failed to enable rustxwidgets dependency" >&2; exit 1; }
grep -q '^gui = \["dep:rustxwidgets", "rustxwidgets/gtk"\]' Cargo.toml \
  || { echo "build-gui.sh: failed to enable gui feature" >&2; exit 1; }

# ----------------------------------------------------------- build in WSL
# Build script that runs INSIDE the distro. We discovered that AlmaLinux-8's
# `bash -c '<script>'` (script passed as an argv) breaks variable assignments/
# expansions — only pre-existing env vars like $HOME survive. So we write the
# script to a FILE the distro can read and execute it as `bash <file>`, which
# behaves normally. We substitute __GTK__ / __FEATURES__ / __NEED_NCURSES__
# before writing it.
step "building --release --features '$FEATURES' inside '$WSL_DISTRO' ..."
INNER=$(cat <<'INNER'
set -euo pipefail
export CARGO_TARGET_DIR="$HOME/.cache/pir-gui-target"
export PATH="$HOME/.cargo/bin:$PATH"

# Rust toolchain (idempotent): install via rustup only if cargo is missing.
if ! command -v cargo >/dev/null 2>&1; then
  echo ":: installing Rust toolchain (rustup) in the distro"
  curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  export PATH="$HOME/.cargo/bin:$PATH"
fi

# C linker needed to produce the final binary.
if ! command -v cc >/dev/null 2>&1; then
  echo ":: installing build essentials (gcc) via dnf"
  sudo dnf install -y gcc 2>/dev/null || dnf install -y gcc
fi

# ncurses headers needed to link the pancurses feature (skip if present).
if [ "__NEED_NCURSES__" = "1" ]; then
  if ! ls /usr/include/ncurses.h /usr/include/ncurses/ncurses.h >/dev/null 2>&1; then
    echo ":: installing ncurses-devel (pancurses feature) via dnf"
    sudo dnf install -y ncurses-devel 2>/dev/null || dnf install -y ncurses-devel
  else
    echo ":: ncurses-devel already present"
  fi
fi

# Optional: GTK3 runtime libs, so `pir --gui` can actually open a window.
if [ "__GTK__" = "1" ]; then
  echo ":: installing GTK3 runtime via dnf"
  sudo dnf install -y gtk3 2>/dev/null || dnf install -y gtk3
fi

echo ":: cargo build --release --features '__FEATURES__'"
cargo build --release --features "__FEATURES__"

mkdir -p target/release
cp "$CARGO_TARGET_DIR/release/pir" target/release/pir

echo ":: artifact"
command -v file >/dev/null 2>&1 && file target/release/pir \
  || echo "(file(1) not installed in distro; skipping type check)"
echo ":: dynamic dependencies (glibc / GTK resolved at runtime)"
ldd target/release/pir | head -n 20
INNER
)

# Substitute placeholders with this-script's values (global, in case of dupes).
INNER="${INNER//__GTK__/$INSTALL_GTK_RUNTIME}"
INNER="${INNER//__FEATURES__/$FEATURES}"
INNER="${INNER//__NEED_NCURSES__/$NEED_NCURSES}"

# Write to a file the distro can read, then run it as a SCRIPT FILE (NOT via
# `bash -c`, which mangles variable expansion in this WSL setup).
INNER_FILE="$SCRIPT_DIR/.build-gui.wsl.sh"
printf '%s\n' "$INNER" > "$INNER_FILE"
"$WSL_EXEC" -d "$WSL_DISTRO" ${WSL_USER:+-u "$WSL_USER"} bash "$INNER_FILE"
rm -f "$INNER_FILE"

# ----------------------------------------------------------- copy out
# If we staged to /mnt/c, bring the binary back to the user's original tree.
if [ -n "$STAGE" ]; then
  mkdir -p "$ORIG_DIR/target/release"
  cp "$SCRIPT_DIR/target/release/pir" "$ORIG_DIR/target/release/pir"
fi
step "binary ready: $ORIG_DIR/target/release/pir"
if [ -n "$OUT_DIR" ]; then
  mkdir -p "$OUT_DIR"
  cp "$ORIG_DIR/target/release/pir" "$OUT_DIR/pir"
  step "also copied to $OUT_DIR/pir"
fi

echo
echo "Done. Built a Linux GUI binary (glibc 2.28+, for AlmaLinux/RHEL/Rocky 8+)."
echo "Run it under Linux/WSL with GTK3 present, e.g.:"
echo "    pir --gui"
echo "(The default 'pir' binary still works without GTK; --gui opens the GTK REPL.)"