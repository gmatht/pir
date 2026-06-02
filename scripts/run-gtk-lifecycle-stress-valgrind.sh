#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT_DIR/target/debug/examples/gtk-lifecycle-stress"
SUPPRESSIONS="$ROOT_DIR/scripts/valgrind-gtk.supp"
SEED="${SEED:-1}"
STEPS="${STEPS:-40}"
TIMEOUT_SECS="${TIMEOUT_SECS:-120}"
USE_XVFB="${USE_XVFB:-1}"
PREFER_GTK3="${PREFER_GTK3:-1}"

# Keep the default Valgrind pass short enough to stay in the current stable
# smoke envelope; larger step counts are useful for bug-finding but can hit
# known GTK3 lifecycle crashes before memory diagnostics complete.

cargo build -p rustxwidgets --example gtk-lifecycle-stress --manifest-path "$ROOT_DIR/Cargo.toml"

cmd=(
    valgrind
    --tool=memcheck
    --error-exitcode=101
    --leak-check=full
    --show-leak-kinds=definite,indirect,possible
    --track-origins=yes
    --num-callers=40
    --gen-suppressions=all
    --suppressions="$SUPPRESSIONS"
    "$BIN"
    --seed "$SEED"
    --steps "$STEPS"
)

if [[ "$PREFER_GTK3" == "1" ]]; then
    cmd+=(--prefer-gtk3)
fi

echo "[valgrind] seed=$SEED steps=$STEPS suppressions=$SUPPRESSIONS"
if [[ "$USE_XVFB" == "1" ]]; then
    timeout "${TIMEOUT_SECS}s" xvfb-run -a "${cmd[@]}"
else
    timeout "${TIMEOUT_SECS}s" "${cmd[@]}"
fi
