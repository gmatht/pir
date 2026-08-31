# MORE_SECURITY.md — Agent write-quarantine design discussion

This document captures the design discussion that followed the original
`docs/SECURITY_MODEL.md`. It is a working notes / rationale file, not a spec.
It covers:

- the write-quarantine task and the posture we landed on,
- *why* a private mount namespace (not a chroot) scopes the quarantine to one
  agent,
- what overlayfs does and does not pass through,
- the host/`ai_*` user question, and
- a comparison of **btrfs subvolume snapshots** vs **overlayfs** for this job.

All code references are to the current tree (`src/security/overlay.rs`,
`src/security.rs`, `extensions/wt/src/lib.rs`, `src/agent.rs`, `src/user.rs`).

---

## 0. The task (as posed)

> By default we allow the agent to run all commands, but intercept and quarantine
> non-whitelisted WRITES where only the agent can see them (so the user can apply
> or deny them at leisure). Put each agent in its own work tree. The agent's own
> worktree should be white-listed, but not the central `.git` or other
> worktrees. After the agent reports the prompt is finished (or is waiting for
> user feedback), get it to fix cargo build errors in its work tree. Once all
> errors are fixed, get it to fix warnings, then failing tests. Finally merge the
> worktree with main, and repeat the fix of errors, warnings and test failures on
> main.

Read first: `docs/SECURITY_MODEL.md` §2.3 (overlayfs staging), §5.1 (namespaces),
§9 (threat model), §11 (worktree mode).

---

## 1. Posture we implemented

Two cooperating overlayfs mechanisms. Mounting needs `CAP_SYS_ADMIN`
*in the namespace the overlay lives in* -- which an **unprivileged** process gets
for free by creating a user namespace (`unshare(CLONE_NEWUSER)` makes you root
*inside* that namespace). So no **host** root is required: the unprivileged form
is `unshare(CLONE_NEWUSER | CLONE_NEWNS)`. (Our current
`enter_private_mount_ns` uses `CLONE_NEWNS` alone, which *does* require host root,
hence `can_mount()` checks `id -u == 0`; switching to the user-ns form removes
that requirement -- see S6/S7.)

1. **System-tree quarantine** (`Quarantine` in `overlay.rs`). Overlays the
   default trees `DEFAULT_OVERLAY_DIRS = /etc /usr/local /opt /srv /var /boot`
   with a private `upperdir`. Writes there stage; the agent sees them; the real
   fs is untouched until `/quarantine apply`.

2. **Project quarantine** (`ProjectQuarantine` in `overlay.rs`). Overlays the
   **repo root** (`lowerdir = real root`, `upperdir = staging`) and bind-mounts
   the agent's own git **worktree read-write on top**. So the worktree is the only
   tree written to the real fs; central `.git`, the trunk checkout, and other
   agents' worktrees stage (agent-only view).

Both are reviewed/flushed by the `/quarantine` command (`status` / `apply` /
`discard`), which now flushes **both** layers, and `apply` suspends the overlay
within the namespace during the merge so writes land on the **real** fs.

The `wt` extension drives the idle loop (`health_check` tiers → `auto_flow_generic`
→ `merge_and_heal_main`): build errors → warnings → failing tests, in that order;
when the worktree is green it merges into `main` and then re-runs the **same**
tiered fix loop on `main`. Every agent auto-creates its own worktree at launch
(`PIR_WT_AUTOCREATE=0` disables); the worktree is the only whitelisted tree.

---

## 2. Why a private mount namespace (and not a chroot)

The naive version mounts the overlays in the **global** mount namespace. That
shadows `/var`, `/etc`, … for the **entire host** — i.e. it quarantines
*everybody's* writes, not just the agent's. That is wrong.

Fix: `enter_private_mount_ns()` does `unshare(CLONE_NEWNS)` + `mount --make-rprivate
/` before any overlay is mounted (`agent.rs`, `mount_project_quarantine`). All
overlays then live only in the agent's mount namespace. (Note `CLONE_NEWNS` *alone*
requires host root; the unprivileged equivalent is `CLONE_NEWUSER | CLONE_NEWNS`,
which grants `CAP_SYS_ADMIN` *inside* the new namespace so an unprivileged user can
create the mount ns and mount the overlays without host root -- see S6.)

