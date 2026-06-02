#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT_DIR/target/debug/examples/gtk-lifecycle-stress"
SEED_START="${SEED_START:-1}"
SEED_COUNT="${SEED_COUNT:-10}"
STEPS="${STEPS:-60}"
TIMEOUT_SECS="${TIMEOUT_SECS:-30}"
USE_XVFB="${USE_XVFB:-1}"
PREFER_GTK3="${PREFER_GTK3:-1}"
VERBOSE="${VERBOSE:-0}"

# The default budget is a smoke sweep that is currently stable on GTK3.
# Raising STEPS turns this into a crash-finding fuzz run and may reproduce
# known lifecycle bugs still being investigated.

cargo build -p rustxwidgets --example gtk-lifecycle-stress --manifest-path "$ROOT_DIR/Cargo.toml"

for ((offset=0; offset<SEED_COUNT; offset++)); do
    seed=$((SEED_START + offset))
    cmd=("$BIN" --seed "$seed" --steps "$STEPS")
    if [[ "$PREFER_GTK3" == "1" ]]; then
        cmd+=(--prefer-gtk3)
    fi
    if [[ "$VERBOSE" == "1" ]]; then
        cmd+=(--verbose)
    fi

    echo "[stress] seed=$seed steps=$STEPS"
    if [[ "$USE_XVFB" == "1" ]]; then
        timeout "${TIMEOUT_SECS}s" xvfb-run -a "${cmd[@]}"
    else
        timeout "${TIMEOUT_SECS}s" "${cmd[@]}"
    fi
done
