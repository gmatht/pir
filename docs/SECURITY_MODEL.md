# Windows Security Model for AI Agents

## 1. Goal

Prevent an AI coding agent (and every child process it spawns) from
**wandering** outside a single, designated project directory while still
allowing it to compile, run, and edit files inside that directory.

The model below targets a *native Windows* agent (`pir.exe`). Where relevant,
notes on WSL/container equivalents are included for contrast.

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

> **Use Job Objects + AppContainer.** AppContainer supplies the directory
> jail (SID-scoped ACLs + capability stripping); the Job Object supplies
> guaranteed cleanup and resource caps. Together they are native, near-zero
> overhead, and specifically solve both "don't read outside the project" and
> "don't leave zombies behind."

Layered extras (optional, recommended for production):
- Launch the launcher itself as a **medium-IL, non-admin** process.
- Add **WDAC/AppLocker** to allow-list the toolchain the agent may execute.
- Mount the project dir as the only writable volume and keep everything else
  read-denied by the AppContainer SID.
- Log all `CreateFile` access-denied events via ETW/Sysmon for audit.

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

## 6. Verification Plan

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

### 6.1 Linux / WSL2 verification
- [ ] Inside the agent: `ls /` shows only bind-mounted project + toolchain; `/home/realuser` is **unreachable**.
- [ ] Agent runs as non-root mapped uid; cannot `chmod`/`read` files outside the jail.
- [ ] `cat /etc/shadow` or other host files → **Permission denied** (not mounted / not allowed).
- [ ] Agent writes to the overlay mount → real lowerdir **unchanged**; write appears in `upperdir`; `pir apply` copies accepted ones.
- [ ] `curl` to an allowed host via the egress proxy → **Succeeds**; direct egress to any other host → **Dropped** by nftables.
- [ ] Launcher exits → cgroup `kill` removes the whole tree; `ps` shows no agent children.
- [ ] cgroup caps enforced: memory > 2G → OOM-killed; `fork()` past `pids.max` → **fails**.
