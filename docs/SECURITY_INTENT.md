# SECURITY_INTENT.md — secret access: ask-gate, intent heuristic, and what "extra permissions" mean

How the agent gets to touch credentials (SSH keys, gh tokens, `~/.aws`, …): the
tool-level **ask** gate, the **session-intent** heuristic, the red warning the
user sees when extra permissions are granted, and a post-mortem of the
`/etc/canary` incident.

---

## 1. Why NOT FIFO credentials

Turning `~/.ssh/id_ed25519` / `~/.config/gh/hosts.yml` into named pipes (feed
the secret only after a human says yes) is fragile:

- **FIFO reads are destructive** — a read consumes the data; a second `open()`
  gets EOF. Tools routinely open a file twice (perm-check then read; `ssh`
  reads the key *and* `~/.ssh/config`) → intermittent failures.
- **Blocking = hangs** — nothing-writes-yet blocks the reader indefinitely; a
  "deny" must produce EOF + a timeout or `ssh` hangs on a dismissive user.
- **Some tools require a regular file** (`stat()`/`mmap` checks).
- **The human's own tools hit the pipes** unless the FIFOs are gated to the
  agent session — which breaks the human's normal `ssh`/`git`.
- **The keys have to move to a new trust store** the watcher feeds from —
  you've built a second secret broker with its own attack surface.

**Decision:** keep the credential files real; gate the *access* at the tool
layer.

## 2. The tool-level ask gate

Secret paths are already recognized (`is_secret`: `~/.ssh`, `~/.aws`,
`~/.gnupg`, `*.key`/`*.pem`, `~/.config/gh`, …) and every tool call runs
through `security_preflight`/`decide`. The gate:

- `security.intent = ask` (default): when a tool would read a secret path,
  surface `allow once / allow session / deny` (TTY prompt; queued `ai-perm-request`
  in full-auto). Approve → the command proceeds and reads the real file; deny →
  blocked with a reason. This is the FIFO's "only feed the secret on yes" —
  without the FIFO breakage.
- Grant is **path-bound, session-scoped, logged** (`{parcel, scope, reason,
  ttl, who}`), revocable (`/rights revoke`), TTL-expiring.

## 3. The session-intent heuristic

To stop nagging when the user clearly wants git/gh access, scan the recent
session transcripts (`~/.pi/agent/sessions/*.jsonl`) for an active push/gh
workflow: `git push`, `git fetch`, `git pull`, `git rebase origin`, `git clone`,
`git remote`, `gh pr create`, `gh pr`, `gh repo`, `gh auth`, `cargo publish`,
`ssh -T git@`, `force-push`. If the last N sessions match, the heuristic
**pre-grants gh/ssh read access for this session** — which is "look through the
sessions, determine the user wants the agent to access these."

`security.intent = intent` auto-consults the heuristic; `ask` (default) still
prompts; `deny` never allows secret reads.

## 4. The red "extra permissions granted" warning

When the heuristic auto-grants (or `/su-security off` grants invoker authority),
the operator MUST be told loudly — rendered in red, not dim:

```
╔══════════════════════ EXTRA PERMISSIONS GRANTED ══════════════════════╗
║  gh  : read access auto-granted (this session)                       ║
║  ssh : read access auto-granted (this session)                       ║
║  reason: recent sessions show a push/gh workflow (git push, gh pr…)  ║
║  revoke: /su-security on  ·  inspect: /rights                        ║
╚══════════════════════════════════════════════════════════════════════╝
```

Rule: **any time an extra permission is granted on behalf of the user (heuristic
or explicit), print the red banner; a grant without a banner is a bug.**

## 5. Post-mortem: why `echo boom > /etc/canary` in `/sh` was REAL

The evidence was in the session log:

```
[pir] project write-quarantine not engaged: overlay io error:
      create /home/ai_pir/agent/quarantine-staging/project: Permission denied (os error 13)
[pir] write-quarantine not engaged (...) ; writes are guarded in-process only
```

Three things combined on that day:

1. **The overlay never engaged.** A staging-path bug (`agent_user_home()/agent/
   quarantine-staging` — the `.pi` component was dropped when the base was
   switched from `pi_dir()` to `agent_user_home()`) made the staging uncreatable
   in the agent session, so the whole write-quarantine fell back to "writes are
   guarded in-process only" — **no overlay on `/etc` at all**.