- only the agent process (and the bash/commands it spawns) sees the staged view;
- the host, and every other process / other `pir` agents, keep the real `/var`,
  `/etc`, repo;
- when the agent exits the namespace (and its mounts) vanish — no global pollution.

**Chroot does not replace this.** A `chroot` only changes which directory a
process resolves as `/`; it does **not** give the process a separate set of
mounts. If you mount an overlay in the global namespace and then `chroot` a child,
the host still sees the overlay. You still need `unshare(CLONE_NEWNS)` to scope
the mounts. So chroot and namespace solve *different* problems:

- namespace ⇒ what mounts a process sees (scopes the quarantine to the agent);
- chroot/pivot_root ⇒ what root path the process resolves (bounds path names).

A **minimal chroot jail** is the heaviest option and the most breakage-prone
(needs `/proc`,`/sys`,`/dev`,`/run`, toolchain, caches bound in by hand), and is
only justified if you also want to *hide* the host from the agent's reads — which
the task does not ask for. The pragmatic middle option is a **full-root overlay
inside the existing namespace** (overlay `/` instead of enumerating trees), which
closes the coverage gap (`/tmp`, `/home`, `/root` currently leak to the host)
without a jail's plumbing.

Recommendation: keep the private mount namespace; optionally switch the selective
overlays for a single full-root overlay (still in the namespace) to quarantine
*every* agent write.

---

## 3. What overlayfs passes through — and what it shadows

Overlayfs serves **regular files and directories** from the lower layer by
read-through: the toolchain (`/usr/bin/*`), `~/.cargo`, CA bundles
(`/etc/ssl/certs`), `git`, and all libraries are byte-identical to the host and
need **no** bind-mounting. `cargo build`, `git`, and TLS just work.

Overlayfs does **not** preserve underlying **submounts**. `/proc`, `/sys`,
`/dev`, `/dev/pts`, `/run`, `/dev/shm` are separate kernel mounts attached to
empty directories in the real root; an overlay mounted over `/` shadows them. So
after overlaying `/` you must re-establish them — and you must do it **before**
promoting the overlay to `/` (a post-hoc `mount --bind /proc /proc` resolves to
the empty lower dir once shadowed):

1. `unshare(CLONE_NEWUSER | CLONE_NEWNS)` + `make-rprivate` (the `CLONE_NEWUSER`
   half is what makes this work *without* host root -- inside the user ns you hold
   `CAP_SYS_ADMIN`).
2. build the overlay at a staging path: `mount -t overlay overlay -o
   lowerdir=/,upperdir=…,workdir=… /mnt/agent-root`.
3. bind the real pseudo-fs into it while still reachable:
   `mount --bind /proc /mnt/agent-root/proc`, `/sys`, `/dev`, `/dev/pts`,
   `/run`, `/dev/shm` (and `/tmp` if you want the real tmpfs).
4. promote: `mount --bind /mnt/agent-root /` (or `pivot_root`).

That is ~6 bind mounts — small and standard (it is exactly what `runc` does).
**Network is orthogonal** to overlayfs (it is a net-namespace concern, and we do
not unshare the net ns), so connectivity passes through untouched.

---

## 4. `is_system_state` and the step-aside rule

`is_system_state` (in `security.rs`) classifies a path as system state
(`/boot`, `/etc`, `/efi`, `/var`, `/usr/local`, `/opt`, `/srv`, …). Its purpose
is the **in-process fallback hard-deny** used by `decide` when **no overlay is
mounted** (e.g. a non-root `ai_*` agent that cannot `unshare`/mount): we can't
stage writes, so we at least deny them.

When an overlay *is* mounted, `decide` now **steps aside** instead:

```rust
if project_quarantine_engaged() || system_quarantine_engaged() {
    return Verdict::Allow;   // overlay stages it; operator reviews via /quarantine
}
```

So `/var` (and friends) writes are **quarantined** (staged, reviewable) in root
mode rather than hard-blocked; `is_system_state` only fires as a fallback when no
overlay is available.

