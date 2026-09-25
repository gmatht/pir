# Resource limits for agent commands (cgroups, OOM/swap guard)

Status: **implemented** — `src/resource.rs`, wired into the builtin `bash` tool.

This document is the plan, rewritten against the platform measurements it was
originally written without. The first version assumed cgroup v2 enforcement; it
was wrong about this host, and the correction changes which mechanism does the
work (see §2).

---

## 1. Goal

A command the model invents is bounded in *time* (the 10-minute detach, the
2-hour ceiling) and in *privilege* (the `ai_X` uid drop), but was not bounded in
*resources* at all. Three consequences, in increasing severity:

1. **Host OOM.** `cargo build -j$(nproc)` or a `find / -exec` exhausts the
   machine; the kernel then picks a victim by heuristic, often `pir` itself or
   another agent's build. The agent loses a command; the operator loses the
   session.
2. **Swap death.** A runaway command thrashes swap for minutes. Every `pir` on
   the box becomes unresponsive. This is a *liveness* failure with no error
   message anywhere — nothing crashes, so nothing is reported.
3. **No measurement.** Nothing records how much memory a command used, so the
   model cannot learn "that command was 30 GB" and the operator cannot see which
   of twenty jobs is the problem.

The design goal is therefore not "harder timeouts". It is: **the agent's process
tree gets a budget; the budget is enforced by the kernel where possible; `pir`
kills the tree itself, deliberately and with a message, before the kernel's OOM
killer or a swap collapse does it by accident.**

## 2. Platform reality (measured, not assumed)

The host this was built on is a **hybrid** container, and the original plan's
availability table was wrong about it. Verified:

| fact | evidence |
|---|---|
| cgroup **v2** is mounted at `/sys/fs/cgroup/unified` | `cgroup2 rw,…` in `/proc/self/mounts` |
| …but it has **no controllers**, even as real root | `cgroup.controllers` is empty; `echo +memory > cgroup.subtree_control` **fails**; a child cgroup gets no `memory.max`/`memory.stat` |
| the working controller is **v1** | `/sys/fs/cgroup/memory`, our process in `memory:/init.scope` |
| **v1 enforcement works** | a child joined the cgroup, limit 200 MiB, `bytearray(800 MiB)` never printed: `oom_kill 1`, `max_usage_in_bytes` == limit, exit status 9 (SIGKILL) |
| **PSI is absent** | `/proc/pressure/memory` does not exist |
| the v1 tmpfs mount may be `ro` | needs `mount -o remount,rw /sys/fs/cgroup`, or degrade |

**Consequence for the design:** the plan is written as *detect the hierarchy,
use whichever controller actually works, degrade honestly*. It never assumes v2
and never claims a limit is enforced when it is not. The implementation probes
the real operations (create a directory, write a limit) rather than trusting the
mount table — the v2 mount looks perfectly fine and enforces nothing.

## 3. Layout

```
/sys/fs/cgroup/memory/pir-<uid>/          the operator's tree
    <session-key>/                        one per pir session (aggregate cap)
        cmd-<pid>/                        one per spawned command
```

`<session-key>` comes from the scratchpad directory name, so a cgroup is
trivially correlatable with `$PIR_SCRATCHPAD` and the session log.

The **session slice** bounds the aggregate of foreground command + detached jobs
+ sub-agent commands, which per-command limits cannot: `pir` in full-auto can
have a foreground command plus concurrent sub-agents plus detached jobs, and
per-command budgets alone would let four commands each take their full 25% at
once.

**Lifecycle is structural, not best-effort.** `CommandCgroup`'s `Drop` removes
the directory, so the cgroup's lifetime is exactly the command's — including
across the detach-into-a-`Job` path, where the guard is moved into the `Job` so
a detached command stays accounted for. A startup sweep removes leftovers from a
crash: a leaked cgroup *holding a limit* is worse than no cgroups, because it
makes the next run unaccounted. The sweep only removes cgroups with an empty
`cgroup.procs`, so a live session is never disturbed.

> Implementation note worth keeping: a v1 cgroup directory **always** contains
> pseudo-files (`cgroup.procs`, `tasks`, `memory.*`), so "is the directory
> empty" is the wrong test and leaves every cgroup behind forever. The sweep
> asks `cgroup.procs` instead, which is also what makes `rmdir` legal.

## 4. Limits

Every number is derived from the host and then clamped — never a bare constant —
so a 4 GiB VM and a 128 GiB workstation both behave sanely with no configuration.

### The posture rule: advisory by default, armed only in extremis

