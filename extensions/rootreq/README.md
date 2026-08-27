# rootreq — request privilege escalation (off by default)

The agent (`ai_*` / `ai_rpi`) can **request** privilege escalation, but never
escalate itself. This mirrors the `ai-permctl` "request, don't take" model.

## Tools (enabled with `PIR_ROOTREQ=1`)

- **`request_root`** — queue an escalation request. Intents (all allowlisted):
  - `apt-install <pkgs>` — install package(s) via the logged `ai-apt-install` wrapper.
  - `mk-ai-user <ai_NAME>` — create a new `ai_*` account.
  - `su-ai <ai_NAME>` — switch to an `ai_*` account.
  - `command <id|uname|whoami|pwd|lsb_release>` — one allowlisted read-only command.
  The agent must give a `reason`. The request is written to the spool
  (`/tmp/ai-perm-requests` by default) and a human is told how to apply it
  (`sudo rootreq-enforcer`). **If the calling user already holds a passwordless
  sudo rule for the exact intent, the tool may run it inline** — but it never
  broadens its own privilege.
- **`run_as`** — run a command as an `ai_*` user the agent is *already* permitted
  to (existing sudoers). Grants no new privilege.

## Operator side (root)

`rootreq-enforcer` reads the spool, validates each request against an allowlist
(no shell metacharacters; `ai_*` name shape; tiny command set), and applies the
minimal grant via the root-owned `skynet` wrappers
(`/usr/local/sbin/{ai-apt-install,mk-ai-user,su-ai}`). Every action is logged to
`/var/log/ai-permctl.log`. Deploy the wrappers first with
`install-skynet-ai.sh` (root).

```sh
sudo rootreq-enforcer        # reviews + applies queued requests
```

## Security model

- **Request, don't take.** The agent queues; an operator (root, or a `skynet_*`
  account with a sudoers rule) fulfills. No unattended root by default.
- **Allowlist is the boundary.** The enforcer rejects anything not in an exact
  intent/shape. No `;`, no `*`, no writing sudoers, no `rm -rf`.
- **Reuses existing scaffolding.** Same spool as `ai-perm-request`; the actual
  grants go through the already-validated `skynet` wrappers, so `rootreq` grants
  nothing the operator hasn't already authorized system-wide.