---

## 5. Can root look inside the private namespace?

**Host root: yes, trivially.** Mount namespaces are files in `/proc`:
- `cat /proc/<pid>/mountinfo` shows the agent's mount table;
- `nsenter -m -t <pid> <cmd>` runs a command inside the agent's mount view;
- the staged writes live on the **real** host fs under
  `~/.pi/agent/quarantine-staging/…` (those paths are outside every overlaid
  tree), so the operator can `cat`/`diff` them directly, with or without
  entering the namespace.

**The agent (root, same user namespace): also yes — and that is the catch.**
We only did `unshare(CLONE_NEWNS)`; we did **not** create a new *user* namespace
and did **not** drop capabilities. So the agent is still root in the host's user
namespace and can `setns` back into the host's mount namespace (via
`/proc/1/ns/mnt`) or simply `umount` the overlays. Therefore the private mount
namespace is a **cooperative** quarantine, not a hard boundary against a hostile
root agent. It defends against *mistakes and prompt injection* (the §9 threat
model), not a deliberately adversarial root process.

For genuine containment you add `CLONE_NEWUSER` + capability drop — see §7.

---

## 6. Does the `ai_*` user need to exist outside the namespace?

**Current code: yes.** `src/user.rs` runs `useradd` to create a real
`ai_<project>` system user, and the bash tool's `before_exec` does
`setresuid(uid, uid, 0)` to drop into it. That requires a uid valid/mapped in the
**host** user namespace — i.e. a real `/etc/passwd` entry.

**With a user namespace: no.** `unshare(CLONE_NEWNS | CLONE_NEWUSER)` + a uid/gid
map (`/etc/subuid` + `newuidmap`) gives the agent its own user namespace. The
`ai_*` user is then **namespace-local** (defined in a namespaced `/etc/passwd` or
just an inside uid); the host has no `ai_*` account — no `useradd`, no leftover
accounts, no host home. The agent can even run as **virtual root** inside the ns
(uid 0 in-ns → unprivileged host subuid) so `mount`/`setuid`/`cargo` still
behave as root, while the host sees only an unprivileged user.

Bonus: a user namespace is also what makes the quarantine a **real** boundary —
an unprivileged mapped uid cannot `setns` back to the host or `umount` the
overlays. So moving `ai_*` *into* the namespace removes host-account pollution
**and** closes the escape described in §5. Caveats / how it works without host root:
- `unshare(CLONE_NEWUSER | CLONE_NEWNS)` is unprivileged -- the creating user
  becomes root *inside* the user ns and thus holds `CAP_SYS_ADMIN` there, enough
  to create the mount ns and mount the overlays. **No host root needed.**
- A **uid/gid map** is required for the user ns: a single self-map (your own host
  uid -> 0 inside) needs no setuid helper; a subuid range needs `/etc/subuid` +
  `newuidmap`/`newgidmap`.
- **Overlayfs inside a user namespace** works on modern kernels (5.11+ with
  id-mapped mounts) but some kernels/distros still return `EPERM`; the portable
  fallback is `fuse-overlayfs` (FUSE, fully unprivileged) -- exactly what rootless
  Podman/Docker use for their overlay storage.
- Host root is only needed if the distro **disables** unprivileged userns
  (`user.max_user_namespaces=0`, or Ubuntu's AppArmor `unprivileged_userns`
  restriction); then you either need root or a setuid helper to create the userns.

**Empirical result on WSL2 (Microsoft kernel 5.15, tested via `wsl.exe`):**
unprivileged `unshare(CLONE_NEWUSER)` *succeeds* on every WSL2 distro
(`max_user_namespaces=192685`, USERNS_OK) — so the namespace can be created
without host root. A `tmpfs` mount *inside* the user ns also succeeds
(`TMPFS_NS_OK`). But an **in-kernel overlay mount inside the user ns is
rejected** (`wrong fs type / bad superblock on overlay`), even as virtual
root. Therefore on WSL2 you still need **real (init-ns) root** to mount the
in-kernel overlay; the unprivileged path requires **fuse-overlayfs**, which
*does* mount inside the user ns (`FUSE_NS_OK`, verified). `fuse-overlayfs`
ships on Ubuntu 24.04 but was absent on AlmaLinux-8/9 and Ubuntu-16.04 in
testing (install it there). On a native distro kernel (not WSL2) in-kernel
overlay-in-userns generally *does* work (5.11+ with id-mapped mounts), so
this is a WSL2-kernel-specific limitation, not a general one. Plain root
overlay mount in the init namespace works fine on WSL2 (`ROOT_OVERLAY_OK`).

