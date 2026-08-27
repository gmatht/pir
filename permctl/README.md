# ai-permctl — least-privilege file access for the AI agent

The AI runs as `ai_rpi` with `HOME=/root`, but `/root` and `/root/.config`
are root-only (`0700`). The AI therefore cannot read root's `gh` config or any
other credential. This tool lets the AI **request** access; a privileged
enforcer (run by a human operator) validates and applies the **minimal** grant.

## Model

- **Request, don't take.** The AI queues a structured JSON request. It never
  touches root-owned files itself.
- **Validate against an allowlist.** The enforcer only honors files under
  allowed prefixes (default: `/root/.config/gh`). Anything else is denied and
  logged.
- **Minimal grant.** For a granted file: `chgrp ai_rpi` + `chmod g+r` (read).
  To make the file *reachable*, each ancestor dir gets `o+x` (search only —
  the AI can traverse but not list). The file itself is never made world-readable.
- **Auditable + reversible.** Every grant/deny/revoke is appended to
  `/var/log/ai-permctl.log`. Each grant records the file's original group/mode
  and an expiry; `revoke` or TTL expiry restores it exactly.

## Files

- `ai-perm-request` — AI-side: queues requests into `$AI_PERM_REQUEST_DIR`
  (default `/tmp/ai-perm-requests`).
- `perm-enforcer` — root-side: validates the queue, applies grants, rotates
  expired ones, reconciles ancestor dirs. State in `/var/lib/ai-permctl`.
- `install.sh` — root-only: installs the enforcer to `/usr/local/sbin`,
  creates state/log dirs, and adds `sudo perm-enforcer` to the `sh-gate`
  sudoers file.

## Usage

AI side:
```sh
ai-perm-request grant-read /root/.config/gh/hosts.yml --reason "gh push" --ttl 2h
ai-perm-request list
ai-perm-request revoke <id>
```

Operator side (root):
```sh
sudo /usr/local/sbin/perm-enforcer      # or: sudo perm-enforcer (after install)
```

## Security notes / caveats

- `o+x` on ancestor dirs is the only way to let a non-root user *traverse* a
  `0700` tree without exposing listings. It does **not** grant read on the
  directory contents. Still, prefer copying a token into AI-owned storage
  (`GH_CONFIG_DIR=/home/ai_rpi/.config/gh`) over live shared access whenever
  possible — that avoids widening traversal on root's home at all.
- The allowlist is the real security boundary. Keep it tight; the enforcer
  refuses directory grants (request the specific file).
- `sh-gate` still requires a human password — nothing here grants the AI
  unattended root.
