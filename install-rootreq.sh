#!/bin/bash
# install-rootreq.sh — generic config/install for the `rootreq` privilege
# "request, don't take" model.
#
# This deploys everything needed so an agent (running as an ai_* user) can
# *request* privilege escalation via the `request_root` tool and an operator
# (root) can fulfil it out-of-band with `rootreq-enforcer`:
#
#   1. root-owned allowlisted wrappers in /usr/local/sbin:
#        su-ai, mk-ai-user, ai-apt-install   (from install-skynet-ai.sh)
#   2. /etc/sudoers.d/skynet-ai              (passwordless sudo to those wrappers)
#   3. /usr/local/sbin/rootreq-enforcer      (the root enforcer the agent queues to)
#   4. spool + state dirs (/tmp/ai-perm-requests, /var/lib/ai-permctl, logs)
#   5. enables rootreq queueing by default (PIR_ROOTREQ is on-by-default in code;
#      we only set it in /etc/environment for shells that clear env).
#
# The agent NEVER gets root. It queues a request; a human (or a skynet_*
# account with the passwordless sudo rule) runs `sudo rootreq-enforcer` to
# apply the minimal, allowlisted grant. Every action is audited to
# /var/log/ai-permctl.log.
#
# Usage:
#   sudo ./install-rootreq.sh                 # interactive grant selection
#   sudo ./install-rootreq.sh --yes           # accept sensible defaults (all grants)
#   sudo ./install-rootreq.sh --uninstall     # remove deployed wrappers/enforcer/sudoers
#
# Env (non-interactive grant selection, any subset of):
#   SKYNET_SU=1  SKYNET_MK=1  UNDERLING=1  AI_APT=1
#
# Requires: bash, jq (for the enforcer), visudo (for sudoers validation),
#           and the sibling install-skynet-ai.sh in the same directory.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
W=/usr/local/sbin
SD=/etc/sudoers.d
CONF="$SD/skynet-ai"
ENFORCER="$W/rootreq-enforcer"
SPOOL="${AI_PERM_REQUEST_DIR:-/tmp/ai-perm-requests}"
STATE="${AI_PERM_STATE_DIR:-/var/lib/ai-permctl}"
AUDIT="${AI_PERM_AUDIT_LOG:-/var/log/ai-permctl.log}"

if [ "$(id -u)" -ne 0 ]; then
  echo "error: run this script as root (sudo ./install-rootreq.sh)." >&2
  exit 1
fi

# --------------------------------------------------------------- uninstall
if [ "${1:-}" = "--uninstall" ]; then
  rm -f "$ENFORCER" "$W/su-ai" "$W/mk-ai-user" "$W/ai-apt-install" "$W/su-underling"
  rm -f "$CONF"
  rm -rf "$STATE"
  # Leave /tmp spool + audit log (may hold pending requests / history).
  echo "Done. (/tmp/ai-perm-requests and $AUDIT left in place.)"
  exit 0
fi

YES=0
[ "${1:-}" = "--yes" ] && YES=1

# --------------------------------------------------------------- wrappers + sudoers
# install-skynet-ai.sh does the wrapper + sudoers work and is non-interactive
# when its SKYNET_SU/MK/UNDERLING/AI_APT env vars are set.
if [ "$YES" -eq 1 ]; then
  export SKYNET_SU=1 SKYNET_MK=1 UNDERLING=1 AI_APT=1
  export PIPELINE_YES=1
fi
if [ ! -f "$HERE/install-skynet-ai.sh" ]; then
  echo "error: $HERE/install-skynet-ai.sh not found" >&2
  exit 1
fi
echo "==> deploying skynet wrappers + sudoers (install-skynet-ai.sh)"
bash "$HERE/install-skynet-ai.sh"

# --------------------------------------------------------------- enforcer
echo "==> installing $ENFORCER"
if [ ! -f "$HERE/extensions/rootreq/rootreq-enforcer" ]; then
  echo "error: $HERE/extensions/rootreq/rootreq-enforcer not found" >&2
  exit 1
fi
install -d -m 0755 "$W"
install -m 0755 "$HERE/extensions/rootreq/rootreq-enforcer" "$ENFORCER"
command -v jq >/dev/null 2>&1 || { echo "error: jq is required by rootreq-enforcer (apt-get install jq)" >&2; exit 1; }

# --------------------------------------------------------------- spool + state
echo "==> creating spool/state/audit paths"
install -d -m 0700 "$SPOOL" "$STATE"
touch "$AUDIT"; chmod 0640 "$AUDIT"; chown root:root "$AUDIT" 2>/dev/null || true

# --------------------------------------------------------------- enable by default
# rootreq queueing is ON by default in the binary, so this is belt-and-braces
# for environments that start pir with a stripped env. Idempotent.
if [ -d /etc/environment.d ]; then
  cat > /etc/environment.d/pir-rootreq.conf <<'EOF'
# Enable the agent's privilege-request queueing by default.
PIR_ROOTREQ=1
EOF
  echo "==> wrote /etc/environment.d/pir-rootreq.conf (PIR_ROOTREQ=1)"
elif [ -f /etc/environment ]; then
  if ! grep -q '^PIR_ROOTREQ=' /etc/environment; then
    echo 'PIR_ROOTREQ=1' >> /etc/environment
    echo "==> appended PIR_ROOTREQ=1 to /etc/environment"
  fi
fi

# --------------------------------------------------------------- verify
echo
echo "==> sanity checks"
for f in "$W/su-ai" "$W/mk-ai-user" "$W/ai-apt-install" "$ENFORCER"; do
  [ -x "$f" ] && echo "  [ok] $f" || echo "  [MISSING] $f"
done
[ -f "$CONF" ] && { echo "  [ok] $CONF"; visudo -cf "$CONF" >/dev/null 2>&1 && echo "       (sudoers valid)"; } || echo "  [MISSING] $CONF"

echo
echo "rootreq is installed. Usage:"
echo "  agent side :  request_root intent=apt-install arg=<pkgs> reason=\"...\""
echo "  operator   :  sudo rootreq-enforcer     # reviews + applies queued requests"
echo "  logs       :  tail -F $AUDIT"
echo " disable    :  PIR_ROOTREQ=0 pir …   (or edit the environment drop-in)"
