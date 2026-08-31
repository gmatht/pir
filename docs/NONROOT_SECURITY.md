# NONROOT_SECURITY.md — write-quarantine for unprivileged users

Working notes / design doc for the non-privileged (no `sudo`, no `ai_*` drop)
agent posture. All claims below were validated empirically on the **WSL2
Microsoft kernel 5.15.167.4** (WSL2 Ubuntu 24.04) unless noted.

---

## 0. The doctrine

> **Permit all operations; quarantine non-whitelisted writes.** The agent may
> run anything and write anything it "wants" to — but every write outside the
> whitelist is intercepted and **staged** (visible only to the agent) so the
> human reviews and applies/denies it. Nothing reaches the real filesystem
> without an explicit human action. No operation should be a hard stop; a
> blocked write should be a *"here's a staged change, review it"* notification.

The root-launched posture (`ai_*` UID sandbox + selective overlays over
`/etc /usr/local /opt /srv /var /boot` + the repo-worktree overlay) does NOT
transfer to unprivileged users, and even for root it hard-stops some writes.
This doc is the non-root design.

---

## 1. Why the root posture doesn't transfer

- **`ai_*` UID sandbox is root/sudo-gated.** Dropping to a per-project
  `ai_*` user needs `setuid`, which needs euid 0 or `sudo -u ai_* pir`. An
  unprivileged launcher can't do it: the code falls back to running the agent
  **as the invoking user** and prints
  `pir: not privileged — agent commands will run as the invoking user (re-run as
  root, or sudo -u ai_* pir …)`.
- **Selective overlays need root.** Overlaying the system trees in-kernel
  requires `CAP_SYS_ADMIN` in the init mount namespace; unprivileged, only
  **fuse-overlayfs inside a user namespace** works.
- **In-kernel overlay copy-up stops unprivileged writes to root-owned files.**
  Even *through* the overlay, a non-root writer can't modify a root-owned lower
  file: overlayfs copy-up preserves root ownership of the upper copy, so the
  write EACCESes (observed with `.git` writes and `/etc/hosts`-type targets).
  This is precisely the "permit but quarantine" violation: the agent is a hard
  stop, not a staged-change notification.
- **DAC is a hard stop, not a quarantine.** Files the agent's UID can't write
  fail immediately (EACCES) — the operator never gets the chance to review "the
  agent wants to change X."

**Consequence:** for non-root, protection must come from something the
unprivileged user *can* mount (fuse-overlayfs in a userns) over paths they can
actually write — and DAC quietly handles the rest.

---

## 2. Primitives available to an unprivileged user (validated on WSL2)

| Primitive | Works unprivileged? | Notes |
|---|---|---|
| `unshare(CLONE_NEWUSER)` | ✅ `USERNS_OK`, `max_user_namespaces=192685` | self-map `0 <uid> 1` → **ai-root** inside, unprivileged on the host → cannot `setns` back to the host user namespace (real no-escape) |
| `CLONE_NEWNS`, `CLONE_NEWPID` | ✅ | pid ns "owned" by the userns is required for `/proc` |
| `tmpfs` mounts | ✅ | upper/work staging |
| fresh `mount -t proc` | ✅ (with owned pid ns) | rootless containers rely on this |
| fresh `mount -t tmpfs /run /tmp` | ✅ | |
| `pivot_root` | ✅ `PIVOT_ROOT_OK` | a non-root user can confine its own root |
| bind of the user's OWN files | ✅ `PLAIN_BIND_OK`, `IDMAP_BIND_OK` | own-home/configs |
| **fuse-overlayfs** over the user's own dirs / their mounts | ✅ | **the key primitive** — mounts in a userns, and the daemon does copy-up as the user, so even **root-owned lower files stage** (write staged, real untouched) |
| in-kernel `mount -t overlay` inside a userns | ❌ `OVERLAY_NS_FAIL` | WSL2 refuses it |
| fresh `mount -t sysfs` / `devtmpfs` in a userns | ❌ | sysfs/devtmpfs aren't mountable in a child userns |
| bind of HOST system dirs (`/usr`, `/lib`, `/sys`, `/dev`) into the container | ❌ `wrong fs type` (even `-o idmap=` refused) | cross-userns bind of init-owned system mounts |

