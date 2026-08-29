# Plan: per-user `su` / account creation inside `ai_*` and `X__*` namespaces

## Goal (from the request)
- The `skynet` user (and `skynet_*` users) may `su` to **any** `ai_[[:alnum:]]*` account, or
  **create** new `ai_*` accounts.
- Optional extension: any user `X` may `su` to its own `X__Y` ("underling") accounts.

## Deployment
An interactive installer is provided: `install-skynet-ai.sh` (run as root). It prompts
**yes/no for each permission** before applying anything, creates the `skynet` system user on
request, writes the root-owned wrappers to `/usr/local/sbin/`, and emits a `visudo`-validated
`/etc/sudoers.d/skynet-ai`. In addition to the `su`/`mk` powers below, it offers a logged,
validated `apt install` capability for `ai_*` users (see `ai-apt-install`).

## Environment facts (discovered)
- Ubuntu 24.04, sudo 1.9.15p5, shadow `su` with stock `/etc/pam.d/su`.
- Existing account `ai_rpi` (uid 984, `/usr/sbin/nologin`).
- No `skynet` user exists yet → must be created.
- `ai_*` should be **real local accounts** (not a single shared `ai` user), so each agent has its
  own uid, home, and audit trail.

## Design (recommended): sudo NOPASSWD wrappers, not PAM `pam_wheel`
Root causes every switch/create via a small root-owned wrapper that re-validates the target name.
This bounds privilege to the two exact regexes (`^ai_[[:alnum:]]+$` and `^X__[[:alnum:]]+$`) and
keeps secrets out of `su` (which would need the target password). Avoids editing PAM.

### Files to deploy (as root)
1. `/usr/local/sbin/su-ai` — `sudo su-ai <ai_NAME>` → validates `^ai_[[:alnum:]]+$`, runs
   `/bin/su - <target>`. No password required on the target.
2. `/usr/local/sbin/mk-ai-user` — `sudo mk-ai-user <ai_NAME>` → validates the same regex, then
   `/usr/sbin/useradd -m -s /bin/bash -c "AI agent account" <name>`. `(NOPASSWD,LOG)` so creation
   is audited. Whitelists the shell (no `-s` argument from the caller).
3. `/usr/local/sbin/su-underling` — `sudo su-underling <X__NAME>` → validates the target matches
   `^$SUDO_USER__[[:alnum:]]+$` (anchored caller prefix, so `alice` can't ride `alice2__x`),
   then `/bin/su - <target>`.
4. `/etc/sudoers.d/skynet-ai` (mode 0440, owned root:root) — grants:
   ```
   Cmnd_Alias SKYNET_AI_SU = /usr/local/sbin/su-ai [A-Za-z0-9_]*
   Cmnd_Alias SKYNET_AI_MK = /usr/local/sbin/mk-ai-user ai_[A-Za-z0-9_]*
   skynet    ALL=(root) NOPASSWD: SKYNET_AI_SU, SKYNET_AI_MK
   skynet_*  ALL=(root) NOPASSWD: SKYNET_AI_SU, SKYNET_AI_MK
   ALL ALL=(root) NOPASSWD: /usr/local/sbin/su-underling [A-Za-z0-9_]*
   ```
   - Use `NOPASSWD` for `su-ai` / `su-underling` (they never need a password); use
     `NOPASSWD:LOG` (sudo 1.9+) for `mk-ai-user` so creation is logged but passwordless.
   - The command args in sudoers are a coarse guard; the wrappers are the authoritative check.

### Setup (preferred: interactive installer)
```bash
sudo ./install-skynet-ai.sh      # prompts per permission, deploys + validates
```

### Setup steps (manual, as root)
```bash
# 1. create the orchestrator account (no password; root-only launch)
useradd -r -s /usr/sbin/nologin -c "skynet orchestrator" skynet 2>/dev/null || true

# 2. deploy wrappers
install -m 0755 /tmp/sky/su-ai          /usr/local/sbin/su-ai
install -m 0755 /tmp/sky/mk-ai-user     /usr/local/sbin/mk-ai-user
install -m 0755 /tmp/sky/su-underling   /usr/local/sbin/su-underling
install -m 0755 /tmp/sky/ai-apt-install /usr/local/sbin/ai-apt-install

# 3. deploy sudoers (visudo-checked) and lock it down
install -m 0440 /tmp/sky/skynet-ai.sudoers /etc/sudoers.d/skynet-ai
visudo -cf /etc/sudoers.d/skynet-ai

# 4. smoke test from skynet
sudo -u skynet sudo mk-ai-user ai_test1      # creates ai_test1 (logged)
sudo -u skynet sudo su-ai ai_test1 whoami    # => ai_test1
sudo -u skynet sudo su-ai root               # DENIED by wrapper+sudoers
sudo -u ai_rpi sudo ai-apt-install curl      # ai_* apt install (logged)
```

## Alternative considered
- **PAM `pam_wheel group=skynet` on `su`** — gives skynet passwordless `su` to *every* account,
  not just `ai_*`. Too broad. Rejected unless the intent truly is "skynet is all-powerful".

## Adding and removing the security at runtime
The deployed artifacts (the sudoers gate + the three root-owned wrappers) form the
"su based security" boundary. `pir` exposes a reversible, root-only toggle so an
operator can drop/re-apply the delegation without hand-editing files:

```
/su-security status   # report state of the gate + wrappers (no mutation)
/su-security off      # disable: rename artifacts to *.disabled (sudo silently
                     #   ignores any sudoers file containing a '.', so the gate
                     #   is gone immediately; wrappers become non-executable paths)
/su-security on       # re-enable: restore the *.disabled artifacts; the sudoers
                     #   file is re-validated with `visudo -cf` before it is
                     #   accepted (rollback to disabled on failure)
```

- Requires root (mirrors `pir project init`). `status` works for any user.
- Re-enabling when the model was never deployed reports "not installed" rather
  than fabricating a sudoers file.
- Nothing is ever deleted — only renamed — so the operation is fully reversible.
- When `pir` itself runs as an `ai_*` user (via `su-ai`/`become_user`), it defaults to
  **full-auto and will not prompt to confirm each command** — the account is the sandbox boundary.
  Override with `pir --confirm` or `PI_CONFIRM=1` (and `pir -y`/`PI_FULL_AUTO=1` to force it).
- Long-running `bash` commands: after 10s a live elapsed timer shows on the TTY; after 10 min
  the command is **detached into a background job** and control returns to the agent (which can
  `job_status`/`job_kill` it). This avoids blocking an unattended `ai_*` agent waiting on a human.
  A hard 2h ceiling still kills runaway commands.

## ai_* package installation (logged, validated)
To let agents install dependencies without broad root, `ai_*` users get a single passwordless
sudo command: `/usr/local/sbin/ai-apt-install <pkgs>`.
- The wrapper restricts the caller to `ai_*` users (`SUDO_USER` starts with `ai_`) and rejects
  `-*` options except a small safe allowlist (`-y -q -qq --no-install-recommends`).
- Package names are validated against `^[A-Za-z0-9._+~-]+$` (no shell metacharacters), so
  `sudo ai-apt-install 'git;rm -rf /'` cannot execute anything but a package list.
- Every invocation is appended to `/var/log/ai-apt-install.log` (`user=… pkgs=… opts=…`).

## Validation status
- `sh -n` on all three wrappers: OK.
- `visudo -cf /tmp/sky/skynet-ai.sudoers`: parsed OK.
- Live switching/useradd not yet run (would require root privilege on the host).
