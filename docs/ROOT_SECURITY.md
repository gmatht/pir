# ROOT_SECURITY.md — root-launched write-quarantine (current + full-root)

The privileged (root / `sudo`) posture, its current implementation, and what the
WSL2 kernel is missing that blocks the eventual full-root container.

---

## 1. Current root posture (shipped, default)

- **Identity**: `pir` runs as the invoking root user; the `bash` tool's child
  drops to the per-project `ai_*` user (`drop_to_agent_user`). The saved-root on
  the parent enables `/su-security off <reason>` to grant the agent the invoking
  user's authority for the session (escalation is human-gated, reason-logged,
  reversible, persisted per session).
- **Selective write-quarantine** (overlayfs, in a private mount namespace):
  - **Project overlay**: the repo root is overlaid (`lowerdir = repo`,
    `upperdir = staging`); the agent's **worktree is bind-mounted real on top** —
    the only whitelisted tree; the trunk, `.git`, and other worktrees stage.
  - **System overlay**: `/etc /usr/local /opt /srv /var /boot` stage
    (configurable via `quarantine-dirs`; `quarantine-critical` hard-denies at
    apply time).
  - Staging lives in `~<agent>/.pi/agent/quarantine-staging` (owned/chowned so
    the `ai_*` agent can write the upper). `/quarantine status|apply|discard`
    reviews; **apply** suspends the overlay within the namespace, merges
    upper -> real, and re-engages (so it lands on the real fs).
- **Guardrail fallback** (when no overlay can mount): in-process `decide` hard-
  denies DBs (`*.db`, `*.sqlite*`, `*.duckdb`), secrets (`~/.ssh`, `~/.aws`,
  `~/.gnupg`, `*.key`/`*.pem`, `~/.config/gh`…), system state (`/boot`,
  `/etc`, …), test oracles, and repo `.git`; everything else is real.
- **Worktree idle loop**: after a change, fix build errors -> tests -> merge to
  main -> heal main (errors -> tests -> warnings); warning fixing only
  post-merge (`PIR_WT_AUTOFIX=all|no-warnings|errors|off`).

**Known gap (documented)**: the overlay is selective. A root agent can still
write anything *outside* the repo + system trees (e.g. `/tmp`, `/home/…`, the
repo's parent) for real. The full-root option below closes that on capable
kernels.

## 2. Full-root option (`PIR_QUARANTINE_MODE=fullroot`, default)

Attempts the rootless-container recipe: overlay **`/`** (`lowerdir=/`, upper/
work on tmpfs outside `/`), re-host pseudo-fs, bind the worktree real, promote
over `/`. On kernels that can't, it prints a banner and **falls back** to §1.
"ai-root inside, cannot escape" = user+mount+PID namespaces with a self-mapped
uid/gid: the agent is root only inside its own ns (unprivileged on the host),
cannot `setns` back to the host user namespace.

### What to hide inside the container (§3.1 of NONROOT_SECURITY.md)
Minimal `/dev` (never the host's raw block devices), host mounted filesystems
hidden (overlay doesn't traverse submounts), unmount the old root after
`pivot_root`, keep the staging out of the container view, read-isolation over
`/root`,`/boot`,secrets (TODO), minimal read-only `/sys`.
Can't be done by "read-only-ing" a device mount (`/mnt/sda1` etc.) — hiding, not
read-only.

---

## 3. What the WSL2 Microsoft kernel is missing (5.15.167.4)

Validated empirically. The WSL2 kernel provides: unprivileged user namespaces
(`max_user_namespaces=192685`, `USERNS_OK`), pid/mount nets, tmpfs, fresh
`proc` (with an owned pid ns), `pivot_root`, and fuse-overlayfs in a userns
(`FUSE_NS_OK`) — but it **lacks/refuses** exactly what the full-root container
needs:

1. **overlayfs inside a user namespace** — `mount -t overlay` in a userns →
   `wrong fs type / bad superblock` (Microsoft WSL2 kernel doesn't allow
   in-userns overlay, unlike native 5.11+ with idmapped mounts).
2. **sysfs in a userns** — `mount -t sysfs` -> `permission denied`. (Sysfs was
   never mountable in a child userns; native containers must *bind* it — which
   works into a real-dir rootfs but not into fuse-overlayfs.)
3. **bind of init-owned system mounts in a userns** — `mount --bind /usr`
   (and `/lib`, `/sys`, `/dev`) into the container -> `wrong fs type`, even with
   `-o idmap=` (idmapped mounts not honored for these on WSL2; native 5.12+
   rootless uses them).
4. **mount-on-top inside fuse-overlayfs** — binds INTO a fuse-overlayfs path
   fail (`wrong fs type`); only some fresh-mounts work (tmpfs, proc-with-pidns),
   sysfs/devtmpfs refuse. So a fuse-over-root container can't re-host the
   pseudo-fs.

**Consequence:** on WSL2, "ai-root inside + cannot-escape + host toolchain
mounted" is impossible for both root and non-root. The directory-rootfs model
(lxc-style, own rootfs + binds + pivot) avoids 1, but still needs working
cross-userns binds / idmap (3) to mount the host toolchain. On a **native
kernel**, the full-root container (§2) is the target.

## 4. Root vs non-root (see docs/NONROOT_SECURITY.md)
Root = selective overlays (current) or full-root container (native Linux).
Non-root = fuse-overlay `$HOME` + user-writable mounts + repo (`auto-writable`,
see NONROOT_SECURITY.md §4).
