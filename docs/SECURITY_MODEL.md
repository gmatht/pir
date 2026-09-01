# Windows Security Model for AI Agents

## 1. Goal

Give the operator a **configurable** way to keep an AI coding agent (and the
processes it spawns) from causing **real, non-theoretical harm** — corrupting a
production database, wiping credentials, nuking system config — while
**not** surprising pi users with onerous restrictions that only defend against
theoretical attacks.

Philosophy (drives every default below):
- pir is a lightweight clone of pi. The default posture must feel like running
  pi, **plus a few guardrails** — not a walled garden.
- **Reads are cheap to allow.** World-readable files are world-readable for a
  reason; an `ai` group's files are meant to be shared. The agent reads what
  the invoking user can read (normal DAC). Blocking these only annoys people.
- **apt / package managers are not a real threat vector** in practice — let
  them run by default.
- **Network is on by default.** We don't want the agent DDoSing someone, but
  that's theoretical; gating all egress by default would shock users.
- **Writes are where real harm lives.** The only thing worth guarding by
  default is *corruption of high-value data* (DBs, creds, other users, system
  state). Everything else the user can write, the agent can write.
- **If the LLM is truly malicious, you shouldn't run its code anyway** — so the
  goal is to stop *accidents and carelessness*, not a determined adversary.
  Heavy sandboxing that assumes a hostile model is the wrong trade-off here.

The hardened default-deny jail (§2/§5) and parcel UX (§7) remain available as
opt-in postures for hostile/multi-tenant scenarios. They are **not** the
default.

---

## 2. Recommended Model: Job Objects + AppContainer

This is the recommended hybrid. Neither primitive alone is sufficient, but
together they cover **isolation** (AppContainer) and **lifecycle/resource
control** (Job Object).

### 2.1 AppContainer — filesystem & network isolation

An AppContainer is a low-privilege process identity backed by a per-profile
SID. By default it has **no** access to the filesystem, registry, or network
except what is explicitly granted.

Steps to set up:

1. **Create a profile** once, per project/session:
   - `CreateAppContainerProfile(name, displayName, description, capabilities,
     appContainerSid)` — or call
     `DeriveAppContainerSidFromName` if you manage the SID yourself.
   - Fewer capabilities = smaller attack surface.
2. **Grant directory ACLs** — to the project root *plus* the minimal set of
   external paths the agent actually needs (see §2.2). For each, add an ACE for
   the AppContainer SID with the least privilege required:
   - Project root: `GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE`,
     inherited by children (this is the only writable project tree).
   - External read-only tools/libs: `GENERIC_READ | GENERIC_EXECUTE`.
   - External writable caches/temp: `GENERIC_READ | GENERIC_WRITE`.
   - Do **not** grant access to `C:\Users` home data, the registry hives, or
     secret stores. Under AppContainer those are already denied, so they need
     no explicit ACE — but they must stay absent from the grant list.
3. **Restrict capabilities**: pass an empty capability list, or only
   `internetClient` if the agent must do web search or fetch packages. Fewer
   capabilities = smaller attack surface. Network egress should then be
   narrowed further with a firewall/WFP allow-list (see §2.2).
4. **Launch inside the container**:
   - `CreateProcessAsUser` with a token from
     `CreateAppContainerToken` / `DuplicateTokenEx` set to the AppContainer
     SID, or call `CreateProcess` with the `PROC_THREAD_ATTRIBUTE_`
     `SECURITY_CAPABILITIES` attribute.

Result: the agent can read/write only what is explicitly granted. Any attempt
to open a file or socket outside the grants fails with `STATUS_ACCESS_DENIED`.

### 2.2 Legitimate external access the agent needs

A default-deny container is useless if the agent cannot compile, resolve
dependencies, or search the web. The trick is to grant *narrow, explicit*
exceptions rather than opening the whole profile. Treat each as an allow-list
entry, not a class of access.

**Filesystem — read-only (tools, SDKs, headers, libs):**
- Compiler & toolchain: `C:\Program Files\Microsoft Visual Studio\...`,
  `C:\Program Files (x86)\...`, `C:\msys64\...`, the Rust/Go/Node/Python
  install dirs.
- System DLLs needed at runtime: grant `GENERIC_READ | GENERIC_EXECUTE` on the
  specific directories (AppContainer already has a small set of
  well-known read paths; you may need to add a few).
- Read-only reference data: vendored deps, docs you deliberately share.

**Filesystem — writable (caches, temp, build output):**
- A per-session **scratch temp dir** outside the project, e.g.
  `%LOCALAPPDATA%\pir\scratch\<session>` — grant `RW` so build systems can
  write intermediate artifacts without polluting the project tree.
- Language/package caches *only if* offline use requires them
  (`~\.cargo`, `\npm-cache`, `\pip`): prefer letting the agent populate a
  fresh cache in the scratch dir instead, so nothing user-wide is touched.

**Network — web search / package fetch:**
- Grant the `internetClient` capability (outbound only). Pair it with a
  **Windows Firewall / WFP allow-list** restricting egress to the specific
  hosts/ports the agent needs (e.g. `api.search.example`, `crates.io:443`,
  `github.com:443`) and blocking everything else. Without this, `internetClient`
  permits *any* outbound connection.
- For sensitive environments, route egress through an **egress proxy** that
  the agent is configured to use, so the proxy — not the agent — holds the
  allow-list and can log every request.
- Keep `internetClientServer` and `privateNetworkClientServer` **off** unless
  the agent must accept inbound connections (it normally should not).

**What stays denied (the important part):**
- No access to the user's `Documents`, `Desktop`, `AppData` *user data*
  (configs, creds, browser profile, SSH/Microsoft Store keys).
