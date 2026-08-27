#!/usr/bin/env bash
# install.sh — run as ROOT. Installs ai-permctl under /usr/local and a
# passwordless-ish sudo entry via sh-gate convention.
#
# This script is intentionally split so the AI (ai_rpi) never needs root:
#   * the AI-side tool `ai-perm-request` lives in the repo and is run by the AI.
#   * this root installer drops the enforcer into /usr/local/sbin and wires it
#     to `sh-gate` so an operator can run `sudo perm-enforcer` (with password)
#     after reviewing queued requests.
set -euo pipefail

SRC="$(cd "$(dirname "$0")" && pwd)"
DEST_BIN=/usr/local/sbin/perm-enforcer
STATE_DIR=/var/lib/ai-permctl
LOG=/var/log/ai-permctl.log

[ "$(id -u)" -eq 0 ] || { echo "install.sh: run as root" >&2; exit 1; }

install -m 0700 -o root -g root "$SRC/perm-enforcer" "$DEST_BIN"
echo "installed $DEST_BIN"

mkdir -p "$STATE_DIR/grants" "$STATE_DIR"
chmod 700 "$STATE_DIR" "$STATE_DIR/grants"
touch "$LOG"; chmod 640 "$LOG"

# Allowlist config — edit ALLOW_PREFIXES here (or set AI_PERM_ALLOW env in sudoers).
cat > /etc/ai-permctl.conf <<'CONF'
# ai-permctl enforcer config (sourced by perm-enforcer)
# Comma-free: space-separated allow prefixes. Only files under these may be
# requested by the AI for read access.
CONF
echo "wrote /etc/ai-permctl.conf (empty allowlist -> defaults to /root/.config/gh)"

# Wire to sh-gate so `sudo perm-enforcer` is allowed (password still required).
# Reads the existing sh-gate sudoers file and appends our command if absent.
SG=/etc/sudoers.d/sh-gate
if [ -f "$SG" ]; then
  if ! grep -q "perm-enforcer" "$SG"; then
    echo "ai_rpi ALL=(root) /usr/local/sbin/perm-enforcer" >> "$SG"
    echo "appended perm-enforcer rule to $SG"
  else
    echo "perm-enforcer rule already present in $SG"
  fi
else
  echo "ai_rpi ALL=(root) /usr/local/sbin/perm-enforcer" > "$SG"
  chmod 0440 "$SG"
  echo "created $SG"
fi

echo
echo "Done. From the AI shell, queue a request:"
echo "  ai-perm-request grant-read /root/.config/gh/hosts.yml --reason 'gh push' --ttl 2h"
echo "Then you (operator) run as root:"
echo "  sudo perm-enforcer"
