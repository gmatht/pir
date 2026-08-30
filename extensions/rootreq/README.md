# rootreq — request privilege escalation (on by default)

The agent (`ai_*` / `ai_rpi`) can **request** privilege escalation, but never
escalate itself. This mirrors the `ai-permctl` "request, don't take" model.
Queueing is **on by default** (`PIR_ROOTREQ=0` disables it). The agent only
ever *requests* — an operator must still fulfil each request out-of-band via
`rootreq-enforcer`, so enabling queueing grants no new privilege by itself.

## Tools (enabled by default; `PIR_ROOTREQ=0` to disable `request_root`)

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

`install-rootreq.sh` is a generic installer that wires everything together: the
`skynet` wrappers, the `/etc/sudoers.d/skynet-ai` gate, the `rootreq-enforcer`,
the spool/state dirs, and enables queueing by default (`PIR_ROOTREQ=1` in
`/etc/environment.d`). Use `sudo ./install-rootreq.sh --uninstall` to remove
what it deployed.

## How to use it

`pir` only ever *requests*; a human (or a `skynet_*` account with the passwordless
sudo rule) fulfils the request out-of-band. The flow:

1. **once (root):**
   ```sh
   sudo ./install-rootreq.sh --yes
   ```
2. **Agent asks** (as a tool call — never typed by you). Queueing is on by
   default, so the `request_root` tool is always available. Example intents:
   - `request_root intent=apt-install arg=<pkgs> reason="deploy new pir build"`
   - `request_root intent=mk-ai-user arg=ai_demo reason="new project"`
   - `request_root intent=su-ai arg=ai_demo reason="switch context"`
   - `request_root intent=command arg=whoami reason="probe identity"`
3. **Operator applies** the queued request:
   ```sh
   sudo rootreq-enforcer          # reviews + applies everything in the spool
   ```
   Each request is validated against an allowlist and logged to
   `/var/log/ai-permctl.log`. Anything not in the exact allowlist is denied.
4. **Audit:** `tail -F /var/log/ai-permctl.log`.

If the agent's user *already* holds a passwordless sudo rule for the exact
intent (e.g. `ai_pir` granted `NOPASSWD: /usr/local/sbin/...`), `request_root`
may run it **inline** without queueing — but it never broadens its own
privilege beyond what that rule already allows.

To turn the agent's queueing off: `PIR_ROOTREQ=0 pir …` (or remove the
`/etc/environment.d/pir-rootreq.conf` drop-in).

## Security model

- **Request, don't take.** The agent queues; an operator (root, or a `skynet_*`
  account with a sudoers rule) fulfills. No unattended root by default.
- **Allowlist is the boundary.** The enforcer rejects anything not in an exact
  intent/shape. No `;`, no `*`, no writing sudoers, no `rm -rf`.
- **Reuses existing scaffolding.** Same spool as `ai-perm-request`; the actual
  grants go through the already-validated `skynet` wrappers, so `rootreq` grants
  nothing the operator hasn't already authorized system-wide.