- No access to other users' profiles, `C:\Windows` system stores outside the
  runtime DLL allow-list, the registry (except the container's own hive), or
  any mounted backup/cloud drive.

> Pattern: **read-only for tools, scratch-only for writes, proxied + allow-listed
> for network.** Everything else is denied by default.

### 2.3 Faking out-of-tree writes (staging + human review)

Sometimes the agent *thinks* it must write outside the project — a tool wants
to drop a config in `%APPDATA%`, write a log next to a binary, or patch a
system file. Rather than granting real write access (which defeats the
sandbox), **redirect those writes into a staging/upper layer** and let a human
decide whether to commit them. This is a union/overlay mount:

```
        read  ─────────────►  real outside path (lower, read-only to agent)
agent ──► projection (merged view)
        write ─────────────►  staging dir (upper, write-only to agent)
```

- **Reads** check the upper layer first, then fall through to the real (lower)
  file, so the agent sees a consistent merged view and its own writes read back.
- **Writes/creates/renames/deletes** go *only* to the upper layer. The real
  outside path is never mutated by the agent.
- **Review**: a user-side tool (run *outside* the container) diffs the upper
  layer against the real files and presents a per-file accept/reject list.
- **Apply**: accepted files are copied to their real targets; rejected ones
  are discarded. Nothing reaches the real filesystem without explicit consent.

#### Implementation options

1. **Projected File System (ProjFS) + union logic — recommended, transparent.**
   ProjFS is a *user-mode* Windows API that projects a virtual filesystem into
   a directory. You implement `PrjStartVirtualizing` with callbacks that serve
   reads from lower-then-upper and capture writes into the upper (staging)
   layer, reporting success to the caller. Because it's user-mode, no kernel
   driver signing is needed, and the AppContainer only needs RW to the
   projection root + staging dir — the real outside targets stay read-only/
   denied to the container. Fully transparent: no changes to the agent or its
   tools.

2. **Minifilter file-system driver — most faithful, heavier.** A kernel filter
   in the I/O path transparently redirects out-of-tree writes to staging.
   Highest fidelity (covers every process/API) but requires a signed driver
   and more engineering — usually overkill for an agent.

3. **No-driver "intent manifest" — lightweight.** Skip the virtual filesystem:
   the agent writes under `staging/` and records intended real targets in a
   `.pir-apply.json` manifest
   (`{ "staging/foo.conf": "C:\\Users\\x\\AppData\\foo.conf" }`). A `pir apply`
   command (run by the user, *outside* the container, with normal privileges)
   shows a diff and copies only what the user accepts. Downside: not
   transparent — tools that hard-code an absolute path won't find their writes
   unless you remap the path for them.

#### Security & UX notes
- The staging/upper layer must live **inside** the sandbox (project root or
  scratch), so the AppContainer already owns it and cannot escape via it.
- Keep the lower layer **read-only to the container**; otherwise a "delete"
  intent could wipe the real file. Implement delete-on-upper as a *tombstone*
  that hides the lower file from the agent but leaves the real file intact
  until review.
- Review tooling runs with the user's normal privileges and is the *only*
  component allowed to touch real outside paths — preserving the trust
  boundary.

### 2.4 Job Object — lifecycle & resource control

A Job Object groups the agent and all descendants under one controller.

Recommended limits:

- `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` — terminate the whole tree the instant
  the launcher exits or crashes (no orphaned shells, compilers, or runaway
  scripts).
- `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` — cap the number of live processes (e.g.
  64) to stop fork-bombs.
- `JOB_OBJECT_LIMIT_JOB_MEMORY` / `JOB_OBJECT_LIMIT_PROCESS_MEMORY` — bound
  RAM.
- `JOB_OBJECT_LIMIT_JOB_TIME` — optional CPU-time budget per session.
- `JOB_OBJECT_UILIMIT_*` (via `SetInformationJobObject` with
  `JobObjectBasicUIRestrictions`) — block desktop switching, clipboard, and
  `ExitWindows` so the agent cannot log the user off or lock the screen.

### 2.5 Put it together (launcher pseudocode)

```
sid   = DeriveAppContainerSidFromName("pir.session.123")
CreateAppContainerProfile(..., sid)

# Project tree: full RWX, inherited.
GrantAcl(projectRoot, sid, RWX, INHERIT_ONLY|OBJECT_INHERIT)
# Toolchains: read + execute only.
GrantAcl(vsInstallDir,    sid, RX)
GrantAcl(msysDir,         sid, RX)
GrantAcl(rustToolchain,   sid, RX)
# Scratch space: read + write (no execute of attacker-dropped binaries).
GrantAcl(scratchDir,      sid, RW)

# Network: outbound only, narrowed by firewall/WFP to an allow-list,
# optionally via an egress proxy.
caps  = [internetClient]   # only if web search / package fetch is required

job   = CreateJobObject()
SetInformationJobObject(job, KILL_ON_CLOSE | ACTIVE_PROCESS_LIMIT)

token = CreateAppContainerToken(sid, capabilities=caps)
proc  = CreateProcessAsUser(token, "cmd.exe" /C build-and-edit)

AssignProcessToJobObject(job, proc)
# ... run agent loop ...
TerminateJobObject(job, 0)   # or just close the handle
DeleteAppContainerProfile("pir.session.123")
```

---

## 3. Alternative Models (compare & contrast)

| Model | Isolation strength | Overhead | Setup cost | Fits a terminal agent? |
|---|---|---|---|---|
| **Standard restricted user** | Low. The agent can still read most of `C:\Users\<user>` (Documents, AppData) and roam the network. | None (built-in) | Trivial | ❌ Too permissive — exactly the "wandering" we must prevent. |
| **Job Object only** | None on filesystem. Great for killing the tree, useless against path traversal. | Negligible | Low | ❌ Incomplete. |
| **AppContainer only** | Strong FS/registry/network isolation via SID + ACLs. | Negligible (native process) | Medium (SID + ACL plumbing) | ⚠️ Good isolation but no per-session process-tree kill guarantee. |
| **Windows Sandbox / Hyper-V VM** | Maximum (separate kernel, disposable). | High (hundreds of MB RAM, boot time) | Low (one-click) | ❌ Overkill; hard to map a live project dir in/out and to drive a terminal agent interactively. |
| **WSL2 + Linux container/chroot** | Strong (Linux namespace/jail). | Medium (VM-light). | Medium | ⚠️ Fine *if* the agent runs on Linux; not a Windows-native answer. |
| **AppLocker / WDAC (allow-listing)** | Controls *what* runs, not *where* it goes. | Negligible | High (policy authoring) | ⚠️ Complements, does not replace, isolation. |
| **Mandatory Integrity Level (MIC) + low-IL token** | Weak. Low-IL blocks writes to medium objects but still reads most files. | Negligible | Low | ❌ Insufficient alone. |
| **Job Object + AppContainer (recommended)** | Strong FS/registry/network + guaranteed tree teardown. | Negligible | Medium | ✅ Yes. |

### Notes
- **AppLocker/WDAC** and a **low Integrity Level** are useful *defense-in-depth*
  layers you can stack on top, but neither alone stops directory escape.
- **Windows Sandbox** is the right tool when you need a *clean, untrusted*
  environment (e.g. running a downloaded binary), not for an agent that must
  persistently edit your repo.

---

## 4. Recommendation

> **Default to `guard`, not a hard jail.** Run the agent as the invoking user
> with normal DAC reads, apt on, and network on — but deny/redirect writes to a
> short, explicit list of critical targets (production DBs, credentials, other
> users' trees, system state, and its own test oracle). This stops the accidents
> that actually happen (corrupting a DB, wiping keys, faking a test) without
> shocking pi users with theoretical restrictions. Reach for the full
> default-deny jail (§2/§5 + §7) only when the model is untrusted or the machine
> is multi-tenant.

Layered extras (optional, recommended for production):
- Launch the launcher itself as a **medium-IL, non-admin** process (Windows) or
  a dedicated **`ai_*` user** (Linux) so the guardrail's "other users' trees"
  deny is meaningful and audit trail is clean.
- Add **WDAC/AppLocker** (Win) or a seccomp/Landlock write rule (Linux) to make
  the §9.3 guardrail cheap and tamper-evident.
- Mount the project dir as the writable tree and keep everything else
  read-denied only when you opt into `sandbox`/`strict`.
- Log all guardrail hits via ETW/Sysmon (Win) or auditd (Linux) for audit.

---

## 5. Linux / WSL2 Equivalent (and where it's *stronger*)

Yes — and in several respects the Linux kernel primitives are a *superset* of
the Windows model. **WSL2 runs a real Linux kernel**, so everything below works
natively there too (and on bare Linux). The map:

| Windows primitive | Linux / WSL2 equivalent | Notes |
|---|---|---|
| AppContainer SID + ACL grants | **Mount namespaces + bind mounts + read-only mounts** of exactly the dirs you allow; everything else is simply not mounted into the jail. | No SID bookkeeping — the view *is* the policy. |
| Capability stripping (`internetClient` etc.) | **Linux capabilities** (`capset` / `CAP_NET_RAW` dropped, etc.) + a separate **network namespace** with no routes by default. | Drop all caps; re-add the few needed. |
| Job Object `KILL_ON_JOB_CLOSE` | **PID namespace + `kill --kill-child` / cgroup `pids.max` + `PR_SET_PDEATHSIG`** or a dedicated cgroup that you `kill` on exit. | Whole process tree dies with the launcher. |
| Job Object memory/time/process limits | **cgroups v2** (`memory.max`, `cpu.max`, `pids.max`). | First-class, hierarchical. |
| User-restriction / integrity | **User namespace** (agent runs as uid≠your real uid; file ownership remapped) — *or* just a normal unprivileged user. | Agent literally cannot touch your files without an explicit bind mount. |
| Firewall / WFP egress allow-list | **nftables / iptables** OUTPUT allow-list + **egress proxy**. | Identical intent. |
| ProjFS union (fake writes) | **overlayfs** upperdir=staging, lowerdir=real, workdir=tmp. Fully kernel-native and transparent. | Linux's `overlayfs` is the *battle-tested* version of what ProjFS approximates. |
| Sysmon/ETW audit | **auditd** (`-w` watches) + **fanotify** + **bpf/LSM** (e.g. **Landlock**). | Landlock lets *unprivileged* processes self-confine to a set of paths. |

### 5.1 Recommended Linux/WSL recipe

1. **Namespaces** via `unshare` (or a small launcher using `clone(2)`):
   - `mount --make-rprivate /` then bind-mount the **project dir** at `/` (or
     `/home/agent`), the **toolchain** read-only, and a **scratch** dir RW.
   - Separate `pid`, `net`, `user`, `ipc`, `uts` namespaces.
2. **Capabilities**: `capsh --drop=all` except `CAP_NET_BIND_SERVICE` if needed;
   run as a mapped non-root uid via the user namespace.
3. **overlayfs for fake out-of-tree writes**:
   ```
   mount -t overlay overlay \
     -o lowerdir=/real/appdata,upperdir=/scratch/upper,workdir=/scratch/work \
     /mnt/agent-appdata
   ```
   The agent writes to `/mnt/agent-appdata`; real `/real/appdata` is never
   touched. The user reviews `/scratch/upper` and `cp`/`rsync` accepted files.
4. **cgroups v2** limits:
   ```
   mkdir -p /sys/fs/cgroup/pir
   echo $$ > /sys/fs/cgroup/pir/cgroup.procs
   echo 2G > /sys/fs/cgroup/pir/memory.max
   echo 64  > /sys/fs/cgroup/pir/pids.max
   ```
5. **Network egress**: in the new netns, add a single default route through an
   egress proxy, then `nft add rule inet filter output ip daddr != <proxy> drop`.
6. **Lifecycle**: launch under `setsid` + cgroup; on exit `cgkill`/`kill -9` the
   cgroup, or use `systemd-run --scope --property=KillMode=process`.

### 5.2 Why Linux can be *better* than Windows here
- **Landlock** (since Linux 5.13) lets an *unprivileged* agent confine *itself*
  to a list of allowed paths (`LANDLOCK_ACCESS_FS_*`) — no admin, no driver, no
  SID/ACL ceremony. This is the closest thing to a one-call AppContainer and
  arguably simpler.
- **overlayfs** is in-kernel and production-hardened (Docker, Podman use it) —
  the fake-write/staging trick needs no user-mode filesystem like ProjFS.
- **cgroups v2** are hierarchical and scriptable; Job Object limits are a close
  but slightly less expressive cousin.
- WSL1 (the *translation* layer, not WSL2) does **not** support these — require
  **WSL2** (real kernel) for the full model.

---

## 6. Comparison with the current Linux implementation (`pir`)

The model above (AppContainer + Job Object on Windows; namespaces + cgroups +
overlayfs + Landlock on Linux) is the *target*. What `pir` actually does on
Linux today is simpler and weaker. This section contrasts them based on the
shipped code (`src/user.rs`) and the deployed `permctl` / `skynet-ai` tooling.

### 6.1 What `pir` does today on Linux
- **Per-project `ai_<project>` account.** `pir project init` (root) creates a
  non-login system user, `chown`s the project tree and `.pir/` to it, and gives
  it a self-owned `HOME` with `CARGO_HOME`/`GH_CONFIG_DIR`. At launch `pir`
  calls `become_user` → `setuid`/`setgid` to that uid. **The sandbox boundary is
  the Unix UID + filesystem DAC**, not a namespace or a jail.
- **No filesystem jail.** The agent runs as `ai_X` in the *real* root mount
  namespace. It can still `stat`/`read` any **world-readable** path on the host
  (`/etc`, `/usr/include`, other users' `o+r` files) and traverse any `o+x`
  ancestor. Only files owned by others without `o+r/o+x` are blocked. (The
  `cwd_accessibility_wizard` even relocates/clones the project into the
  sandbox user's home when an ancestor isn't traversable — i.e. it works *with*
  DAC, not around it.)
- **No network egress control.** The agent can make outbound connections to
  anywhere. No proxy/firewall allow-list.
- **No resource limits.** No cgroups; a runaway build or fork-bomb is bounded
  only by the 2h command ceiling and a 10 min detach — not by `pids.max`/
  `memory.max`.
- **Human-gated privilege via `permctl` + sudoers.** The AI runs as `ai_rpi`
  with `HOME=/root` but `/root` is `0700`, so it cannot read root's `gh` creds.
  To get a specific read it must `ai-perm-request grant-read <file>`; a root
  operator runs `perm-enforcer`, which (a) validates the path against an
  allowlist (`/root/.config/gh`), (b) does a minimal `chgrp ai_rpi`+`chmod g+r`
  + `o+x` on ancestors, (c) logs and sets a TTL. `su` to `ai_*` and
  `ai-apt-install` are gated by `/etc/sudoers.d/skynet-ai` behind a human
  password.
- **No write-staging/overlay.** If a tool insists on writing outside the
  project, it simply fails (DAC denies it). There is no "fake write then
  review" path.

### 6.2 Pros / cons — current vs proposed

| Aspect | Current (UID + DAC + sudoers + permctl) | Proposed (ns + cgroups + overlayfs + Landlock) |
|---|---|---|
| Confinement | Default-**allow** for world-readable host files; blocks only non-`o+r/o+x`. | Default-**deny**; agent sees only bind-mounted project + toolchain. |
| Network | Unrestricted egress (exfiltration possible). | nftables allow-list + egress proxy. |
| Resources | Only a 2h timeout; fork/mem bombs possible. | cgroups `memory.max`/`pids.max`/`cpu.max`. |
| Write outside dir | Hard fail. | overlayfs staging + human `pir apply`. |
| Audit / attribution | **Excellent** — real per-project uid in syslog/auditd; human`/`apt`. | Weaker unless you map a fixed subuid to a real uid; needs deliberate wiring. |
| Human-in-the-loop | **Built in** via `permctl` request/grant + sudoers password. | Must be re-layered (gate the bind/overlay setup behind the enforcer). |
| Runtime privilege | Root only at `init`; agent fully unprivileged after drop. | Needs user-ns + `/etc/subuid` + cgroup v2 delegation + netns/nft; more gears. |
| Portability | Works on any Linux, WSL1 *and* WSL2. | WSL2/modern kernels only (no WSL1, no ancient kernels). |
| Setup fragility | Chown + sudoers; low risk. | Many moving parts (mounts, cgroups, nft, overlay); more to break. |

### 6.3 Verdict / recommended layering
The two are **complementary, not exclusive**:
- Keep the **per-project `ai_*` UID + DAC** boundary — it is cheap, auditable,
  and gives one agent no way to touch another project's files. This is the
  identity/attribution layer the proposed model lacks out of the box.
- **Layer the proposed confinement on top** wherever the host supports it
  (WSL2, modern Linux): a user namespace + bind-mounts turn the "can read
  world-readable host files" gap into a true default-deny jail; cgroups stop
  resource abuse; nftables stop exfiltration; overlayfs gives the fake-write/
  review flow.
- **Preserve `permctl` as the gate.** Instead of granting a raw bind-mount or
  overlay to an arbitrary path, route it through `ai-perm-request` →
  `perm-enforcer`: the operator validates the path against the allowlist and
  then the launcher sets up the mount. That keeps the "request, don't take"
  guarantee inside the stronger jail.
- Where the host can't do namespaces (WSL1, old kernels), fall back to the
  current UID+DAC model — it is strictly better than nothing and already ships.

---

## 7. Informed denials and right "parcels"

A default-deny jail is only safe if it is also *transparent*. If a denied
operation is invisible, the agent either silently no-ops (confusing) or the
operator is tempted to grant a broad right "to make it work." The design below
makes **every denial a visible, classified, reversible request** and offers the
user a small menu of pre-vetted right bundles ("parcels") instead of an
all-or-nothing yes/no.

### 7.1 Making denials visible (the broker)

1. **Capture the attempt, not just the failure.** The confinement layer already
   knows *exactly* what was tried:
   - **Linux**: Landlock/overlayfs/auditd emit the path, access type
     (`LANDLOCK_ACCESS_FS_*`, `fanotify`), and pid; nftables logs the denied
     `daddr:port`; cgroup logs OOM/fork rejections.
   - **Windows**: the AppContainer rejects an open → Sysmon/ETW
     (`Microsoft-Windows-Kernel-File`, `Audit Object Access`) records the path,
     desired access, and SID; ProjFS callbacks know the exact virtualized
     write.
2. **Classify → parcel.** A broker maps each denial to the *narrowest* parcel
   that would satisfy it (see §7.3) and computes a risk rating + blast radius.
3. **Raise a prompt (or a queued request).**
   - **Interactive** (`pir` on a TTY, not `--confirm`/full-auto): show a banner:
     ```
     [denied] READ  /usr/include/openssl/ssl.h
             → satisfies parcel: toolchain-ro  (risk: low)
             [o] allow once  [s] allow this session  [n] no  [i] info
     ```
   - **Full-auto / `ai_*` agent** (`PI_FULL_AUTO`): **do not prompt**. Log the
     denial, queue it as an `ai-perm-request` (reusing the `permctl` channel),
     and let the agent retry later / continue. A human reviews the queue with
     `perm-enforcer` out-of-band. This preserves the "request, don't take"
     guarantee from `docs/SKYNET-AI-PERMS.md`.
4. **Apply minimally, revocably.** A grant adds *only* the parsed parcel:
   one bind-mount, one nft allow-list rule, one overlay upperdir, or one
   `permctl` read on a single file — never "more root." Every grant records
   `{parcel, scope, reason, ttl, who}` to an audit log and is `TTL`-expired or
   `revoke`-able (same reversible pattern as `/su-security on|off`).

### 7.1b The universal "ask" primitive — never broken, at worst naggy

The whole point is that **the agent can ask for literally anything**, so a
denial never strands it. Every blockable action — a read, a write, a network
connection, a capability, an apt install, even "run as root" — funnels through
one request channel:

```
pir ask <verb> <target> [--reason "..."] [--ttl 2h] [--scope session|revocable]
```
- `verb` ∈ {read, write, exec, connect, bind, apt, become-root, …} — anything
  the confinement layer can intercept.
- `target` is a path, `host:port`, capability name, package list, etc.
- The broker classifies it to the **narrowest parcel** (§7.3) or, if it's
  something we never anticipated, wraps it as a **`custom:<description>`**
  parcel the user can still approve/deny.

Two ways it fires, so the agent is never stuck:
- **Reactive (auto):** when a guarded op is blocked, the broker *automatically*
  raises an `ask` for exactly that op (with the captured details) — the agent
  does not even need to know the permission system exists. This is what makes
  `guard`/`sandbox` feel "naggy, not broken": a blocked DB write or a blocked
  host just becomes a prompt/queue item.
- **Proactive (agent-initiated):** the agent can call `pir ask` up front when
  it *knows* it will need something (e.g. "I'm about to `npm install -g`, may I
  have `apt`/network + write to `/usr/local`?"). Pre-asking lets a human approve
  in a batch instead of one nag per file.

If the agent is unsure whether something needs permission, the rule is
**ask rather than assume**. There is no "hard fail with no recourse" state:
every denial maps to a request the user can answer, defer, or reject.

### 7.1c "Ask mode" — the default that feels like a seatbelt, not a wall

`security.ask` controls how unanticipated requests behave:
- **`ask` (default at `guard`/`sandbox`):** any guarded op becomes a prompt on
  a TTY, or a queued `ai-perm-request` in full-auto. The agent keeps working
  (or waits politely) — it is *nagged*, never *broken*.
- **`auto-yes` (default at `off`):** unanticipated requests are granted to the
  invoking user's level with a log line; pure pi behavior.
- **`auto-no`:** unanticipated requests are denied + logged, but the agent is
  still told *how* to `pir ask` for them — so even here it's "denied, with a
  path forward," not a silent dead end.

So the security posture can be dialed from "silent pi" → "naggy but functional"
→ "strictly reviewed," and **at no setting does the agent hit a wall it cannot
ask its way past**. That is the bar: security should be a conversation, not a
crash.

### 7.2 Decision support the prompt must show
For each denial the user sees:
- **What** the agent tried (path / host:port / resource) and **why it was
  denied** (not mounted / not in allow-list / caps dropped).
- **Parcel** it maps to, and **what that parcel additionally permits** (the
  honest blast radius — e.g. "toolchain-ro also lets it read all of
  `/usr/include`").
- **Risk** (low/med/high) based on parcels' static ratings + whether it touches
  secrets/network/creds.
- **Scope & lifetime**: path- or host-bound, session-only vs. until revoked,
  with a default short `TTL` (e.g. 2h) for anything beyond read-only toolchain.
- **`[i]nfo`** expands to the full audit trail (prior denials, which parcels
  already granted).

Principles enforced by the broker: default to **deny**; prefer the **narrowest
parcel**; grants are **path/host-bound, not privilege-bound** (no "give root");
**secrets and network are always human-gated**.

### 7.3 Sensible parcels to offer

**Filesystem — read (low risk, usually session-scoped):**
| Parcel | Grants | Offer when… |
|---|---|---|
| `toolchain-ro` | read+exec of compiler/SDK/include dirs (`/usr`, `C:\Program Files\…`, msys64) | agent builds code and the toolchain wasn't pre-bind-mounted. |
| `docs-ro` | read of deliberately shared reference/docs dir | agent needs project-adjacent specs you chose to expose. |
| `vendor-ro` | read of vendored deps / mirrors | offline builds against a local registry. |
| `secret-read:<path>` | one-file read via `permctl` (`chgrp`+`g+r`, TTL) | agent needs a specific credential (e.g. `gh` token) for `gh push`. **Always human-gated.** |

**Filesystem — write (staged, never direct):**
| Parcel | Grants | Offer when… |
|---|---|---|
| `scratch-rw` | read+write to per-session scratch (default-on) | build intermediate output. |
| `config-staging` | writes to `~/.config/…` land in overlay **upper**, reviewed via `pir apply` | a tool insists on dropping a config. Default: **reject**, offer staging instead. |
| `cache-rw` | writes to a package cache (cargo/npm/pip) | offline dependency builds; prefer `scratch` over touching user-wide cache. |

**Network — outbound allow-list (med risk, host:port bound, egress proxy):**
| Parcel | Grants | Offer when… |
|---|---|---|
| `net-none` | default — all egress dropped | always the starting point. |
| `net-web-search` | `443`→search API host only | agent explicitly needs web search. |
| `net-packages` | `443`→crates.io / npm / pypi mirrors | `cargo`/`npm`/`pip install`. |
| `net-github` | `443`→api.github.com + github.com | clone/fetch/open PRs. |
| `net-dev-tunnel` | inbound + outbound to a chosen port | running a local dev server the user must reach. |

**Capabilities / privilege (high risk, always human-gated, logged):**
| Parcel | Grants | Offer when… |
|---|---|---|
| `no-caps` | default — all Linux caps dropped | always. |
| `cap-net-bind` | `CAP_NET_BIND_SERVICE` only | agent must bind a low port for a dev server. |
| `apt-install:<pkgs>` | run `ai-apt-install` (validated, logged) via sudoers | a system dependency is genuinely required. |

### 7.4 Lifecycle & review commands
- `pir rights` — list currently granted parcels, scopes, TTLs, and the audit
  trail of denials/grants for this session/project.
- `pir rights revoke <parcel>` — drop a grant immediately (removes the mount /
  rule / overlay / `permctl` grant).
- `perm-enforcer` (root operator) — drain the queued `ai-perm-request`s from
  full-auto agents; validates each against the allowlist before applying.
- Sessions expire; `scratch` and overlay `upper` are wiped on teardown, so
  staged-but-unaccepted writes never reach the real filesystem.

---

## 8. Running package managers (apt/dnf/yum/winget) safely

Package managers are the **exception case**: they need root *and* write to the
system root, which is exactly what the jail forbids. Handing the agent host
root (even via `sudo apt`) collapses the entire security model. The rule is:

> **The agent never gets host root. Installs happen *inside* the jail, land in a
> reviewable staging layer, and only reach the real system after human review —
> or, for the host itself, only via the existing human-gated `ai-apt-install`
> sudoers path.**

### 8.1 Prefer non-system, project-local installs (Tier 0 — no risk)
Most "I need apt" requests are really "I need a library." Resolve them *without*
touching the system at all:
- Python: `pip install --target <proj>/.deps` or a venv inside the project.
- Node: `npm install` into the project (no `-g`).
- Rust: `cargo` already writes only to `CARGO_HOME` (project-owned, see §6.1).
- Go: `go mod` / `GOPATH` inside scratch.<br>
These stay inside the project/`scratch` writedir and need nothing beyond the
`scratch-rw` parcel. **Always offer this first.**

### 8.2 Install inside the jail, then review (Tier 1 — low risk)
When a *system* package is genuinely required, run the package manager **in a
throwaway namespace** so its writes never touch the real root:
1. Enter the existing jail (PID + user + mount namespaces). Inside the user
   namespace the agent is **uid 0** (mapped to your real uid — no host root).
2. Mount an **overlayfs over `/`**: `lowerdir=/` (real root, read-only to the
   agent), `upperdir=/scratch/apt-upper`, `workdir=/scratch/apt-work`. Now
   `apt-get install` mutates *only* `apt-upper`; the real filesystem is
   untouched.
3. Grant the **`net-packages`** parcel (§7.3) so the manager can fetch from the
   pinned mirror through the egress proxy. Use `--no-install-recommends` and
   version-pin to minimize footprint.
4. After the install, **stop** the container and present `/scratch/apt-upper`
   as a diff: every file the package would add/modify on the real system. The
   user runs `pir apply` to accept (copy into real `/`) or reject each.
5. `apt-upper` is wiped on teardown, so an unaccepted install disappears.

This reuses the §2.3 staging pattern and the `permctl`/`pir apply` review
flow — the package manager thinks it succeeded; the host stays immutable until
you say so. It also contains **supply-chain risk**: a malicious package can
only damage the overlay, not your real OS, and you see exactly which files it
drops before accepting.

### 8.3 Real host install — branch on `security.apt` (Tier 2 — high risk)
If the package must truly live on the host (e.g. a system service the agent
will run), the path depends on the active `security.apt` setting (§9.2):
- **`auto`** (the default): the agent simply shells out to the real
  `apt`/`dnf`/`yum` as the invoking user. If that user already holds the
  privilege, the install just works — no gate, no review. This is the intended
  pi-like behavior: "normal apt just runs." If the user is *not* privileged
  (e.g. an `ai_*` agent), it falls through to the `human` path below.
- **`human`:** the agent issues `ai-perm-request apt-install <pkgs>` (or the
  denial broker maps a failed apt to the `apt-install:<pkgs>` parcel, §7.3) and
  a **root operator** reviews and runs `sudo /usr/local/sbin/ai-apt-install
  <pkgs>`: package names are regex-validated (`^[A-Za-z0-9._+~-]+$`), only a
  safe option allow-list is permitted, every call is logged to
  `/var/log/ai-apt-install.log`, and it requires the human's sudo password.
  This is **out-of-band**; a full-auto `ai_*` agent cannot trigger it
  unattended.
- Keep `human`/`stage` as the exception, not the default, unless the operator
  opts in.

### 8.4 Windows note (winget / choco / msys)
Windows has no overlayfs for system dirs, so the jail-install trick differs:
- Best: run the installer **inside Windows Sandbox** (disposable, already
  strongly isolated per §3) and copy the resulting artifacts out — same
  "review the diff" idea.
- Or: stage the install result through **ProjFS** (§2.3) so writes virtualize to
  a staging dir, then `pir apply` them.
- Real host installs (`winget`/`choco`) require admin and must go through a
  human-approved, logged elevated installer — there is no `ai-apt-install`
  equivalent yet, but the *pattern* (request → human → validated → logged) is
  identical to §8.3.

### 8.5 Decision summary
| Need | Mechanism | Host root? | Review |
|---|---|---|---|
| lib/dep for the project | language PM into project/scratch (§8.1) | no | none needed |
| system pkg, reviewable | jail + overlay upper + `pir apply` (§8.2) | no (ns root only) | yes, per-file |
| system pkg on host | `ai-perm-request` → human `ai-apt-install` (§8.3) | yes, human only | yes, logged |

Principles: **never host root for the agent**; **default to project-local**;
**overlay-stage review** for system pkgs; **real host installs stay human
password-gated and logged**. This slots cleanly into the §7 parcels
(`scratch-rw`, `net-packages`, `apt-install:<pkgs>`).

---

## 9. Configurable security posture

Security is **opt-in and configurable**, not forced — but the *default* is now
a pragmatic middle ground, not a hard jail and not laissez-faire `off`. Two
independent dials control it (`pir.toml` or env vars), plus a small
critical-target guardrail that is on by default.

### 9.1 Overall confinement level — `security.level` (`PIR_SECURITY_LEVEL`)
- **`guard`** — **default**. The agent runs as the **invoking user** (normal
  DAC: it can read/write exactly what that user can), with **apt and network
  on**, and **one guardrail**: writes to a short, explicit list of critical
  targets are **denied or staged for review** (see §9.3). Reads of
  world-readable and `ai`-group files are unrestricted. This is "pi, plus a
  seatbelt," and is meant to never surprise a pi user.
- **`off`** — no guardrail at all: identical to running plain pi. For trusted,
  single-user, throwaway work.
- **`sandbox`** — the default-deny jail from §2/§5 (bind-mounts / AppContainer,
  cgroups, overlayfs), with the §7 denial broker. Reads are restricted to what
  is mounted; network starts at `net-none`. Use for untrusted/multi-tenant.
- **`strict`** — `sandbox` plus *mandatory* human review for every
  cross-boundary op and zero network until explicitly allowed. Paranoia mode.
- **`worktree`** — a **write-isolated contributor mode** (see §11): the agent
  never writes to the real repo/trunk at all. It owns a git *worktree* + branch,
  submits pull/push requests, and merges only through a CI gate (auto-merge if
  tests pass) or a dedicated fixer agent. This is the strongest *repo* isolation
  available and composes with `guard`/`sandbox` for the rest of the host.

### 9.2 Package-manager, network & ask modes
- **`security.apt`** — **`auto` (default)**: normal `apt`/`dnf`/`yum`/`winget`
  just run as the invoking user. Other values (§8): `human` (gated via
  `ai-perm-request` → operator `ai-apt-install`), `stage` (jail + overlayfs +
  `pir apply`), `project` (refuse system installs).
- **`security.network`** — **`on` (default)**: unrestricted egress, like a
  normal user. Use `allowlist` (nftables/WFP to a host:port set) only when you
  want the §7 `net-*` parcels enforced; `off` blocks all egress.
- **`security.ask`** — **`ask` (default at `guard`/`sandbox`)**: any guarded op
  becomes a prompt or queued request (§7.1b/§7.1c) — naggy, never broken.
  **`auto-yes` (default at `off`)**: grant to the invoking user's level with a
  log; pure pi behavior. **`auto-no`**: deny + log but still tell the agent how
  to `pir ask`.

### 9.3 The critical-target guardrail (on at `guard`, the default)
Rather than jailing the whole machine, we deny/redirect writes to a small,
high-blast-radius allow-*deny* list. Sensible defaults:
- **Production databases** the user has flagged: `*.db`, `*.sqlite*`,
  `postgres`/`mysql` data dirs, `*.duckdb`, Redis/Mongo data files — a write
  that would truncate/corrupt them is **denied + alerted** (the agent may read
  them).
- **Credentials & secrets**: `~/.ssh`, `~/.aws`, `~/.gnupg`, `*.key`,
  `~/.config/gh`, browser profiles — **denied** (the `permctl` `secret-read`
  path can grant a single-file read on request).
- **Other users' homes / the `ai` group's private trees**, unless explicitly
  shared — denied, to keep one project's agent out of another's.
- **Boot/system state**: `/boot`, `/etc` (except a few append-only logs),
  EFI vars, systemd unit files — denied.
- **Its own test outputs**: the agent **may not overwrite the artifacts it is
  meant to test against** (the reference fixtures / expected-output files under
  the project). A write there is **denied**, so it cannot "fake a passing test"
  by clobbering the oracle. (It can still create new files and edit its own
  source.)
- **Repository metadata (`.git`)**: the agent **may not write to any `.git`
  directory** (the repo's refs, objects, hooks, or config) — for the repo it is
  in or any submodule. A write there is **denied** with a pointer to the right
  path: commit on a branch and **submit a pull request** (e.g. `git push -u
  origin <branch>` then `gh pr create`, or `pir submit`). The agent's changes
  must reach trunk through review/CI, never by mutating `.git` directly. (See
  also the repo-isolated `worktree` mode, §11, where this is enforced by the
  filesystem rather than by policy.)

Everything *not* on the list is writable exactly as the invoking user would
writes — including normal project files, user docs, and `/tmp`. The list is
user-editable (`pir.toml [security.guard]`), per-project or global.

Implementation note: at `guard`, this is enforced cheaply — a pre-exec / FUSE /
Landlock **write** rule (or a wrapper around the shell's file tools) that
checks the target path against the deny list. No full jail, no namespace, no
cgroup needed. It degrades gracefully: if the encloser isn't available,
falls back to a warning log rather than breaking the agent.

### 9.4 Effective default for pir
```toml
security.level   = "guard"   # normal user + critical-target guardrail
security.apt     = "auto"    # apt just runs
security.network = "on"      # egress open, like a normal user
security.ask     = "ask"     # blocked ops become a prompt/queue, never a dead end
```
So out of the box the agent can read everything the user can, install packages,
use the network, and edit project/user files — but it physically **cannot**
corrupt a production DB, wipe keys, or overwrite its own test oracle.
Users who want containment opt into `sandbox`/`strict`; users who want zero
guardrails set `level = "off"`.

### 9.5 Switching posture at runtime
The launcher reads these at startup. Moving from `guard` to `sandbox` is a
one-line change; the §7 denial broker and §7.3 parcels apply once enabled, so
the review UX is identical. `pir rights` (§7.4) reports the active posture,
the guard list in effect, and any grants.

---

## 10. Verification Plan

- [ ] Read `C:\Windows\System32\drivers\etc\hosts` from inside the agent → **Access Denied**.
- [ ] Read a file in `C:\Users\<user>\Documents` → **Access Denied**.
- [ ] Read the user's SSH/`AppData` credentials → **Access Denied**.
- [ ] `cd` / `cat` a file one level above the project root → **Access Denied**.
- [ ] Create, edit, delete files *inside* the project directory → **Succeed**.
- [ ] Run the compiler from the toolchain dir (read-only grant) → **Succeed**.
- [ ] Write to the scratch/temp dir → **Succeed**; write to any other dir → **Denied**.
- [ ] Agent spawns a background process, then the launcher exits → background process is **Killed** by the Job Object.
- [ ] Agent tries to disable the network/lock the screen → blocked by UI restrictions.
- [ ] Web search / package fetch to *allowed* host → **Succeed**; to any other host/port → **Blocked** by egress allow-list.
- [ ] Agent "writes" a config to `%APPDATA%` → **Succeeds for the agent**; the real file is **unchanged**; the write lands in the staging dir.
- [ ] `pir apply` (user-side) shows the staged file as a diff; accept → file copied to real target; reject → discarded, real target untouched.

### 10.1 Linux / WSL2 verification
- [ ] Inside the agent: `ls /` shows only bind-mounted project + toolchain; `/home/realuser` is **unreachable**.
- [ ] Agent runs as non-root mapped uid; cannot `chmod`/`read` files outside the jail.
- [ ] `cat /etc/shadow` or other host files → **Permission denied** (not mounted / not allowed).
- [ ] Agent writes to the overlay mount → real lowerdir **unchanged**; write appears in `upperdir`; `pir apply` copies accepted ones.
- [ ] `curl` to an allowed host via the egress proxy → **Succeeds**; direct egress to any other host → **Dropped** by nftables.
- [ ] Launcher exits → cgroup `kill` removes the whole tree; `ps` shows no agent children.
- [ ] cgroup caps enforced: memory > 2G → OOM-killed; `fork()` past `pids.max` → **fails**.
- [ ] `apt-get install` inside the jail → writes land **only** in the overlay upper; real `/` is **unchanged**; `pir apply` shows the per-file diff.
- [ ] Unaccepted overlay install → `apt-upper` wiped on teardown; real system untouched.
- [ ] Agent issues `ai-perm-request apt-install curl` → not installed unattended; appears in the operator queue; `sudo ai-apt-install curl` (human) logs to `/var/log/ai-apt-install.log` and validates the package name.
- [ ] Default posture (`security.level=guard`, `security.apt=auto`, `security.network=on`): agent reads world-readable + `ai`-group files, runs apt, uses the network, and edits project/user files — but writing a production `*.sqlite`/`*.db` or `~/.ssh` is **denied + alerted**, and overwriting its own test oracle is **denied**.
- [ ] `security.level=off`: behaves exactly like plain pi — no guardrail at all.
- [ ] `security.level=sandbox` + `security.apt=stage`: the same install lands in overlay upper and requires `pir apply`.
- [ ] Blocked op (e.g. write to `~/.ssh`) at `security.ask=ask` → not a hard crash; becomes a prompt (TTY) or a queued `ai-perm-request` the operator can approve/deny; `pir ask write ~/.ssh/foo --reason …` is honored the same way.
- [ ] `security.ask=auto-no`: same op is denied + logged, but the agent is told it can `pir ask` for it (path forward, no silent dead end).
- [ ] `security.ask=auto-yes` (or `level=off`): unanticipated requests granted to the invoking user's level with a log; pure pi behavior.

---

## 11. Worktree-only contributor mode (`security.level = "worktree"`)

`worktree` takes the §9.3 guardrail to its logical extreme for *repository
integrity*: the agent is **physically incapable of writing to the real repo or
its trunk**. Every change it makes lives in an isolated git **worktree** + branch,
and the only way a change reaches `main`/`trunk` is through a pull/merge request
that passes a gate. This is repo-level isolation that composes cleanly with the
per-host isolation of `guard`/`sandbox`: you can run `worktree` *inside* a sandbox.

### 11.1 Layout

```
<repo>/.git                         # the only "real" repo; agent has NO write to it
<repo>/                             # trunk checkout; agent has READ-ONLY (or no) access
pir-wt/<agent>/                     # git worktree, branch agent/<id>  ← agent's writable tree
     src/ ...                       # agent edits here, builds here, tests here
```

- At launch, `pir` (as the invoking user, or the per-project `ai_*` UID) creates
  a linked worktree off trunk with a fresh branch `agent/<id>` (`git worktree
  add -b agent/<id> pir-wt/<agent> trunk`). The agent's `cwd` is set into the
  worktree. The trunk checkout is remembered separately for merging.
- The trunk checkout and `.git` are mounted/ACLed **read-only** to the agent
  (or simply not writable by the agent's uid). The worktree is the *only* writable
  tree it can see for this repo.
- Writes outside the worktree (other projects, the trunk, `.git` internals) fall
  under the §9.3 guardrail / DAC, exactly as at `guard`.

### 11.2 The submit → gate → merge flow

1. **Submit (PR).** When the agent decides a change is ready (or on a schedule),
   it commits on `agent/<id>` and calls `pir submit` (or directly `gh pr create`
   / pushes the branch and opens a PR). It does **not** push to trunk. In
   full-auto, `pir submit` can run unattended if the `net-github` parcel (§7.3) is
   granted.
2. **Gate (CI).** A merge gate — GitHub Actions / CI / a `pir merge-gate` hook —
   runs the project's **build + test suite** on the branch. This is the trust
   boundary: the agent's code is validated by *your* pipeline, not by the agent
   vouching for itself.
3. **Auto-accept (if green).** If the gate is green and the PR is small/clean
   (configurable: `"auto-merge": { "if-tests-pass": true, "max-files": N,
   "max-lines": M }`), a merger component fast-forwards/merges it into trunk and
   deletes the worktree. No human in the loop for the happy path.
4. **Fixer agent (if red).** If tests fail, the PR is handed to a **dedicated
   merge-request fixer agent** (see §11.4) that owns resolving the breakage —
   either by amending the original agent's branch or by a follow-up PR — and
   resubmits. The *authoring* agent and the *fixing* agent are separate so a
   stuck/looping author can't just keep force-merging.
5. **Human override.** A human can always review the queue, block auto-merge,
   request changes, or `pir merge <pr>` manually. `worktree` is "auto-accept if
   green," never "merge blindly."

### 11.3 Why this is stronger than any FS guardrail
- A careless agent literally **cannot** corrupt trunk, wipe history, or clobber
  a teammate's work — there is no write path to `.git` or the trunk checkout.
- Every change is a **reviewable, atomic, reversible unit** (a PR). Bad merges
  are a `git revert`, not a restore-from-backup.
- The test gate means the agent can't fake success by editing artifacts (the
  §9.3 "test oracle" rule) *and* can't merge anything that breaks the build.
- It gives **multi-agent fan-out for free**: N authoring agents each own a
  worktree + branch, all converging on one trunk through one gate, with one
  fixer agent keeping trunk green. No two agents write the same tree, so there
  is no in-repo race.

### 11.4 Idle-agent policy — errors, then warnings, then lints

When an authoring agent has no assigned task (no open PR to extend, no user
instruction), it should not sit idle — and it should not invent risky features.
It is auto-tasked, in priority order, from a live "code health" queue:

1. **If there are failing tests / build errors (snuck-in breakage):** assign the
   agent a worktree branch to **fix the error**. Highest priority — a red trunk
   is the worst state. It opens a PR whose gate is the build+test it just fixed.
2. **Else if idle and the tree builds but has warnings / compiler lints:** assign
   it to **clear warnings and lints** (unused vars, dead code, clippy/rustc,
   `-Werror` gaps, deprecations). These go through the same PR gate; auto-merge
   only if the suite still passes and no new warnings are introduced.
3. **Else if idle and clean:** optionally assign **low-risk hygiene** (doc
   comments, formatting via `cargo fmt`/`prettier`, trivial TODOs) — only if the
   operator opts in (`worktree.idle = "hygiene"`); default is to **stay idle**
   once the tree is clean rather than gold-plate.

Hard rules for the idle policy:
- An idle agent **only ever writes to its own worktree + branch** and submits a
  PR — it never edits trunk directly, even to "fix a typo."
- It must **not** widen scope: a "fix this warning" task stays scoped to that
  warning. The gate rejects PRs that touch files outside the targeted area
  unless explicitly allowed (`worktree.idle-scope = "tight"` by default).
- The fixer agent (§11.2.4) is the *only* agent allowed to amend another agent's
  failing PR; authoring agents fix their own, and the fixer unblocks when an
  author is wedged.
- Rate-limit: at most one idle-PR in flight per agent, so a swarm of idle agents
  doesn't spam the gate (`worktree.idle-max-open-prs` per agent, default 1).

### 11.5 Config (`pir.toml`)
```toml
[security]
level = "worktree"              # repo-isolated contributor mode

[security.worktree]
auto-merge        = { if-tests-pass = true, max-files = 25, max-lines = 400 }
idle              = "warnings"   # off | errors | warnings | hygiene
idle-scope        = "tight"      # tight | loose
idle-max-open-prs = 1
fixer-agent       = true         # dedicated agent owns failing-merge PRs
gate              = "ci"         # ci (the host's pipeline) | "pir-merge-gate"
```

### 11.6 Composing with the rest of the model
- `worktree` governs **repo writes**. Pair it with `security.apt`/`network`/
  `ask` for host behavior exactly as at `guard` — the agent can still `apt`
  install into its worktree and use the network, subject to those dials.
- For hostile/multi-tenant runs, run `worktree` **inside** `sandbox`/`strict`
  (§2/§5): the worktree is the writable bind-mount, trunk + `.git` are
  read-only, and cgroups/nftables cap the rest. This is the maximum-isolation
  posture: can't touch the host, the repo, everything is a PR.

---

### 10.2 `worktree` mode verification
- [ ] Agent's `cwd` is inside `pir-wt/<agent>/`; `git worktree list` shows the
      branch `agent/<id>` off trunk.
- [ ] Agent writing into the trunk checkout or `.git/` → **Denied** (read-only /
      wrong uid); write into its worktree → **Succeeds**.
- [ ] `pir submit` (or `gh pr create`) opens a PR from `agent/<id>`; nothing is
      pushed to trunk.
- [ ] Gate (build+test) green on the branch → PR **auto-merges** into trunk and
      the worktree is removed; trunk `git log` shows the merge.
- [ ] Gate red → PR **not** merged; a fixer agent is assigned and resubmits a
      green PR; original author cannot force-merge.
- [ ] Idle agent with a red trunk → opens an **error-fix** PR (highest priority).
- [ ] Idle agent with a green, warning-laden trunk → opens a **warnings/lints**
      PR only after errors are cleared; scope stays within `idle-scope`.
- [ ] Clean trunk + `idle = "warnings"` → agent stays idle (no gold-plating).
- [ ] Multiple agents → each owns a distinct worktree/branch; no two write the
      same tree; trunk never receives an un-gated direct write.
- [ ] Human `pir merge <pr>` / block still works; auto-merge is bypassable.

---

## 12. Implemented default posture (overlayfs write-quarantine + worktree)

The default `pir` posture combines the §2.3 / §5.1 overlayfs staging trick with
the §11 worktree isolation so that **the agent may run every command, but
non-whitelisted writes are intercepted and quarantined** — visible only to the
agent until the operator reviews and applies (or discards) them:

- **Run everything, quarantine writes.** There is no command allow-list: the
  agent reads, executes, and uses the network normally. Every *write* is routed
  through an overlayfs `upperdir` so the real filesystem is untouched until the
  operator says so. This is the `overlayfs` approach the task asks for.
- **Per-agent worktree, whitelisted — opt-in.** Each agent owns a git worktree
  (`wt_create`; auto-created at launch only when worktrees are enabled).
  Worktrees are **off by default** (the guard posture is "pi plus a seatbelt":
  the in-process guardrail protects `.git` and the test oracle); enable them
  with `security.level = "worktree"`, `PIR_WT=1`, or the `/menu` Worktrees
  toggle. When enabled, the agent's worktree is
  bind-mounted **read-write on top** of the overlay, so it is the *only* tree
  the agent can write to the real filesystem through. The central `.git`, the
  trunk checkout, and every *other* agent's worktree are **not** whitelisted —
  writes there are quarantined and visible only to this agent.
- **Review at leisure.** `/quarantine` (alias `/q`) lists the staged writes
  (`status`), copies the non-critical ones to the real fs (`apply`), or throws
  them away (`discard`). Nothing reaches the real filesystem without an explicit
  operator action.
- **Idle fix loop, tiered.** When a turn finishes (or the agent is waiting for
  feedback), the `wt` extension drives the worktree to green in priority order:
  **cargo build errors → compiler warnings/lints → failing tests**. When the
  worktree is green it **merges into `main`** and then runs the *same* tiered fix
  loop **on `main`** (errors → warnings → tests) until trunk is clean.

Config knobs (loaded from `~/.pi/agent/security.toml`, or env): `quarantine`
(default on), `quarantine-project` (default on; `PIR_QUARANTINE=0` disables),
`security.level`, `security.idle` (`off`/`errors`/`warnings`/`hygiene`),
worktrees **off by default** (opt in with `PIR_WT=1`, `security.level =
"worktree"`, or the `/menu` Worktrees toggle), and
`PIR_WT_WHITELIST` (set automatically by `wt` to the agent's worktree).