**Validated end-to-end (non-root, uid 1000/983, userns+mountns):**
fuse-overlayfs over a **mounted lower** (tmpfs), writing a **root-owned** file →
stage in the upper, **real mount untouched** (`real important.txt = realdata`,
`upper important.txt = changed`). This is the mechanism that protects
user-writable `/mnt/*` mounts (i.e. the Windows host through `/mnt/c`) when the
user can actually reach them.

---

## 3. Why the "full-root no-escape container" is native-Linux-only

The ideal end state — ai-root inside a container whose whole `/` is an overlay
(`lowerdir=/`), with `/proc /sys /dev /run` re-hosted inside and the host
toolchain mounted — requires the two things the Microsoft WSL2 kernel blocks:

1. **in-kernel overlayfs inside a user namespace** (refused on WSL2), and
2. **re-hosting `/sys`/`/dev` and binding the host `/usr`/`/lib`** into the
   container from a userns (refused; `idmap` not honored for them on WSL2).

So on WSL2: **ai-root-inside + cannot-escape + host-toolchain-mounted are
mutually exclusive** for root and non-root alike. On a **native Linux kernel**
(≥5.11, idmapped mounts + overlay-in-userns — what rootless Podman uses), the
full-root container is achievable. `lxc` does not change this: it orchestrates
the same syscalls, and its *directory-rootfs* model (own rootfs + binds +
`pivot_root`) avoids overlay-as-root — but it can't un-block the WSL2 kernel.

**Container weight** (measured): ~10 ms per container (namespaces + mount),
~2 MB RSS per container init, ~60 KB bind-only rootfs skeleton (host dirs are
bind-mounted, zero copy); limits `pid_max=4194304`, `threads-max=385371`,
`max_user_namespaces=192685`. Practical concurrency is bounded by agent memory
/ the model API rate limit (e.g. HTTP 429), not the container machinery.

### 3.1 What to hide inside the full-root container

The container's `/` is the host's read-mostly *lower*; the rule is: **curate what
you MOUNT IN — don't expose the host's devices or its mounts.** You cannot make
the real `/dev` or the mounted devices "read-only" from inside; the correct
control is hiding them.

- **Minimal `/dev`, not the host's.** Bind-mounting the host `/dev` gifts raw
  block devices (`/dev/sda`, `/dev/mmcblk0`, NVMe, Windows disks via drvfs) to
  ai-root. Instead: a tiny tmpfs `/dev` with only `null, zero, full, random,
  urandom, tty, ptmx` + `devpts`, `shm`, `fuse` (mknod for exactly those).
- **Host mounted filesystems are hidden automatically.** overlayfs does not
  traverse submounts, so anything mounted in the root user's namespace
  (`/mnt/c`, `/mnt/d`, USB, external disks) appears as an empty mountpoint dir in
  the container — do NOT bind them back in. Expose only what the user
  explicitly wants the agent to touch, and (per this doc) only through the
  quarantine.
- **Unmount the old root after `pivot_root`** (runc does this) so the host's
  full mount tree isn't reachable via the detached old-root path.
- **Hide the quarantine staging** (upper/work) from the container's final view:
  the overlay holds it by inode, so keep it unmounted-at-a-visible-path so the
  agent can't locate/alter its own quarantine record.
- **Optional read-isolation**: bind empty/own dirs over `/root`, `/boot`, other
  users' homes, and secret stores (the lower is readable through the overlay;
  the doctrine permits reads, so this is a tightening).
- **`/sys`** minimal and read-only (`-o ro`) to keep device enumeration
  (`/sys/block/…`) non-writable; a fresh userns proc already denies
  `/proc/kmsg`/`/proc/kcore`; minimal `/dev` covers `/dev/kmsg`.
- Network stays on (host netns) per the task — a separate namespacing decision.

---

## 4. Recommended non-root posture (the intended implementation)

> **Quarantine every surface this user can actually write that matters;
> let DAC handle the rest; skip /tmp.**

Surfaces to stage (auto-detected by probing access, `access(W_OK)`):

