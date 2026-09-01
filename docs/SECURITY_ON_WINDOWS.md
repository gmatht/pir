# SECURITY_ON_WINDOWS.md — security architecture for native Windows

**Status: implemented at Layer 1 (Job Object) and the AppContainer primitives;
remainder are explicit seams.** The Windows backend of the security layer
(`src/security/windows.rs`) is real code, not a stub. Everything listed under
§1 below is implemented and unit-tested on native Windows; the heavier layers
(ProjFS callbacks, WFP rule installation, ETW/Sysmon wiring) are documented
call sites that degrade gracefully to the in-process guardrail — the same
"at worst naggy, never broken" doctrine as the Linux overlay. Platform
differences stay inside the security module (`WindowsPlatform` + the
`#[cfg(windows)]` `SecurityPolicy.windows` options), so the other `pir`
modules remain platform-independent.

Read first: `docs/SECURITY_MODEL.md` (the full model, incl. §2 AppContainer +
Job Object + ProjFS), `docs/NONROOT_SECURITY.md`, `docs/ROOT_SECURITY.md`,
`docs/MORE_SECURITY.md`, `docs/SECURITY_INTENT.md`, `docs/SKYNET-AI-PERMS.md`.

---

## 1. What is actually in place on Windows today

The cross-platform core in `src/security.rs` compiles and runs on Windows:

- `SecurityPolicy` (level `off|guard|sandbox|strict|worktree`, read/ask/apt/
  network modes), `Op`/`Ask`/`Verdict`/`Parcel`/`Risk`, `SecurityContext::check`
  as the single entry point, `load_policy`, and the path heuristics
  (`is_secret`, `is_database`, `is_repo_git`, `is_system_state`,
  `is_other_users`) — now **Windows-path aware** (backslash forms of every
  pattern, `C:\Users\<other>` vs own `USERPROFILE`, `C:\Windows\...`
  system state with `C:\Program Files\...` deliberately *not* system state,
  `.git` detection that splits on either separator).
- The **in-process guardrail** at `guard` (the default): writes to production
  DBs, credentials, other users' trees, system state, `.git`, and the agent's
  own test oracle are denied or routed to the ask gate; everything else is
  real. This is OS-agnostic and already works on Windows (verified by the
  `win_guard` tests).