---

## 7. Open questions / next steps

1. **Real containment:** `unshare(CLONE_NEWNS | CLONE_NEWUSER)` + cap drop, with
   `ai_*` defined inside the namespace instead of via `useradd`. Turns the
   cooperative quarantine into a hard one.
2. **Full-root overlay** in the namespace (overlay `/`, re-bind special fs) to
   quarantine *every* write without enumerating trees.
3. **Apply correctness:** `apply` already suspends the overlay within the
   namespace during the merge so it writes to the real fs; verify this on the
   full-root variant too.

---

## 8. btrfs subvolume snapshots vs overlayfs

Both can implement "agent writes are isolated; operator applies/discards at
leisure." Trade-offs for *this* workload (per-agent, worktree-whitelisted, long
coding sessions):

### overlayfs (what we built)
- **Pros:** portable — works on top of *any* backing fs (ext4/xfs/btrfs/…); no
  restructuring of the workspace; instant setup (no data copy; COW only copies
  the changed file up to `upper`); live transparent view; trivial discard (clear
  `upper`); `apply` = merge `upper`→`lower` (we already implement whiteouts).
- **Cons:** needs mount privs (root or user ns); shadows submounts so `/proc`,
  `/sys`, `/dev`, `/run` must be re-bound before promotion (§3); **copy-up** copies
  the *whole* file on first write — expensive for large files (big build
  artifacts, databases, VM images); deletions need whiteout bookkeeping; inode
  numbering is synthetic (some tools notice); can't snapshot the staged state for
  later replay beyond the `upper` dir.

### btrfs subvolume snapshots
- **Pros:** true **block-level COW** — no whole-file copy-up; efficient even for
  very large files (only changed blocks copied). First-class snapshot/diff/
  rollback: `btrfs subvolume snapshot`, `btrfs send -p <parent> <snap>` yields a
  precise change stream that can be `receive`d into the original (a clean
  *apply*); discard is just `btrfs subvolume delete`. The agent works in a **real
  subvolume** with full fs semantics — no union-fs quirks, and **no submount
  shadowing**: inside the snapshot, `/proc`/`/sys`/`/dev` are mounted normally
  (you `pivot_root` into the snapshot subvolume as root). Space cost is only the
  delta.
- **Cons:** **requires the workspace to be on btrfs** — if the repo lives on
  ext4/xfs, snapshots are unavailable, so it is not portable. Snapshotting needs
  the *parent* to be a **subvolume**; arbitrary directories like `/var`, `/etc`
  are usually not subvolumes, so you snapshot the **whole fs root** (when it is a
  subvolume) rather than per-tree — which actually suits "quarantine everything,"
  but means the layout must be btrfs-subvolume-structured. **Nested subvolumes are
  excluded** from a snapshot (they appear empty) — a well-known btrfs gotcha
  (Docker, `.snapshots` dirs, db subvols). `btrfs send/receive` for *apply*
  requires keeping the parent + snapshot and breaks if the original changed in the
  meantime. btrfs has its own operational cost: COW fragmentation / performance
  for heavy random writes (needs `nodatacow` tuning for DBs/build dirs), and
  balance/scrub maintenance.

### Guidance
- **Default to overlayfs** when portability matters (the repo may be on any fs)
  and the workspace is not guaranteed btrfs. It is what we shipped; the namespace
  scoping already solves "not everybody's writes."
- **Prefer btrfs snapshots** when the agent's workspace is *known to be on
  btrfs* (or you are willing to require it). They give efficient, semantically
  clean quarantine with trivial rollback and avoid both the submount-rebind dance
  and the whole-file copy-up cost. The "apply" becomes `btrfs send -p` (clean
  diff) rather than a file merge; watch the nested-subvolume exclusion and keep
  the parent subvolume stable during the session.