- **`$HOME`** — the user's data (staged for review; whitelisted real:
  the active worktree, `~/.cargo`, `~/.pi/agent/sessions`, `GH_CONFIG_DIR`).
- **User-writable mounts** under `/mnt/*`, `/media/*`, `/run/media/*`, etc.
  (`access(W_OK)` true) — protects the host through e.g. `/mnt/c` **when the
  user can reach it**. Where DAC already denies access (e.g. this box's `/mnt`
  is `drwxrwx--- root:user`, group-gated, so `ai_pir` can't even traverse it),
  no overlay is needed — the path is already untouchable.
- **The repo root** (if outside `$HOME`) — worktree whitelisted.

Mechanics per surface: `userns+mountns` (ai-root inside, no-escape from the
host user ns) → `fuse-overlayfs` (lower = the real dir/mount, upper/work on the
user's own staging: tmpfs or `$HOME/.pi/agent/quarantine-staging`) → whitelist
binds real on top → all staged writes surface together under `/quarantine`
(review/apply/discard; apply = merge upper → real, discard = drop).

`/tmp` is deliberately **not** quarantined (default): temp-space breakage is the
fault of code that stores important data in `/tmp` without `mktemp`. Make it
configurable for those who disagree.

---

## 5. Protection matrix

| | Identity | What's staged (quarantined) | What real writes are possible |
|---|---|---|---|
| **root** (current) | invoking user + optional `ai_*` drop for bash | repo (except worktree), system trees `/etc /usr/local /opt /srv /var /boot`, `/root` via DAC | anything else the root identity can write — the known selective gap |
| **non-root** (proposed `auto-writable`) | the invoking user (no `ai_*`) | `$HOME`, user-writable mounts (`/mnt/*` etc.), the repo | `/tmp` + anything outside the listed surfaces (DAC-denied elsewhere) |
| **non-root + no fuse-overlayfs** | the invoking user | none | in-process guardrail only (hard-denies DBs, secrets, `.git`, `/boot`/`/etc`, test oracles); everything else real |
| **full-root container** (native Linux only) | ai-root in userns+pidns+mountns | **everything** except whitelisted worktree + essential dirs | nothing (no write reaches the real fs) |

Key properties:
- **No-escape for non-root is real.** A non-root userns agent is an unprivileged
  host uid in a child user ns — `setns` back to the host user ns is blocked,
  and it has no caps outside its ns.
- **The `/etc/hosts` review-notify flow** (agent "writes" it, you get a staged
  change to review) applies to surfaces the user can write. For root-owned
  system files an unprivileged agent can't reach anyway (and the user couldn't
  either), DAC's hard stop is *consistent* — there's nothing to stage.
- **`/mnt/c` (the Windows host)**: protected in both cases — by overlay when
  user-writable/reachable, by DAC (`/mnt` group-gating) when not. Either way the
  agent has no write path to the host filesystem.

---

## 6. Configuration (proposed)

```
PIR_QUARANTINE_MODE = fullroot | selective | auto-writable | off
  - fullroot       : whole-/ overlay container (native Linux kernels; degrades on WSL2)
  - selective      : current root posture (repo + worktree + system trees)  [root default]
  - auto-writable  : non-root default — $HOME + user-writable mounts + repo, /tmp excluded
PIR_QUARANTINE_SKIP_TMP = 1   (default) — leave /tmp real
PIR_QUARANTINE_DIRS  = ...    (extra surfaces to overlay)
```

---

## 7. Status

- ✅ Validated unprivileged mechanics: userns/ai-root, pidns, tmpfs, proc,
  pivot_root, fuse-overlayfs staging (incl. root-owned lowers), bind of the
  user's own files.
- ✅ Validated the WSL2 blockers end-to-end (overlay-in-userns refused,
  fuse can't host sys/dev, system-dir binds refused) → full-root container is
  native-Linux-only.
- ⏳ Not yet implemented as default: `PIR_QUARANTINE_MODE=auto-writable` (fuse
  overlay of `$HOME` + auto-detected user-writable mounts + repo, whitelist the
  worktree, `/tmp` excluded).