- `WindowsPlatform` (`src/security/windows.rs`) is **real**, providing:

  | Primitive | Status | What it does |
  |---|---|---|
  | **Job Object** (§2.2) | ✅ implemented, **on by default** | wraps the session with `KILL_ON_JOB_CLOSE`; optional active-process / job-memory / process-memory / job-time limits and UI restrictions (block logoff+shutdown by default) |
  | **AppContainer profile** (§2.3) | ✅ implemented | `CreateAppContainerProfile` / SID derive+string / folder path / delete-on-drop-when-we-created-it; empty or `internetClient`-style capability list |
  | **AppContainer ACL grant** (§2.3) | ✅ implemented | `SetEntriesInAclW` + `SetNamedSecurityInfoW` merge (never clobbers the owner's DACL); read-only / RW / RWX with inheritance (the §2.2 pattern) |
  | **AppContainer launch** (§2.3) | ✅ implemented (launcher seam) | `CreateAppContainerToken` + `SetTokenInformation(TokenCapabilities)` + `CreateProcessAsUserW`; returns a handle-wrapped child to assign into the Job |
  | **Low Integrity Level** (§7/§2.7) | ✅ implemented (opt-in) | drops the process token to `S-1-16-4096` + labels the process DACL; medium objects become write-protected |
  | **ProjFS detection** (§2.4) | ✅ implemented | probes `ProjectedFSLib.dll` + the `PrjFlt` service start type (feature enabled?) without elevation |
  | **ProjFS staging** (§2.4/§2.3-option-1) | ⚠️ seam | `windows::staging` module mirrors `overlay.rs`'s `status|apply|discard` (+ tombstone-safe apply); ProjFS *callbacks* (`PrjStartVirtualizing`) are the launcher's job |
  | **WFP egress allow-list** (§2.5) | ⚠️ seam | `NetworkPolicy` + `apply_network_policy()` is the call site; reports the elevation precondition and defers rule installation to an elevated launcher/enforcer (request-don't-take) |
  | **ETW / Sysmon audit** (§2.6) | ✅ audit log / ⚠️ ETW wiring | every denial is appended to `%LOCALAPPDATA%\pir\audit\security.log` as `{ts, who, parcel, scope, reason, ttl}`; ETW/Sysmon configuration is the operator's |
  | **Request queue** (§2.6/§4) | ✅ implemented | headless denials are queued as JSON into `$AI_PERM_REQUEST_DIR` (default `%TEMP%\ai-perm-requests`) — the same `permctl`/`ai-perm-request` spool used on Linux |
  | **Elevation detection** (§3) | ✅ implemented | `is_elevated()` via `TokenElevation`; pir never elevates itself |

  Config surface: `security.windows.*` keys in `~/.pi/agent/security.toml`
  (`job`, `job-active-process`, `job-memory-mb`, `job-process-memory-mb`,
  `job-time-ms`, `ui-exit-windows`, `ui-clipboard`, `ui-desktop`,
  `appcontainer`, `appcontainer-caps`, `low-integrity`, `audit`), parsed by
  `windows::parse_option` behind the same `load_policy` entry point. The
  global escape hatch `PIR_WIN_SECURITY=0|off` disables the host-level layers
  while keeping the in-process guardrail.

**Consequence:** on native Windows, `pir` is now "pi plus the in-process
guardrail **plus** a real Job Object" by default: the accidental harms are
blocked (DBs, keys, `C:\Windows`, other users, `.git`, test oracle) *and* no
process tree can outlive the session. AppContainer/ProjFS/WFP remain the
opt-in `sandbox`/`strict` posture and the launcher's territory.

---

## 2. Recommended architecture: layered, default `guard`

The Linux model is a stack of primitives; the Windows model should be the same
stack, with each primitive swapped for its native equivalent. Layers compose —
you can adopt the cheap ones (Job Object, in-process guardrail) without the
heavy ones (AppContainer, ProjFS).

| Linux (implemented) | Windows (recommended) | Code seam | Status |
|---|---|---|---|
| per-project `ai_*` UID + `setuid` drop | **AppContainer** profile per session (SID + ACL grants) | `Platform` trait → `WindowsPlatform` | ✅ implemented (opt-in launcher posture) |
| overlayfs write-quarantine (system + project + home) | **ProjFS** virtualized staging + `pir apply` | `overlay.rs` → `windows::staging` | ⚠️ staging store + detection implemented; ProjFS callbacks = launcher seam |
| private mount namespace (scopes the view) | AppContainer ACLs *are* the view (no namespace needed) | launcher | ✅ (via AppContainer) |
| cgroups v2 (`memory.max`, `pids.max`) | **Job Object** limits (`JOB_OBJECT_LIMIT_*`) | launcher | ✅ implemented, on by default |
| `KILL_ON_CLOSE` / cgroup kill on exit | **Job Object** `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` | launcher | ✅ implemented, on by default |
| nftables egress allow-list + egress proxy | **WFP / Windows Firewall** allow-list + egress proxy | launcher | ⚠️ seam (`apply_network_policy`) |
| auditd / fanotify | **ETW / Sysmon** (`Microsoft-Windows-Kernel-File`, object-access audit) | audit | ✅ audit log implemented; ETW/Sysmon = operator config |
| Landlock (unprivileged self-confine) | AppContainer (native, no driver) | — | ✅ (via AppContainer) |
| rootreq + sudoers (request-don't-take) | **UAC**-gated, allowlisted, logged elevated wrapper | `extensions/rootreq` | ⚠️ pattern implemented (`is_elevated`, request queue); wrapper = launcher |
| worktree + PR gate | same (git is cross-platform) | `extensions/wt` | ✅ git is cross-platform |

### 2.1 Layer 0 — in-process guardrail (already shipped, keep)

`SecurityContext::check` + `decide` + the ask gate. This is the "pi plus a
seatbelt" default and needs no Windows-specific work. It is the fallback that
keeps the agent safe even when no host-level confinement is engaged (exactly the
role it plays on Linux when the overlay can't mount).

### 2.2 Layer 1 — Job Object (cheap, adopt first) — ✅ implemented, on by default

A Job Object groups the agent and every descendant under one controller. In
`src/security/windows.rs` this is `Job`/`JobLimits`/`enable_lifecycle_job`,
engaged automatically at `SecurityContext::new` (`security.windows.job`, or
`PIR_WIN_SECURITY=0|off` to disable):

- `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` — the whole tree dies the instant the
  launcher exits or crashes (no orphaned shells/compilers — the Windows
  equivalent of the Linux cgroup/pdeathsig teardown). The session job is named
  `pir-session-<pid>` so concurrent pir sessions never share a handle (which
  would delay `KILL_ON_CLOSE`); an operator can still open it by name to
  `TerminateJobObject` a runaway tree.
- `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` (e.g. 64) — fork-bomb bound.
- `JOB_OBJECT_LIMIT_JOB_MEMORY` / `PROCESS_MEMORY` — RAM bound (MiB).
- `JOB_OBJECT_LIMIT_JOB_TIME` — optional CPU budget.
- `JOB_OBJECT_UILIMIT_*` — block desktop switching, clipboard, and
  `ExitWindows` (the agent cannot lock the screen or log the user off).
  `ui-exit-windows` is on by default; clipboard/desktop are opt-in because they
  also affect the operator's console.

> Drop semantics: when the last handle to a kill-on-close job closes, the OS
> terminates every process in the job — *including the process closing it*.
> The context that owns the job therefore holds it for the whole run (and the
> OS reclaims it at process teardown, which is exactly when the tree should be
> reaped). Never drop a session job early.

### 2.3 Layer 2 — AppContainer (the isolation boundary) — ✅ primitives implemented

An AppContainer is a low-privilege process identity backed by a per-profile SID.
By default it has **no** access to the filesystem, registry, or network except
what is explicitly granted — the native equivalent of the Linux
"bind-mount only what you allow" jail.

Implemented in `src/security/windows.rs` (`AppContainerProfile`,
`grant_dir`, `launch_in_appcontainer`):

- **Create the profile once per session**: `create()` wraps
  `CreateAppContainerProfile` (or adopts an existing one without owning its
  teardown — `Drop` deletes only profiles *we* created).
- **Grant ACLs narrowly** (the §2.2 pattern from `SECURITY_MODEL.md`):
  `grant_dir(dir, DirAccess::ReadWriteExec, inherit)` adds ONE ACE for the
  AppContainer SID to the directory's DACL via `SetEntriesInAclW` +
  `SetNamedSecurityInfoW`, **merging** with the existing DACL (never clobbers
  the operator's own access):
  - project root: `GENERIC_READ|GENERIC_WRITE|GENERIC_EXECUTE`, inherited;
  - toolchains/SDKs (`C:\Program Files\...`, `C:\msys64\...`, Rust/Go/Node/
    Python dirs): `GENERIC_READ|GENERIC_EXECUTE`;
  - per-session scratch (`%LOCALAPPDATA%\pir\scratch\<session>`): RW;
  - **never** grant `C:\Users` home data, registry hives, or secret stores —
    under AppContainer they are already denied.
- **Launch inside**: `launch_in_appcontainer(profile, cmd, cwd, caps)` —
  `CreateAppContainerToken` + `SetTokenInformation(TokenCapabilities)` +
  `CreateProcessAsUserW`, returning a handle-wrapped child the launcher can also
  assign into the Job Object.

Seam: `WindowsPlatform` owns the profile lifecycle and the ACL grant list, and
`SecurityContext`'s `Platform` trait is the call site. This is the
"sandbox"/"strict" posture; it is **not** the default (the guardrail + Job
Object are).

### 2.4 Layer 3 — ProjFS staging (fake out-of-tree writes) — ⚠️ staging store implemented, callbacks are the seam

The Linux overlayfs trick — "let the agent write, but stage it" — maps to
**Projected File System (ProjFS)**, a user-mode Windows API that projects a
virtual filesystem into a directory:

- `projfs_available()` (implemented) detects the optional feature
  (`ProjectedFSLib.dll` + the `PrjFlt` service start type).
- `windows::staging` (implemented) mirrors `overlay.rs`'s
  `status|apply|discard` surface (`/quarantine` stays unchanged) with a
  tombstone-safe apply: staged files are copied to their real targets only on
  `pir apply`; missing staged files are treated as tombstones and skipped.
- `PrjStartVirtualizing` callbacks (the transparent union lower-then-upper
  layer) are the launcher's job; the module is the store they write into. Until
  ProjFS is engaged, out-of-tree writes are denied by the in-process guardrail.

The `SECURITY_ON_WINDOWS.md` §2.3 option-3 (no-driver manifest mode) is also
available as `staging::register` if a transparent FS is overkill.

### 2.5 Layer 4 — network egress (WFP allow-list + proxy) — ⚠️ seam

`internetClient` alone permits *any* outbound connection. Pair it with a
**Windows Firewall / WFP allow-list** restricting egress to the specific
hosts/ports the agent needs (`crates.io:443`, `github.com:443`, a search API),
and optionally route egress through a proxy the agent is configured to use, so
the proxy — not the agent — holds the allow-list and logs every request. Keep
`internetClientServer`/`privateNetworkClientServer` off (the agent should not
accept inbound connections). This is the `net-*` parcel enforcement from
`SECURITY_MODEL.md` §7.3.

Implemented as the *contract*: `windows::NetworkPolicy` describes the desired
rules and `windows::apply_network_policy()` is the call site — it reports the
elevation precondition and defers rule installation to an elevated
launcher/enforcer (request-don't-take, exactly like `extensions/rootreq`).
Installing WFP filters from inside the agent would hand the agent admin, which
this model never does.

### 2.6 Layer 5 — audit (ETW / Sysmon) — ✅ audit log implemented; ETW wiring is operator config

The Linux auditd/fanotify story maps to **ETW** (`Microsoft-Windows-Kernel-File`,
object-access auditing) and **Sysmon** (process creation, file/network events).
The denial broker (§7 of `SECURITY_MODEL.md`) already knows exactly what was
tried; `windows::audit()` appends every denial to
`%LOCALAPPDATA%\pir\audit\security.log` as `{ts, who, parcel, scope, reason,
ttl}` — the same audit record the Linux path writes — and headless denials land
in the `ai-perm-request` queue (`windows::queue_perm_request`) for the
operator-side enforcer, exactly as `permctl` does on Linux. ETW/Sysmon
*channels* for kernel-level capture (the AppContainer rejection + ProjFS
callbacks) are the enforcer's configuration.

### 2.7 Defense-in-depth (optional)

- **WDAC / AppLocker** — allow-list what can *run* (complements, does not
  replace, isolation). Operator-side policy authoring.
- **Low Integrity Level** — ✅ implemented (`windows::lower_to_low_integrity`,
  opt-in via `security.windows.low-integrity`): cheap extra write-blocking for
  medium objects.
- **Windows Sandbox** — the right tool for *disposable* installs (see §3):
  run `winget`/`choco`/an installer inside a throwaway Sandbox and copy the
  artifacts out, instead of granting the agent admin.

---

## 3. Privilege escalation on Windows (the rootreq equivalent)

The Linux model is "the agent never escalates itself; it queues a request an
operator fulfills out-of-band" (`extensions/rootreq` + sudoers). Windows has no
sudoers, but the *pattern* transfers:

- The agent calls `request_root`-style intents (`apt-install` → `winget`/`choco`,
  `mk-user`, `command`) which **queue a structured, auditable request** — never
  an actual elevation.
- An operator-side enforcer (the `rootreq-enforcer` equivalent) runs **elevated
  via UAC** (or a scheduled task with stored credentials), validates the request
  against an allowlist (safe token charset, pinned packages), executes, and logs
  to a file. The agent never holds admin.
- `run_as` (run a command as a permitted identity) maps to running under the
  AppContainer token — no new privilege.

There is no `ai-apt-install` sudoers wrapper on Windows yet; the *pattern*
(request → human → validated → logged) is identical and should be implemented
the same way, with UAC as the human gate. The Windows backend already provides
`is_elevated()` (detector — pir never elevates itself) and the queued-request
channel (`windows::queue_perm_request` → `$AI_PERM_REQUEST_DIR`), so a future
`rootreq-enforcer.exe` (elevated via UAC, allowlist-validated) has exactly the
same contract as its Linux `/usr/local/sbin` sibling.

---

## 4. How the existing cross-platform core maps

- **`Platform` trait** (`canonicalize`, `is_other_users`, `is_system_state`,
  `describe`) — the seam is real; `WindowsPlatform` implements it and owns the
  AppContainer/ACL/Job-Object logic behind it. Other modules never see Win32.
- **`SecurityContext` / `check` / `decide`** — unchanged; they are the single
  entry point on every platform. `SecurityContext::new` engages the Windows
  Job Object (Layer 1) at construction.
- **`SecurityPolicy.windows`** — a `#[cfg(windows)]` options struct
  (`WindowsOptions`), parsed by `load_policy` from the same `security.toml`
  file; on unix builds the field does not exist, so no other module changes.
- **`RequestSink`** — `TtySink` works on a Windows console; `QueuedSink` for
  headless now queues into the portable `ai-perm-request` spool.
- **Parcels / ask** — the `net-*`, `scratch-rw`, `config-staging`, `secret-read`
  parcels map 1:1; the *enforcement* mechanism differs (WFP rule vs nftables
  rule, ProjFS upper vs overlay upper, AppContainer ACL vs bind-mount).
- **`/quarantine`** — keep the command; the `windows::staging` module mirrors
  `overlay.rs`'s `status|apply|discard` surface (tombstone-safe), so a future
  ProjFS backend plugs in underneath without touching `/quarantine`.
- **`extensions/wt` (worktree mode)** — git is cross-platform; the worktree +
  PR-gate isolation works unchanged on Windows. The only difference is that
  "the agent cannot write the trunk" is enforced by AppContainer ACLs (or the
  in-process guardrail) instead of a bind-mount.

---

## 5. Implementation order — done / remaining

1. ✅ **Job Object wrapper**: `KILL_ON_CLOSE` + process/memory limits +
   UI restrictions — implemented in `src/security/windows.rs`, on by default.
2. ✅ **`WindowsPlatform` real**: profile lifecycle + ACL grant list behind
   the existing `Platform` trait; the in-process guardrail remains the fallback.
3. ✅ **AppContainer launch** for `sandbox`/`strict`:
   `CreateAppContainerToken` + `CreateProcessAsUser` with
   `SECURITY_CAPABILITIES`; narrow ACL grants (project RWX, toolchain RX,
   scratch RW) — implemented; engaging it in a launcher wrapper is the next
   step.
4. ⚠️ **ProjFS staging**: the `windows::staging` module (mirrors `overlay.rs`
   `status|apply|discard`, tombstones, graceful fallback) is done; the
   `PrjStartVirtualizing` callbacks that *produce* staged writes are the
   remaining launcher work.
5. ⚠️ **WFP egress allow-list** + egress proxy for `network = allowlist`:
   `NetworkPolicy`/`apply_network_policy()` are the seam; rule installation
   stays with an elevated enforcer (request-don't-take).
6. ⚠️ **ETW/Sysmon audit + UAC-gated elevated wrapper**: the audit log and the
   Windows `ai-perm-request` queue are implemented; the UAC-gated
   `rootreq-enforcer.exe` and Sysmon channels are operator-side.
7. ✅/**opt-in** **low-IL** defense-in-depth implemented; WDAC/AppLocker policies
   and Windows Sandbox for disposable installs remain operator tooling.

Each phase is independently shippable and degrades gracefully to the layer
below it — the same "at worst naggy, never broken" doctrine as Linux.

---

## 6. Verification checklist (Windows)

- [x] In-process guardrail: writing a `*.sqlite`/`*.db`, a secret (`.ssh`/
      `.aws`/`.key`/`.pem`, incl. `C:\Users\...\` backslash forms), `.git`,
      `C:\Windows\...`, another user's profile, or the test oracle is
      **denied + alerted**; normal project writes succeed. (Covered by the
      `win_guard` + `windows_path_heuristics_backslash` tests.)
- [x] Job Object: lifecycle job engages at context construction
      (`lifecycle_job_engages_and_tracks`); a child assigned to a kill-on-close
      job is gone after `TerminateJobObject` (`job_object_kills_children`,
      run with `PIR_TEST_JOB=1`); active-process / memory / job-time / UI
      limits are settable and the named `pir-session-<pid>` job is openable by
      an operator.
- [x] AppContainer primitives: profile create → SID string `S-1-15-2-...` →
      folder path → delete roundtrip (`appcontainer_profile_roundtrip`, run
      with `PIR_TEST_APPCONTAINER=1`); `grant_dir` adds a *merged* ACE (does
      not clobber the existing DACL); `launch_in_appcontainer` is the wired
      seam.
- [ ] ProjFS: agent "writes" a config to `%APPDATA%` → succeeds for the agent,
      real file unchanged, write lands in staging; `pir apply` shows a diff and
      copies accepted files; `pir discard` leaves the real target untouched.
      (Staging store + `projfs_available()` detection implemented; the
      virtualization callbacks are the remaining launcher work.)
- [ ] Network: egress to allow-listed hosts succeeds; anything else is
      **blocked** by the WFP rule; `internetClientServer` is off. (Seam
      implemented; rule installation is the elevated enforcer's job.)
- [x] Audit: every denial is recorded to
      `%LOCALAPPDATA%\pir\audit\security.log` as
      `{ts, who, parcel, scope, reason, ttl}` and headless denials are queued
      into `$AI_PERM_REQUEST_DIR`. (ETW/Sysmon channels: operator-side.)
- [ ] Escalation: `request_root`-style intents queue a request; nothing is
      elevated without the UAC-gated, allowlisted enforcer; the agent never
      holds admin. (`is_elevated` detector + queue implemented; the elevated
      enforcer binary is the remaining piece.)
- [ ] Worktree mode: agent's writes land only in its worktree; trunk + `.git`
      are read-only to it; merges go through the PR gate. (git is
      cross-platform; `extensions/wt` gated for non-unix in this build.)

---

## 7. Open questions / decisions

- **AppContainer vs. per-project user.** Windows has no `setuid`; AppContainer
  is the native equivalent of the `ai_*` UID. The backend supports per-session
  profiles out of the box (one profile, deleted when the session ends); per-*project*
  profiles are a one-line launcher change (`AppContainerProfile::create` is
  cheap and idempotent). Default remains per-session unless multi-tenant needs
drive otherwise.
- **ProjFS availability.** It is an optional Windows feature; `projfs_available()`
  detects it on the host and the launcher must fall back to the in-process
  guardrail when absent (the Linux overlay already degrades the same way).
- **Ask UX on Windows.** `TtySink` works on a console; headless denials now
  queue into the portable `ai-perm-request` spool (same as Linux). A toast/
  notification sink remains a small addition if wanted.
- **WSL2 vs native.** If the agent runs under WSL2, the *Linux* model applies
  (namespaces/overlayfs/cgroups) — this doc is for native Windows processes.
- **Admin elevation.** UAC + a logged, allowlisted elevated wrapper is the
  rootreq equivalent; `is_elevated()` + the request queue are implemented, and
  `winget`/`choco` installs should go through that wrapper or Windows Sandbox
  staging (§3) — never through the agent.
- **Job-Object drop semantics.** `KILL_ON_JOB_CLOSE` terminates a job's
  processes when the last handle closes — including the closer. The session
  job therefore lives for the whole process and is reclaimed by the OS at
exit (this is the desired teardown); early drops are a footgun by design.