2. **`/sh` bypasses the agent preflight.** `/sh` is the operator's interactive
   shell (via `run_shell`), not the agent's `bash` tool — it does NOT go through
   `security_preflight`, so the in-process guardrail's `GuardSystem` deny on
   `/etc` never fired for it.
3. **`/sh` had root.** With su-security off (or the invoking-identity path),
   `/sh` ran as root, so `echo boom > /etc/canary` took the normal root write
   path — straight into the real `/etc`.

So: **no overlay + no guardrail (because /sh) + root = real write.** The overlay
is the only thing that makes *any* process (root or not) in the agent's mount
namespace stage `/etc` writes — and it was down. With the staging path fixed (+
`/tmp` fallback so it engages even for unprivileged agent processes), `/etc`
writes — from the bash tool *or* `/sh` — stage into the upper and surface for
review instead of hitting the real filesystem. (Also: quarantine-not-engaged
messages are now **red**, so a downed safety net can't be missed.)

**Lesson:** a quarantine that fails to mount must be treated as an alarm (red),
because everything downstream silently degrades to "real fs".

---

## 6. Detecting capability-required attempts (chosen: in-process)

The root container drops most capabilities, so an operation that needs one
(e.g. `mount`, `setns`, `chroot`, raw I/O) fails with a silent `EPERM`. Options
to *see* those attempts, in order of cost:

- **In-process (chosen, on today)** — the `bash` tool checks, **once per
  command after it exits** (~µs; no per-syscall cost): the captured output for
  `Operation not permitted` / `Permission denied`, and the command string for
  privileged tools (`mount`, `chroot`, `setcap`, `nsenter`, `capsh`,
  `iptables`, `mknod`, `setuid`, …). On a hit it prints a yellow
  `[pir] possible capability-required operation …` line and appends a note to
  the tool result. Gated to the cap-dropped container context (or
  `PIR_DETECT_CAPS=1`) so benign "permission denied" elsewhere doesn't spam.
  Trade-off: post-hoc and output-dependent — it sees the *outcome*, not the
  silent in-kernel attempt.
- **auditd** — install `auditd` + `audit=1`; the kernel records capability
  denials as `cap_capable denied (cap=N)` with the exact capability number.
  Needs the daemon, and (WSL2) events live in the distro's log. More setup,
  in-kernel precision.
- **seccomp `RET_ERRNO` / `RET_LOG`** — a tiny filter on the agent's command
  processes returning `ERRNO` (cheapest in-kernel deny) or `LOG` (record the
  syscall to the audit log) for exactly the cap-requiring syscalls. Cost:
  ~50–200 ns **per syscall** (a few % on syscall-heavy builds) — the price of
  seeing attempts in-kernel.
- **seccomp `SECCOMP_RET_USER_NOTIF`** — a supervisor (pir) is woken on each
  matched syscall to log AND decide deny/allow interactively. Expensive on
  trigger (wakeup + IPC + wait) but zero in steady state if never triggered;
  only worth it if you want live per-attempt grants.

**Decision:** keep the in-process option as the default. Implement `auditd` and
the seccomp `RET_LOG`/`ERRNO`/`USER_NOTIF` variants **only if really needed**
(they'd be configurable via `PIR_DETECT_CAPS=audit|errno|log|notif`, default
`in-process`) — the added value is in-kernel *attempt* visibility and
per-attempt decisions, at per-syscall cost and setup.

---

## 7. Delete-intent tombstones (container) + future: reversible/cooldown deletes

The directory-rootfs container records the agent's deletions of mirrored host
files (a baseline of the copied `/etc` at container start; a file present then
but missing now, while the real host file still exists) as **delete-intent
tombstones**. `/quarantine status` lists them as `DELETE … (irreversible)`,
rules apply (`APPROVE DELETE: .*/[.]cache/.*` auto-approves), `apply` performs
the real removal, `discard` leaves the real file untouched (the container copy
is already gone). Deletes are **irreversible by design** — flagged in the UI.

**Future work — reversible/cooldown delete:** instead of `remove_file` on
`apply`, move the real file to a quarantine trash
(`~/.pi/agent/quarantine-trash/<timestamp>/…`) with a **cooldown/TTL** (e.g.
N days) and a `recover <path>` command, so a wrongly-approved delete can be
undone (trash auto-purges after the TTL). Rule candidates:
`security.delete = hard | trash | cooldown(N)`.
