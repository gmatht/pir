#!/bin/bash
# install-skynet-ai.sh — deploy the skynet / ai_* permission model.
#
# Asks for confirmation on each permission, then writes root-owned wrapper
# scripts and a generated /etc/sudoers.d/skynet-ai (validated with visudo).
#
# Permissions offered:
#   1. skynet (and skynet_*) may su, passwordless, to any ai_<alnum>+ user.
#   2. skynet (and skynet_*) may create new ai_<alnum>+ users (logged).
#   3. Any user X may su, passwordless, to its own X__<alnum>+ underling accounts.
#   4. ai_* users may run `apt install <pkgs>` (logged, validated).
#
# The wrappers are the authoritative security boundary; sudoers is a coarse gate.
#
# Usage:  sudo ./install-skynet-ai.sh

set -eu

if [ "$(id -u)" -ne 0 ]; then
  echo "error: run this script as root (sudo)." >&2
  exit 1
fi

W=/usr/local/sbin
SD=/etc/sudoers.d
CONF="$SD/skynet-ai"

confirm() {                 # $1 prompt -> 0 if yes, 1 if no
  local ans
  read -r -p "$1 [y/N] " ans
  case "$ans" in y|Y|yes|YES) return 0 ;; *) return 1 ;; esac
}

# ---------------------------------------------------------------- prompts
grant_skynet_su=0
grant_skynet_mk=0
grant_underling=0
grant_ai_apt=0

if confirm "Grant skynet/skynet_* passwordless su to ai_<alnum>+ users?"; then
  grant_skynet_su=1
fi
if confirm "Grant skynet/skynet_* creation of new ai_<alnum>+ users (logged)?"; then
  grant_skynet_mk=1
fi
if confirm "Grant every user X passwordless su to its own X__<alnum>+ underling accounts?"; then
  grant_underling=1
fi
if confirm "Grant ai_* users to run 'apt install <pkgs>' (logged, validated)?"; then
  grant_ai_apt=1
fi

if [ "$grant_skynet_su" -eq 0 ] && [ "$grant_skynet_mk" -eq 0 ] \
   && [ "$grant_underling" -eq 0 ] && [ "$grant_ai_apt" -eq 0 ]; then
  echo "No permissions selected; nothing to do."
  exit 0
fi

# ---------------------------------------------------------------- skynet user
if [ "$grant_skynet_su" -eq 1 ] || [ "$grant_skynet_mk" -eq 1 ]; then
  if id skynet >/dev/null 2>&1; then
    echo "skynet user already exists."
  elif confirm "Create the 'skynet' system user (no password, nologin)?"; then
    useradd -r -s /usr/sbin/nologin -c "skynet orchestrator" skynet
    echo "created user skynet."
  else
    echo "warning: skynet user missing; skynet grants will be unusable until it exists." >&2
  fi
fi

# ---------------------------------------------------------------- wrappers
install -d -m 0755 "$W"

if [ "$grant_skynet_su" -eq 1 ]; then
  cat > "$W/su-ai" <<'EOF'
#!/bin/sh
# su-ai: switch to an ai_<alnum>+ account without a password.
# Must be invoked via `sudo /usr/local/sbin/su-ai <name>` as root.
set -eu
target="${1:-}"
[ -n "$target" ] || { echo "usage: su-ai <ai_NAME>" >&2; exit 2; }
printf '%s' "$target" | grep -Eq '^ai_[[:alnum:]]+$' \
  || { echo "su-ai: target must match ai_[[:alnum:]]+" >&2; exit 2; }
id "$target" >/dev/null 2>&1 || { echo "su-ai: no such user: $target" >&2; exit 1; }
exec /bin/su - "$target"
EOF
  chown root:root "$W/su-ai"; chmod 0755 "$W/su-ai"
  echo "wrote $W/su-ai"
fi

if [ "$grant_skynet_mk" -eq 1 ]; then
  cat > "$W/mk-ai-user" <<'EOF'
#!/bin/sh
# mk-ai-user: create an ai_<alnum>+ account (no password, login shell).
# Must be invoked via sudo as root. Logged to /var/log/ai-user-mgmt.log.
set -eu
name="${1:-}"
[ -n "$name" ] || { echo "usage: mk-ai-user <ai_NAME>" >&2; exit 2; }
printf '%s' "$name" | grep -Eq '^ai_[[:alnum:]]+$' \
  || { echo "mk-ai-user: name must match ai_[[:alnum:]]+" >&2; exit 2; }
id "$name" >/dev/null 2>&1 && { echo "mk-ai-user: $name already exists" >&2; exit 1; }
echo "$(date -Iseconds) sudo_user=${SUDO_USER:-?} action=create user=$name" >> /var/log/ai-user-mgmt.log
/bin/useradd -m -s /bin/bash -c "AI agent account" "$name"
echo "created $name (home $(getent passwd "$name" | cut -d: -f6))"
EOF
  chown root:root "$W/mk-ai-user"; chmod 0755 "$W/mk-ai-user"
  touch /var/log/ai-user-mgmt.log; chown root:root /var/log/ai-user-mgmt.log; chmod 0640 /var/log/ai-user-mgmt.log
  echo "wrote $W/mk-ai-user"
fi

if [ "$grant_underling" -eq 1 ]; then
  cat > "$W/su-underling" <<'EOF'