- **Hybrid note:** btrfs's old *btrfs storage driver* (Docker) did exactly this —
  each container = a writable snapshot of a base subvolume. The snapshot approach
  is "replace the overlay mount with a subvolume snapshot + `pivot_root` into it";
  you still want a private mount namespace + user namespace around it for scoping
  and for the `ai_*` user to be namespace-local.
- **Reflink (`cp --reflink`)** is a weaker cousin: per-file COW copies needing
  reflink support, but it does not *transparently intercept* writes the way an
  overlay mount or a snapshot does, so it is not a drop-in quarantine primitive.

**Net:** overlayfs for universality and simplicity; btrfs snapshots for
efficiency and clean rollback *when the workspace is btrfs* — with the nested-
subvolume caveat and the requirement that the relevant trees be subvolumes.

---

## 9. Implementation status: fuse-overlayfs fallback (Scenario 2)

Implemented in `src/security/overlay.rs`:

- `OverlayKind` (`Kernel` | `Fuse`) + `overlay_kind()`: picks **in-kernel overlay**
  only when we are root in the **init** user namespace (`in_init_user_ns()` +
  `can_mount()`); otherwise falls back to **fuse-overlayfs** when the binary and
  `/dev/fuse` are present (`fuse_overlayfs_available()`); else `None` (degrade to
  the in-process guardrail).
- `enter_private_mount_ns()`: root path = `unshare(CLONE_NEWNS)`; unprivileged
  path = `unshare(CLONE_NEWUSER | CLONE_NEWNS)` + self-map (`0 <uid> 1`), which
  grants virtual root + `CAP_SYS_ADMIN` *inside* the ns so fuse-overlayfs can
  mount without host root.
- `Quarantine::mount`/`resume` and `ProjectQuarantine::mount`/`resume` choose the
  mount command by kind: `mount -t overlay` vs `fuse-overlayfs -o ...`.
- `wt`'s mount gate uses `overlay_available()`; `Agent::new`'s overlay engagement
  is `#[cfg(not(test))]` (a launcher concern; unit tests must not mount overlays
  or set the process-global quarantine flags).

Validated on WSL2 (Microsoft kernel 5.15):
- Root path: in-kernel overlay mounts and stages (`ROOT_OVERLAY_OK`).
- Unprivileged path (uid 1000, userns root-map): fuse-overlayfs mounts and stages
  writes into the upper, real lower untouched (`FUSE_UNPRIV_MOUNT_OK` + `WRITE_OK`).
  Requirement: the staging upper/work dirs must be writable by the agent's real
  uid (true for `~/.pi/agent/quarantine-staging`).
- `cargo test --bin pir`: 130 passed, 0 failed.

### Runtime fixes found by actually running the agent

1. **bash was denied by default.** The `bash` tool's preflight surfaced every
   command as `Op::Custom("bash: <cmd>")`, which `decide` denies by default —
   so even `ls` was blocked and the turn aborted. Fixed: bash is surfaced as
   `Op::Exec` (allowed by default); the write-quarantine is enforced by the
   overlay layer, not the in-process preflight (which can't see syscalls).
2. **Unprivileged userns broke `drop_to_agent_user`.** When the agent runs as a
   non-root user and `enter_private_mount_ns` enters a user namespace with the
   single-line root-map (`0 <uid> 1`), the process becomes virtual root (uid 0)
   and the agent's real uid is *not* mapped — so the bash child's
   `setresuid(agent_uid)` failed and every command failed to spawn. The kernel
   only lets an unprivileged process write a ONE-line uid_map (multi-line maps
   need setuid-root `newuidmap`), so we can't map both. Fixed: `drop_to_agent_user`
   now detects the unprivileged-quarantine case
   (`in_userns_mapping_agent_to_root`: non-init user ns whose uid_map maps
   ns-0 -> the agent's uid) and treats virtual root as already being the agent,
   skipping the setuid. The agent's commands then run as virtual root (host uid =
   the agent's uid) — same host identity, no extra privileges.
