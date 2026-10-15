#!/usr/bin/env bash
set -u

REPO_DIR="$(cd "$(dirname "$0")" && pwd)"
REMOTE="${PIR_REMOTE:-imac}"
REMOTE_DIR="${PIR_REMOTE_DIR:-pir-repro}"
MODE="${1:-reset}"
MODEL="${2:-mockan/m1}"
SECS="${3:-60}"
DELAY="${4:-1.5}"

PORT="${PIR_MOCK_PORT:-8765}"

REMOTE_HOME="$(ssh "$REMOTE" 'echo $HOME' 2>/dev/null)"
RDIR="$REMOTE_HOME/$REMOTE_DIR"

ssh "$REMOTE" "mkdir -p '$RDIR'" >/dev/null 2>&1
scp -q "$REPO_DIR/pty_run.py" "$REPO_DIR/mock_sse.py" "$REPO_DIR/models-store.json" \
        "$REPO_DIR/auth.json" "$REPO_DIR/analyze.py" "$REMOTE:$RDIR/"

ssh "$REMOTE" bash -s -- "$MODE" "$MODEL" "$SECS" "$PORT" "$REMOTE_DIR" "$DELAY" <<'REMOTE_SCRIPT'
set -u
MODE="$1"; MODEL="$2"; SECS="$3"; PORT="$4"; RDIR="$5"; DELAY="$6"
DIR="$HOME/$RDIR"
PY=/usr/bin/python
RAW="$DIR/capture.raw"
MOCKLOG="$DIR/mock.log"

pkill -f "mock_sse.py $PORT" 2>/dev/null
sleep 0.3
cd "$DIR"
nohup $PY "$DIR/mock_sse.py" "$PORT" "$MODE" "$DELAY" >"$MOCKLOG" 2>&1 &
sleep 0.8

rm -f "$RAW"
PI_DIR="$DIR/pirmock" \
PIR_RETRY_BASE_SECS="${PIR_RETRY_BASE_SECS:-2}" \
PIR_MAX_ATTEMPTS="${PIR_MAX_ATTEMPTS:-}" \
$PY "$DIR/pty_run.py" "$RAW" "$SECS" \
  "$HOME/bin/pir" -m "$MODEL" "hello"

sleep 0.3
pkill -f "mock_sse.py $PORT" 2>/dev/null
echo "--- mock hits ---"
grep -c served "$MOCKLOG" 2>/dev/null || echo 0
echo "--- footer elapsed sequence ---"
$PY "$DIR/analyze.py" "$RAW"
REMOTE_SCRIPT