#!/bin/sh
# su-underling: user X may su to its own X__<alnum>+ accounts.
# Must be invoked via sudo as root. Enforces target == ${SUDO_USER}__<alnum>+.
set -eu
caller="${SUDO_USER:-}"
target="${1:-}"
[ -n "$caller" ] || { echo "su-underling: must be run via sudo" >&2; exit 2; }
[ -n "$target" ] || { echo "usage: su-underling <${caller}__NAME>" >&2; exit 2; }
# anchor the caller name so e.g. "alice" cannot ride "alice2__x"
printf '%s' "$target" | grep -Eq "^${caller}__[[:alnum:]]+$" \
  || { echo "su-underling: target must be ${caller}__<alnum>+" >&2; exit 2; }
id "$target" >/dev/null 2>&1 || { echo "su-underling: no such user: $target" >&2; exit 1; }
exec /bin/su - "$target"
EOF
  chown root:root "$W/su-underling"; chmod 0755 "$W/su-underling"
  echo "wrote $W/su-underling"
fi

if [ "$grant_ai_apt" -eq 1 ]; then
  cat > "$W/ai-apt-install" <<'EOF'
#!/bin/sh
# ai-apt-install: logged, validated `apt-get install` for ai_* users.
# Must be invoked via sudo as root. Restricted to the invoking ai_* user.
set -eu
caller="${SUDO_USER:-}"
[ -n "$caller" ] || { echo "ai-apt-install: must be run via sudo" >&2; exit 2; }
case "$caller" in ai_*) ;; *) echo "ai-apt-install: not permitted for $caller" >&2; exit 1 ;; esac

opts=""; pkgs=""
while [ $# -gt 0 ]; do
  case "$1" in
    -y|-q|-qq|--no-install-recommends) opts="$opts $1"; shift ;;
    -*) echo "ai-apt-install: unsupported option: $1" >&2; exit 2 ;;
    *)
      printf '%s' "$1" | grep -Eq '^[A-Za-z0-9._+~-]+$' \
        || { echo "ai-apt-install: bad package name: $1" >&2; exit 2; }
      pkgs="$pkgs $1"; shift ;;
  esac
done
[ -n "$pkgs" ] || { echo "ai-apt-install: no packages given" >&2; exit 2; }

LOG=/var/log/ai-apt-install.log
echo "$(date -Iseconds) user=$caller pkgs=$pkgs opts=$opts" >> "$LOG"
exec /usr/bin/apt-get install -y $opts $pkgs
EOF
  chown root:root "$W/ai-apt-install"; chmod 0755 "$W/ai-apt-install"
  touch /var/log/ai-apt-install.log; chown root:root /var/log/ai-apt-install.log; chmod 0640 /var/log/ai-apt-install.log
  echo "wrote $W/ai-apt-install"
fi

# ---------------------------------------------------------------- sudoers
{
  echo "# /etc/sudoers.d/skynet-ai  (generated by install-skynet-ai.sh)"
  echo "# mode 0440, root:root. Wrappers re-validate all arguments."
  if [ "$grant_skynet_su" -eq 1 ] || [ "$grant_skynet_mk" -eq 1 ]; then
    echo "Cmnd_Alias SKYNET_AI_SU = /usr/local/sbin/su-ai [A-Za-z0-9_]*"
    echo "Cmnd_Alias SKYNET_AI_MK = /usr/local/sbin/mk-ai-user ai_[A-Za-z0-9]*"
    [ "$grant_skynet_su" -eq 1 ] && echo "skynet   ALL=(root) NOPASSWD: SKYNET_AI_SU"
    [ "$grant_skynet_mk" -eq 1 ] && echo "skynet   ALL=(root) NOPASSWD: SKYNET_AI_MK"
    [ "$grant_skynet_su" -eq 1 ] && echo "skynet_* ALL=(root) NOPASSWD: SKYNET_AI_SU"
    [ "$grant_skynet_mk" -eq 1 ] && echo "skynet_* ALL=(root) NOPASSWD: SKYNET_AI_MK"
  fi
  [ "$grant_underling" -eq 1 ] \
    && echo "ALL ALL=(root) NOPASSWD: /usr/local/sbin/su-underling [A-Za-z0-9_]*"
  [ "$grant_ai_apt" -eq 1 ] \
    && echo "ai_* ALL=(root) NOPASSWD: /usr/local/sbin/ai-apt-install"
} > "$CONF.tmp"

chown root:root "$CONF.tmp"; chmod 0440 "$CONF.tmp"
if ! visudo -cf "$CONF.tmp" >/dev/null 2>&1; then
  echo "error: generated sudoers failed validation:" >&2
  visudo -cf "$CONF.tmp" >&2 || true
  rm -f "$CONF.tmp"
  exit 1
fi
mv "$CONF.tmp" "$CONF"
echo "wrote $CONF"

# ---------------------------------------------------------------- summary
echo
echo "Installed permissions:"
[ "$grant_skynet_su"  -eq 1 ] && echo "  [x] skynet/skynet_*  su  -> ai_<alnum>+"
[ "$grant_skynet_mk"  -eq 1 ] && echo "  [x] skynet/skynet_*  mk  -> ai_<alnum>+ (logged)"
[ "$grant_underling"  -eq 1 ] && echo "  [x] any X            su  -> X__<alnum>+"
[ "$grant_ai_apt"     -eq 1 ] && echo "  [x] ai_*             apt install <pkgs> (logged)"

echo
echo "Test:"
[ "$grant_skynet_mk" -eq 1 ] && echo "  sudo -u skynet sudo mk-ai-user ai_demo"
[ "$grant_skynet_su" -eq 1 ] && echo "  sudo -u skynet sudo su-ai ai_demo whoami"
[ "$grant_ai_apt"    -eq 1 ] && echo "  sudo -u ai_rpi sudo ai-apt-install <pkg>"
[ "$grant_underling" -eq 1 ] && echo "  sudo -u alice sudo su-underling alice__worker"