**A hard memory cap is not the normal case.** A `cargo build` that legitimately
needs 30 GB must be allowed to take 30 GB — and a static number cannot tell that
apart from a leak heading for 30 GB. OOM pressure is *exceptional*; multi-GB
builds are *ordinary*. A guard that rations the ordinary case to prevent the
exceptional one is a worse bug than the one it fixes (an early cut of this
feature capped commands at 25% of RAM and would have killed that build).

So the default is **advisory**: the cgroup is created and *measured*, a kernel
ceiling is **not** written, and commands are stopped only by the evidence-based
poll when the host is genuinely in danger. Enforcement is armed only when:

* `MemAvailable` is already below `max(10% of total, 1 GiB)` — the machine is
  under pressure *before* the command starts, so launching an unbounded one is
  the wrong move; or
* swap is more than half used — the box is already leaning on swap and another
  large consumer would tip it into thrash; or
* the operator asked (`PIR_RESOURCE_MEMORY_MIB=…` or `PIR_RESOURCE_ENFORCE=1`).

When armed, the ceiling is still the *loose* one below — a backstop, not a small
number. `/limits` states which posture is live ("advisory — … the ceiling is not
a kill line" vs. "ARMED — the host is under pressure").

| limit | value | why |
|---|---|---|
| per-command ceiling | `max(60% of effective total, 4 GiB)`, never above the total; **never** clamped by current `MemAvailable` | deliberately loose: on a 64 GiB box this is ~38 GiB, so a 30 GB build fits comfortably. Low `MemAvailable` must not shrink it — a small build should not be rationed because a big job runs alongside it; that case is for the host guard to decide with evidence |
| session `memory.limit_in_bytes` | `max(90% of total, ceiling + 1 GiB)`, never above the total | must stay **above** the per-command ceiling, or a single legitimate command would be killed by the aggregate cap before reaching its own (and the message would blame the session) |
| per-command `memory.memsw.limit_in_bytes` | `== memory limit`, **only when armed** | the v1 equivalent of v2's `memory.swap.max = 0`: overshoot is an immediate OOM-kill, not a reclaim-then-thrash period |
| per-command `memory.soft_limit_in_bytes` | the ceiling, **always** | set even in advisory mode: it applies reclaim *preference* under pressure and can never kill a process, so a 30 GB build proceeds while the kernel still prefers to reclaim our page cache over someone else's working set |
| session `memory.memsw.limit_in_bytes` | `limit + 256 MiB` | one cold page may be pushed out; sustained thrash is what we prevent |
| `pids.max` | `clamp(1024, 4096)`, scaled by host size | the highest-value limit after memory: a `while true; do cmd & done` cascade exhausts the host PID table, at which point *nothing* can fork |
| CPU | **not limited** | a `cargo test` on a 96-core box *should* use 96 cores; a naive core cap silently makes every build 20× slower, the regression that gets a safety feature ripped out |

**Effective total** is `min(/proc/meminfo MemTotal, our own cgroup limit)`. A
container-limited `pir` that read the host's RAM would compute budgets an order
of magnitude too large — and vice versa, a `pir` that ignored its container's
limit would think it had memory it cannot actually use.

The `MemAvailable` **clamp was removed deliberately.** Rationing a command by
current free memory means a small build gets a small budget just because a big
job is running alongside it — precisely the case where a static number cannot
know better than the runtime guard. Admission control now happens through the
*host guard* (§6), which decides with evidence and can refuse to kill.

## 5. Enforcement: three layers

**L1 — kernel (armed only in extremis, see the posture rule above).**
`limit_in_bytes` + `memsw.limit_in_bytes` + `pids.max`, written at cgroup
creation **only when enforcement is armed**; otherwise just the soft limit, which
cannot kill. v1 has no `memory.oom.group`, so a group kill is *our* job, not the
kernel's — a real difference from the v2 design.

**L2 — `pir`'s own poll.** Runs on the **existing** 250 ms wait tick in
`run_shell`, the same loop that already handles ESC, `job_kill`, the deadline and
detach. No new thread, no new failure mode.

**In the default (advisory) posture the over-budget and growth rules are
disabled outright**: exceeding the ceiling is not by itself a reason to die. The
poll's job in that posture is *measurement* — feeding the peak to the result and
giving the host guard the scale it needs to reason about. Only the host-pressure
check can kill, and only when the machine is actually in danger. Signals
sampled, all single small file reads:

| signal | source |
|---|---|
| usage / peak | `memory.usage_in_bytes`, `memory.max_usage_in_bytes` |
| anonymous vs cache | `memory.stat` (`rss`), `memory.stat` (`cache`) |
| swap | `memory.memsw.usage_in_bytes − memory.usage_in_bytes` |
| limit hit | `memory.failcnt` |
| kernel killed | `memory.oom_control` → `oom_kill` |
| process count | `cgroup.procs` |
| host | `/proc/meminfo`, `/proc/vmstat` |

**L3 — host guard**, and the blame question (§6).

### Kill heuristics

* **Over budget:** `usage ≥ 0.95 × limit` for **2 consecutive samples** (0.95
  because the interesting moment is just *before* the kernel acts; 2 samples
  because a single spike is usually a transient — the linker mapping a 4 GB
  binary for 100 ms — and a kill is irreversible). **Advisory posture: disabled**
  — see the posture rule.
* **Fast growth:** `Δusage > 200 MiB/s` over a 4 s window while already past 60%
  of the limit. A *fast* grower reaches the limit anyway, so killing early costs
  only latency; a *slow* grower (the 1 MiB/min leak) is left alone to finish.
* **PID ceiling:** at the limit and unable to make progress anyway.
* **Host critical:** `MemAvailable < max(5% of MemTotal, 512 MiB)`.
* **Swap thrash:** swap >50% used **and** our own swap growing across samples.

**The page-cache trap** is the subtle one, and getting it wrong produces the
worst possible symptom — false kills of `cargo build`, which legitimately
charges hundreds of MB to a GB of file cache. `usage_in_bytes` counts page
cache, so the over-budget rule additionally requires that **anonymous memory
dominates** (`rss × 2 ≥ usage`) **or** that `failcnt > 0` (the kernel already
tried to reclaim and could not — the strongest available signal). Without this,
the feature kills correct work and reads as flakiness.

**Do-not-kill rules:** never a single sample; never on a cache-dominated cgroup
with `failcnt == 0`; never on `MemFree` alone; never during the TERM grace.

## 6. The blame heuristic

The question is not "is our task big?" but **"does the host's free memory fall
at the rate our task grows?"** — a differential test, not a threshold.

| evidence | verdict |
|---|---|
| `memory.failcnt > 0` / `oom_kill > 0` on **our** cgroup | definitively us — nothing else can have caused it |
| our `Δusage ≈ ΔMemAvailable`, same direction | we are the consumer → stop |
| our usage flat/small while `MemAvailable` falls | **something else is growing** → do not kill us |
| `pgmajfault` rising system-wide with our `rss` flat | global pressure, not ours |

The asymmetry is deliberate and stated: a **wrong kill destroys a correct
`cargo build`** and reads as flakiness; a deferred kill costs one more 250 ms
tick. So we kill only when implicated, and when *not* implicated we shrink the
next command's budget and say so. The single exception is the hard floor
(`MemAvailable < max(5%, 512 MiB)`), which kills regardless of blame — at that
point the box is about to OOM *something* and doing nothing helps nobody. The
message then says it was host protection and **not** an accusation.

Caveats, not buried: a fast-growing page cache (a big `tar`/`cp`) moves `MemFree`
without being an OOM threat, so the comparison also weighs `MemFree`; and with
**no PSI on this host** we lose the "stalled in reclaim" signal entirely, so the
fallback attribution is inference and is labelled as such in the message.

## 7. Legibility (part of the design, not decoration)

A kill the model cannot interpret is nearly as bad as a hang: it retries the
same command. So the tool result carries the budget, its basis, the peak, and
whether the host was in danger:

```
[pir] KILLED by memory limit — this command used 192 MiB (its budget for this
      host: 192 MiB, enforced by cgroup v1).
      peak memory 192 MiB of 192 MiB budget (100%).
      The host itself is fine. This was NOT a machine-wide OOM.
      budget basis: 192 MiB (operator override).
      hint: `cargo build -j2` / `CARGO_BUILD_JOBS=2` often fixes a -j$(nproc) OOM
```

Design points, each deliberate: it distinguishes a memory kill from a timeout and
from an ESC; it names the budget *and where the number came from* so the model can
judge whether a smaller retry will fit; it says whether the host was in danger
(retry vs. stop differ); and it carries a hint for the common causes
(`-j$(nproc)`, `xargs -P`, node leaks, container builds) — a cheap pattern
match with a high hit rate, the same trick the shell env wrapper already plays
for `timeout` and `grep`.

Two more visibility rules: `memory.peak` is reported on the **success** path too
when it exceeds 50% of budget (a command that finished near its ceiling is the
interesting one, and the operator can raise the budget *before* the next
failure); and host pressure that did *not* justify a kill is reported as
`NOT killed for memory — … another consumer is growing`.

> A killed command has no exit code (`try_wait` reports a signal death), which
> used to surface as `[exit code -1]` — technically true and useless. The
> resource message replaces it.

## 8. Fallback when there is no controller

`Backend::None` is a real, expected state (the v2-only container, a locked-down
CI runner). Then: no kernel limit, but the poll and host guard still run, `/limits`
says **"detection only"** in yellow, and a tool result never claims enforcement
it does not have.

`RLIMIT_AS` is deliberately **not** used as a substitute: it bounds address
space, not resident memory, so a process can reserve 8 GiB and touch 1 byte. It
false-kills any JVM or `mmap`-heavy build and does nothing about swap. `RLIMIT_NPROC`
is also not set: it is **per-uid**, and the agent's commands share the `ai_X`
uid, so it would apply to the union across all sessions and break the `fork` the
*next* command needs.

## 9. Configuration

Env knobs (consistent with the rest of the tool):

| variable | effect |
|---|---|
| `PIR_RESOURCE_MEMORY_MIB` | per-command ceiling in MiB; overrides the derived value **and arms enforcement** (an explicit number is a request to enforce it) |
| `PIR_RESOURCE_ENFORCE=1` | arm kernel enforcement even on a healthy host |
| `PIR_RESOURCE_PIDS` | process ceiling; overrides the derived value |
| `PIR_CGROUP=0` | disable cgroups entirely (detection only) |

Command: **`/limits`** (alias `/resources`) prints the detected hierarchy, the
per-command and session budgets with their basis, the enforcement mode and live
host state — including the honest "no controller here" case.

## 10. Verification

Unit tests (no privileges) cover the budget arithmetic across host sizes, the
`MemAvailable` clamp, the operator override, the session/command relationship,
the PID scaling, the message contents (budget named, basis named, blame
distinguished, hints firing) and `human_bytes`.

Live tests (`PIR_RESOURCE_LIVE_TESTS=1`) exercise the real cgroup tree:

* `report_this_host` prints the detected backend and the computed budgets, so a
  change in the *environment* (a host that gains v2 controllers, a container
  that clamps our memory) shows up in the log rather than silently altering
  behaviour;
* `enforcement_kills_an_over_allocator` — a 256 MiB limit against a 900 MiB
  allocation, asserting the process never completed and that the kernel's
  `oom_kill` counter advanced;
* the end-to-end variant asserts the operator override reaches the budget and
  that the kill message names it.

Two live tests pin the **posture**, which is the property that matters most:

* `a_large_allocation_is_allowed_under_the_default_posture` — asserts that a
  healthy host does **not** arm enforcement, that the ceiling is not a kill
  line, and that a **6 GiB allocation completes** (`ALLOCATED-6G`, exit 0, peak
  6.0 GiB, zero OOM kills) with no limit written.
* `enforcement_kills_an_over_allocator` — the same mechanism with enforcement
  explicitly armed: a 256 MiB limit against a 900 MiB allocation, exit status 9
  (SIGKILL), the kernel's `oom_kill` counter at 1, peak exactly the limit.

Plus unit tests for the ceiling itself: `a_thirty_gb_build_is_allowed_on_a_64_gb_host`
(a 30 GB build must fit), `ceiling_is_loose_not_a_ration`,
`low_available_memory_does_not_shrink_the_ceiling`, and
`enforcement_is_off_on_a_healthy_host` / `enforcement_arms_only_under_pressure`.

Verified by hand, end to end: a real `pir` session with
`PIR_RESOURCE_MEMORY_MIB=192` running `python3 -c 'x=bytearray(600*1024*1024)'`
came back with the command killed at exactly its budget, `peak memory 192 MiB of
192 MiB budget (100%)`, and the model correctly reporting a resource kill rather
than a crash.

## 11. Known gaps

1. **`pir`'s own allocations are unconstrained.** In-process `read_file` (a 4 MB
   `.gz`, an unzip, a 2000-match `grep` buffer), checkpoints, the context window
   and sub-agent transcripts all live in `pir`'s address space. No per-command
   cgroup helps; a full fix means moving the heavy readers into child processes.
2. **Sub-agents are threads, not processes.** Only their *commands* are bounded.
   The 8-task concurrency cap remains the only real bound on fan-out.
3. **`pids.max` is v2-only.** v1 keeps PIDs in a separate hierarchy, so on a v1
   host a missing `pids.max` is expected and is not a failure of the memory path
   — the PID ceiling degrades to "not enforced" rather than breaking.
4. **Swap death cannot be *prevented* without swap accounting.** Where `memsw`
   cannot be lowered, the best available is detect-then-kill, which by
   construction acts after some thrashing.
5. **A cgroup with a limit inherited wrongly is worse than none.** If `pir`'s own
   process ended up inside a command's cgroup, a runaway command's OOM would kill
   `pir`. Joining happens **only** in the child's `pre_exec`; the parent never
   writes its own pid.
6. **Group kill is ours.** v1 has no `memory.oom.group`, so `kill(−pgid)` plus
   the escalation ladder is the whole mechanism — and a `setsid` escapee is the
   documented hole that a cgroup is supposed to close. Worth a follow-up test.
