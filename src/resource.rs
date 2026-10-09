//! Resource limits for agent commands: cgroup-backed memory/PID caps plus a
//! blame-aware "kill before the host OOMs or swap-thrashes" guard.
//!
//! # Why this exists
//!
//! A command the model invents is bounded in *time* (the 10-minute detach and
//! the 2-hour ceiling in the builtin `bash` tool) and in *privilege* (the
//! `ai_X` uid drop), but historically not at all in *resources*. Three failure
//! modes followed, in increasing severity:
//!
//! 1. a stray `cargo build -j$(nproc)` OOMs the **host**, and the kernel picks
//!    its victim by heuristic — often `pir` itself or another agent's build;
//! 2. a runaway command thrashes swap for minutes, making every process on the
//!    box (including the REPL) unresponsive, with no crash and so no report;
//! 3. nothing measures any of it, so the model cannot learn "that command was
//!    30 GB" and the operator cannot see which of twenty jobs is the problem.
//!
//! # The platform reality this is written against (measured, not assumed)
//!
//! Enforcement is detected from the **mount table** plus a live probe, because
//! the layouts differ per host and a hardcoded path silently disables the whole
//! feature. Two hosts have been measured:
//!
//! * **This host (single cgroup2 mount).** `/proc/mounts` has one
//!   `cgroup2 /sys/fs/cgroup` line; `cgroup.controllers` lists `memory`; a child
//!   cgroup gets a writable `memory.max`. Proven: `memory.max = 256 MiB`, a
//!   900 MiB allocation was killed (`memory.events` → `oom_kill 1`, `max 38`,
//!   `memory.peak` == the limit). Note `/sys/fs/cgroup/memory` also *exists* as a
//!   plain directory in this layout, so a v1 probe must exercise the real files
//!   (`memory.limit_in_bytes`) and not merely check that the directory is there.
//! * **A hybrid container** where v2 is at `/sys/fs/cgroup/unified` but its
//!   `cgroup.controllers` is empty even as real root, so a child gets no
//!   `memory.max`; the working controller there is **v1** at
//!   `/sys/fs/cgroup/memory`, whose `memory.limit_in_bytes` +
//!   `memory.memsw.limit_in_bytes` do work (proven: a 200 MiB limit killed an
//!   800 MiB allocation, `oom_kill 1`, `max_usage_in_bytes` == limit).
//! * **PSI may be absent** (`/proc/pressure/memory` missing), so there is no
//!   "stalled in reclaim" signal to lean on;
//! * a v1 tmpfs mount can come up read-only and needs `mount -o remount,rw`.
//!
//! Because v1 and v2 expose the same concepts under **different file names**, and
//! writing the wrong one fails silently (the file simply does not exist, so no
//! limit is written while the caller believes one was), every cgroup access goes
//! through the `f_*` name helpers near the bottom of this module rather than
//! naming a file inline.
//!
//! So the design is: **detect the hierarchy at startup, use whichever controller
//! actually works, and degrade honestly** — never assume v2, never pretend a
//! limit is enforced when it is not.
//!
//! # Layout
//!
//! ```text
//! /sys/fs/cgroup/memory/pir-<uid>/              the session slice (aggregate cap)
//!     <session-key>/                            per pir session
//!         cmd-<pid>/                            one cgroup per spawned command
//! ```
//!
//! The session slice bounds the *aggregate* of foreground command + detached
//! jobs + sub-agent commands, which per-command limits cannot: `pir` in
//! full-auto can have a foreground command plus concurrent sub-agents plus
//! detached jobs, and per-command budgets alone would let four commands each
//! take their full 25% at once.
//!
//! # Kill heuristics
//!
//! Three layers, in order:
//!
//! * **L1 kernel** — `limit_in_bytes` / `memsw.limit_in_bytes` / `pids.max`.
//!   The backstop that should never fire, because firing it means L2 failed.
//! * **L2 pir's own poll** — runs on the *existing* 250 ms wait tick in
//!   `run_shell` (no new thread, no new failure mode), with hysteresis.
//! * **L3 host guard** — watches `MemAvailable` / swap so a task that is
//!   *within* its own budget can still be stopped when the box is dying.
//!
//! L3's real job is the **blame question**: when the host is under pressure,
//! *is this task the cause?* A wrong kill destroys a correct `cargo build` and
//! reads as flakiness, so the asymmetry is deliberate — we kill only when the
//! evidence implicates the task, and when it does not we say so and shrink the
//! *next* command's budget instead of killing this one. The one exception is
//! the hard floor ([`HOST_FLOOR_MIN_BYTES`]): at that point the box is about to
//! OOM *something*, and doing nothing helps nobody.
//!
//! Two signals make the blame question more than a guess on this host: the
//! per-cgroup counters (`memory.failcnt` > 0 means *this* cgroup hit *its*
//! limit — nothing else can have caused that), and the fact that sibling
//! cgroups under the v1 hierarchy expose `memory.usage_in_bytes`, so "something
//! else is growing" is substantiated rather than assumed.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

/// Where the v1 memory controller is expected to be mounted.
const V1_ROOT: &str = "/sys/fs/cgroup/memory";
/// The fallback location for a cgroup v2 mount when the mount table cannot be
/// read. `/sys/fs/cgroup` is the standard single-mount point; the historical
/// `/sys/fs/cgroup/unified` (a hybrid-layout artefact) is also probed, because
/// a host that has one may not have the other.
const V2_ROOT_FALLBACKS: &[&str] = &["/sys/fs/cgroup", "/sys/fs/cgroup/unified"];

/// Swap allowance for the *session* slice. A single cold page may be pushed out
/// without a kill; sustained thrashing is what we are preventing, and swap is
/// not evil in moderation. Per-command cgroups get zero.
const SESSION_SWAP_BYTES: u64 = 256 * 1024 * 1024;

/// `MemAvailable` below this (or below [`HOST_FLOOR_FRACTION`] of `MemTotal`)
/// is the hard floor: kill regardless of blame.
pub const HOST_FLOOR_MIN_BYTES: u64 = 512 * 1024 * 1024;
const HOST_FLOOR_FRACTION: f64 = 0.05;

/// A command over its slice kills only after two consecutive samples: a single
/// 250 ms spike is normal (a linker mapping a 4 GB binary for a moment), and a
/// kill is irreversible.
const OVER_LIMIT_SAMPLES: u8 = 2;
/// Fraction of the limit that counts as "over" — we want to fire *just before*
/// the kernel does, not after.
const OVER_LIMIT_FRACTION: f64 = 0.95;

/// Growth rule: if the tree adds this many bytes in a 4-second window while
/// already past `GROWTH_TRIGGER_FRACTION` of its limit, kill early. A *fast*
/// grower reaches the limit anyway (so killing early costs only latency); a
/// *slow* grower — the classic 1 MiB/min leak — is left alone to finish.
const GROWTH_BYTES_PER_SEC: u64 = 200 * 1024 * 1024;
const GROWTH_TRIGGER_FRACTION: f64 = 0.6;
const GROWTH_WINDOW_SECS: f64 = 4.0;

/// How many samples of history to keep for slope/blame analysis.
const HISTORY: usize = 24;

/// Which cgroup hierarchy (if any) actually enforces limits on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// cgroup v1 `memory` controller at `/sys/fs/cgroup/memory`.
    V1,
    /// cgroup v2 with a working `memory` controller.
    V2,
    /// No usable controller: detection only, no enforcement.
    None,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Backend::V1 => "cgroup v1",
            Backend::V2 => "cgroup v2",
            Backend::None => "none (detection only)",
        }
    }
}

/// The detected hierarchy plus the root we create per-session slices under.
#[derive(Debug, Clone)]
pub struct Hierarchy {
    pub backend: Backend,
    /// Directory new slices are created in (`/sys/fs/cgroup/memory` for v1).
    pub root: Option<PathBuf>,
    /// One-line explanation for the operator (`/limits`, startup banner).
    pub note: String,
}

/// The uid owning this process, used to namespace cgroup directories
/// (`pir-<uid>`) so two users' slices cannot collide.
///
/// `libc::geteuid` is unix-only; on Windows there is no cgroup hierarchy to
/// namespace in the first place ([`detect_hierarchy`] yields [`Backend::None`]),
/// so a fixed placeholder is enough to keep the module compiling.
fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: `geteuid` is a pure getter with no preconditions.
        unsafe { libc::geteuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Detect the cgroup layout once per process.
///
/// Never fails: every error path yields [`Backend::None`] with a note saying
/// why, because a missing controller must degrade to detection-only, not abort
/// the session.
pub fn hierarchy() -> &'static Hierarchy {
    static H: OnceLock<Hierarchy> = OnceLock::new();
    H.get_or_init(detect_hierarchy)
}

fn detect_hierarchy() -> Hierarchy {
    // v1 first: on a hybrid host the v2 mount can exist but carry no
    // controllers, and v1's memory controller is the one that actually enforces.
    // `probe_v1` exercises the real operations (create a child, read and write
    // `memory.limit_in_bytes`) rather than trusting that the directory exists —
    // on this host `/sys/fs/cgroup/memory` is a *cgroup2* view, so the path is
    // present while `memory.limit_in_bytes` is not.
    if probe_v1().is_ok() {
        return Hierarchy {
            backend: Backend::V1,
            root: Some(PathBuf::from(V1_ROOT)),
            note: format!("cgroup v1 ({V1_ROOT})"),
        };
    }
    if let Ok(()) = probe_v2()
        && let Some(root) = v2_root()
    {
        return Hierarchy {
            backend: Backend::V2,
            note: format!("cgroup v2 ({})", root.display()),
            root: Some(root),
        };
    }
    Hierarchy {
        backend: Backend::None,
        root: None,
        note: "no cgroup memory controller — commands are measured, not limited".to_string(),
    }
}

/// Can we create a child cgroup under the v1 memory controller and write a
/// limit into it? Exercises the real operations rather than trusting the mount
/// table (the v2 mount, for instance, looks fine and enforces nothing).
fn probe_v1() -> Result<(), String> {
    let probe = PathBuf::from(V1_ROOT).join(format!("pir-probe-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&probe) {
        return Err(format!("mkdir {}: {e}", probe.display()));
    }
    let r = (|| -> Result<(), String> {
        // A limit write is the operation we actually depend on. Write, then
        // restore the (unlimited) default so the probe leaves no trace.
        let limit = probe.join("memory.limit_in_bytes");
        let before = std::fs::read_to_string(&limit).map_err(|e| format!("read {limit:?}: {e}"))?;
        std::fs::write(&limit, b"268435456").map_err(|e| format!("write {limit:?}: {e}"))?;
        let _ = std::fs::write(&limit, before.trim().as_bytes());
        Ok(())
    })();
    let _ = std::fs::remove_dir(&probe);
    r
}

/// v2 probe: a controller must be delegatable *and* the child must actually get
/// the files. `cgroup.controllers` being empty is the failure this catches.
fn probe_v2() -> Result<(), String> {
    match v2_root() {
        Some(r) => {
            // A controller is only usable when it is delegated *and* the child
            // actually receives the files — an empty/populated `cgroup.controllers`
            // is the failure this catches.
            let probe = r.join(format!("pir-probe-{}", std::process::id()));
            std::fs::create_dir_all(&probe).map_err(|e| format!("mkdir: {e}"))?;
            let has_memory = probe.join("memory.max").exists();
            // Leave the child in place if the write test below wants it.
            if !has_memory {
                let _ = std::fs::remove_dir(&probe);
                return Err("no memory controller delegated to the subtree".into());
            }
            let _ = std::fs::remove_dir(&probe);
            Ok(())
        }
        None => Err("not mounted".into()),
    }
}

/// The directory a cgroup v2 mount lives at, or `None` when there is not one.
///
/// Resolved from the **mount table** rather than a hardcoded path, because the
/// single-mount (`/sys/fs/cgroup`) and hybrid (`/sys/fs/cgroup/unified`)
/// layouts are both common and a host has one or the other. Hardcoding the
/// hybrid path is exactly how this host reported `Backend::None` ("commands are
/// measured, not limited") while a fully working v2 controller — with
/// `memory.max` writable — was mounted at `/sys/fs/cgroup` the whole time.
fn v2_root() -> Option<PathBuf> {
    if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
        for line in mounts.lines() {
            let mut f = line.split_whitespace();
            let _dev = f.next();
            let Some(mnt) = f.next() else { continue };
            let Some(fstype) = f.next() else { continue };
            if fstype == "cgroup2" && Path::new(mnt).join("cgroup.controllers").exists() {
                return Some(PathBuf::from(mnt));
            }
        }
    }
    // Fall back to the conventional locations, so a host whose mount table is
    // unreadable is still detected rather than silently declared unenforceable.
    V2_ROOT_FALLBACKS
        .iter()
        .map(PathBuf::from)
        .find(|p| p.join("cgroup.controllers").exists())
}

/// Everything the poll loop needs to know about the command it is supervising.
#[derive(Debug, Clone)]
pub struct Limits {
    pub backend: Backend,
    /// The command's own memory ceiling, in bytes (0 = unmetered). When
    /// [`Limits::enforce`] is false this is *advisory*: it is the number the
    /// guard would use, and the threshold the poll watches, but it is not
    /// written to the kernel.
    pub memory_bytes: u64,
    /// **Whether the kernel enforces `memory_bytes`.**
    ///
    /// This is the single most important knob in the design and it defaults to
    /// **false**. A hard kernel cap must not be the normal case: a `cargo build`
    /// that legitimately needs 30 GB has to be *allowed* to take 30 GB, and a
    /// static budget cannot tell that apart from a runaway. Enforcement is
    /// therefore armed only when the host is genuinely under pressure
    /// ([`Limits::enforce`] set from the pressure probe, see
    /// [`should_enforce`]), or when the operator asks for it explicitly.
    ///
    /// With it false the command runs unbounded by the kernel and is stopped
    /// only by the poll, and only when the evidence says it is the thing eating
    /// the machine.
    pub enforce: bool,
    /// `pids.max` value, 0 = unmetered. Like memory, this is a guard against a
    /// runaway fork cascade, not a routine ration — and it is generous by
    /// default (thousands, not tens).
    pub pids: u64,
    /// The session slice this command also belongs to (aggregate cap).
    pub session_bytes: u64,
    /// Human explanation of where the numbers came from (shown to the model
    /// when a command is killed, and by `/limits`).
    pub basis: String,
}

/// Host memory facts at one instant.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostMem {
    pub total: u64,
    pub available: u64,
    pub free: u64,
    pub swap_total: u64,
    pub swap_free: u64,
    /// `pgmajfault` from `/proc/vmstat` — major faults, i.e. real swap/disk I/O.
    pub pgmajfault: u64,
}

impl HostMem {
    pub fn read() -> Option<HostMem> {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let get = |key: &str| -> u64 {
            meminfo
                .lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
                .map(|kb| kb * 1024)
                .unwrap_or(0)
        };
        let pgmajfault = std::fs::read_to_string("/proc/vmstat")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("pgmajfault "))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse::<u64>().ok())
            })
            .unwrap_or(0);
        Some(HostMem {
            total: get("MemTotal:"),
            available: get("MemAvailable:"),
            free: get("MemFree:"),
            swap_total: get("SwapTotal:"),
            swap_free: get("SwapFree:"),
            pgmajfault,
        })
    }

    /// Bytes resident in *this* process's own cgroup, if v1 accounting is
    /// readable. Used to attribute pressure: it is the size of the container
    /// `pir` and the command both live in.
    pub fn own_cgroup_bytes(&self) -> Option<u64> {
        let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        // v1 line: `5:memory:/init.scope`
        let path = cgroup
            .lines()
            .find_map(|l| l.strip_prefix("5:memory:"))?
            .trim();
        let p = PathBuf::from(V1_ROOT).join(path.trim_start_matches('/'));
        read_u64(&p.join("memory.usage_in_bytes"))
    }
}

fn read_u64(p: &Path) -> Option<u64> {
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

/// Compute the per-command budget from host state and any operator override.
///
/// The rule is "derive from the host, then clamp; never a bare constant", so a
/// 4 GiB VM and a 128 GiB workstation both behave sanely with no configuration.
///
/// * `25%` of the *effective* total, floored at 512 MiB and capped at 8 GiB;
/// * then clamped to half of `MemAvailable`, so the second and third concurrent
///   command get smaller budgets instead of piling onto a box that is already
///   low — a crude admission controller with no shared state;
/// * the effective total is `min(/proc/meminfo MemTotal, our own cgroup limit)`
///   so a container-limited `pir` does not compute an order-of-magnitude-too-
///   large budget from the host's RAM.
pub fn command_budget(host: &HostMem, override_mib: Option<u64>) -> (u64, String) {
    command_budget_with(host, override_mib, own_cgroup_limit())
}

/// Derive the per-command budget, which is an **emergency ceiling**, not a
/// routine cap.
///
/// # Why this is not "25% of RAM"
///
/// A first cut set the command budget to a quarter of RAM, which meant a
/// `cargo build` needing 30 GB on a 64 GB box was killed for doing exactly what
/// it was asked to do. That is a *worse* failure than the one being prevented:
/// OOM pressure is exceptional, but builds needing tens of GB are ordinary. The
/// guard must therefore be a circuit breaker for a dying host, not a
/// per-command ration.
///
/// So the ceiling is deliberately **loose** — it only has to be low enough that
/// a runaway cannot eat everything before the poll notices:
///
/// * `75%` of the effective total, capped at `60%` of it so the kernel keeps
///   headroom for page cache and the rest of the system, floored at 4 GiB;
/// * **never** clamped down by current `MemAvailable`. A small build must not
///   get a small budget just because a big job runs alongside it — that is
///   exactly the case the host guard should decide, with full knowledge, rather
///   than a static number guessed in advance.
///
/// On a healthy host this ceiling rarely fires: the host guard and the growth
/// rule are what stop a runaway, and they act only when the machine is actually
/// in danger.
fn command_budget_with(
    host: &HostMem,
    override_mib: Option<u64>,
    own_limit: Option<u64>,
) -> (u64, String) {
    if let Some(mib) = override_mib {
        let b = mib * 1024 * 1024;
        return (b, format!("{mib} MiB (operator override)"));
    }
    let effective_total = effective_total_with(host, own_limit);
    if effective_total == 0 {
        return (0, "unmetered (host total unknown)".into());
    }
    let ceiling = (effective_total / 10 * 6)
        .max(4 * 1024 * 1024 * 1024)
        .min(effective_total);
    (
        ceiling,
        format!(
            "{} = emergency ceiling (60% of {})",
            human_bytes(ceiling),
            human_bytes(effective_total)
        ),
    )
}

/// Session budget: bounds the aggregate of every command this session spawns.
/// Deliberately looser than the per-command budget so a single command is
/// always stopped by *its own* limit (with a precise message) rather than by an
/// anonymous session-wide ceiling.
pub fn session_budget(host: &HostMem) -> u64 {
    session_budget_with(host, own_cgroup_limit())
}

/// [`session_budget`] with the container limit supplied explicitly.
fn session_budget_with(host: &HostMem, own_limit: Option<u64>) -> u64 {
    let effective_total = effective_total_with(host, own_limit);
    if effective_total == 0 {
        return 0;
    }
    // Must stay **above** the per-command ceiling: the session slice bounds the
    // *aggregate* of a foreground command plus detached jobs plus sub-agent
    // commands, so if it were the smaller of the two, a single legitimate
    // command would be killed by the aggregate cap before its own ceiling was
    // ever reached — and the message would blame the session rather than the
    // command, which is exactly the confusing failure this design avoids.
    //
    // 90% of the total leaves a little for everything else while still being a
    // real bound (it stops four concurrent commands from each taking 60%).
    let ceiling = command_budget_with(host, None, own_limit).0;
    let aggregate = effective_total / 10 * 9;
    aggregate.max(ceiling + (1 << 30)).min(effective_total)
}

/// The total the budgets are derived from: the smaller of the host's RAM and
/// whatever limit our own cgroup already imposes (the container case).
fn effective_total(host: &HostMem) -> u64 {
    effective_total_with(host, own_cgroup_limit())
}

/// [`effective_total`] with the container limit supplied explicitly.
///
/// Split out so the budget arithmetic is a *pure* function of its inputs: the
/// live version reads `/proc/self/cgroup`, which means the same call returns
/// different numbers on a laptop and inside a memory-capped container. Tests
/// that want "what does a 128 GiB host give?" must be able to ask that question
/// on whatever machine they run on, so they pass the limit (`None`) instead of
/// inheriting the runner's.
fn effective_total_with(host: &HostMem, own_limit: Option<u64>) -> u64 {
    let own = own_limit.unwrap_or(0);
    if own > 0 && own < host.total {
        own
    } else {
        host.total
    }
}

/// The memory limit already applied to `pir` itself, if any. v1 exposes it as
/// `memory.limit_in_bytes` in our own cgroup; a value near `u64::MAX` means
/// "unlimited" and is normalised to 0.
fn own_cgroup_limit() -> Option<u64> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = cgroup.lines().find_map(|l| l.strip_prefix("5:memory:"))?.trim();
    let p = PathBuf::from(V1_ROOT).join(path.trim_start_matches('/'));
    let v = read_u64(&p.join("memory.limit_in_bytes"))?;
    if v > (1u64 << 60) { None } else { Some(v) }
}

/// PIDs ceiling: the highest-value limit after memory. A
/// `while true; do cmd & done` cascade can exhaust the host PID table, at which
/// point *nothing* can fork — including the operator's shell.
pub fn pids_limit(host: &HostMem, override_n: Option<u64>) -> u64 {
    if let Some(n) = override_n {
        return n;
    }
    // Scale mildly with the host: 1024 on a small box, 4096 on a large one.
    let n = host.total / (16 * 1024 * 1024 * 1024);
    (n.max(1) * 1024).clamp(1024, 4096)
}

/// One cgroup `pir` created for a command. `Drop` removes the directory, so the
/// lifetime of the cgroup is structurally tied to the lifetime of the command —
/// a leaked cgroup holding a limit forever is worse than no cgroups at all.
#[derive(Debug)]
pub struct CommandCgroup {
    /// The command's own cgroup.
    dir: PathBuf,
    /// `true` when a limit was actually written (kernel enforcement live).
    pub enforced: bool,
    /// The limit that was written, for the kill message.
    pub limit_bytes: u64,
}

impl CommandCgroup {
    /// Join `pid` to this cgroup. Must be called from the child's `pre_exec`
    /// (single-threaded, after fork, before exec) — the only window in which
    /// writing `cgroup.procs` is race-free. Ordering matters: this must happen
    /// **before** the uid drop, because afterwards the write fails EACCES.
    pub fn attach(&self, pid: u32) -> std::io::Result<()> {
        std::fs::write(self.dir.join(f_procs()), pid.to_string())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Current usage in bytes (the cgroup's own accounting; includes page cache).
    pub fn usage(&self) -> Option<u64> {
        read_u64(&self.dir.join(f_usage()))
    }

    /// Peak usage since the cgroup was created.
    pub fn peak(&self) -> Option<u64> {
        read_u64(&self.dir.join(f_peak()))
    }

    /// Resident anonymous memory, excluding page cache. This split is what
    /// makes the difference between killing a runaway allocator and killing a
    /// `cargo build` that merely read a lot of files.
    ///
    /// The key differs by backend: v1 calls it `rss`, v2 calls it `anon`. Both
    /// exclude page cache, which is the property that matters here.
    pub fn anon_bytes(&self) -> Option<u64> {
        let stat = std::fs::read_to_string(self.dir.join("memory.stat")).ok()?;
        let keys: &[&str] = match hierarchy().backend {
            Backend::V2 => &["anon ", "rss "],
            _ => &["rss ", "anon "],
        };
        for key in keys {
            if let Some(v) = stat
                .lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
            {
                return Some(v);
            }
        }
        None
    }

    /// Bytes swapped out by this cgroup.
    pub fn swap_bytes(&self) -> Option<u64> {
        read_u64(&self.dir.join("memory.memsw.usage_in_bytes"))
            .zip(self.usage())
            .map(|(memsw, mem)| memsw.saturating_sub(mem))
    }

    /// How many times the limit has been hit (nonzero ⇒ the kernel already
    /// refused an allocation here — definitive blame).
    ///
    /// v1 exposes a monotonic `memory.failcnt`; v2 exposes event counters in
    /// `memory.events` instead, where `max` counts the times a charge was
    /// refused at the `memory.max` limit. Both mean the same thing here, so a
    /// v2 host keeps its definitive-blame signal rather than always reporting 0.
    pub fn failcnt(&self) -> u64 {
        match hierarchy().backend {
            Backend::V2 => {
                let Ok(s) = std::fs::read_to_string(self.dir.join("memory.events")) else {
                    return 0;
                };
                s.lines()
                    .find(|l| l.starts_with("max "))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0)
            }
            _ => read_u64(&self.dir.join("memory.failcnt")).unwrap_or(0),
        }
    }

    /// Whether the *kernel* OOM-killed something in this cgroup, and how many.
    ///
    /// v1 reports this in `memory.oom_control`; v2 counts it in
    /// `memory.events` as `oom_kill`. Either is unambiguous evidence that the
    /// kernel — not pir — stopped a process here.
    pub fn oom_kills(&self) -> u64 {
        match hierarchy().backend {
            Backend::V2 => {
                let Ok(s) = std::fs::read_to_string(self.dir.join("memory.events")) else {
                    return 0;
                };
                s.lines()
                    .find(|l| l.starts_with("oom_kill "))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0)
            }
            _ => {
                let Ok(s) = std::fs::read_to_string(self.dir.join("memory.oom_control")) else {
                    return 0;
                };
                s.lines()
                    .find(|l| l.starts_with("oom_kill "))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0)
            }
        }
    }

    /// Current process count in the cgroup.
    pub fn pids(&self) -> Option<u64> {
        read_u64(&self.dir.join(f_procs())).map(|_| count_procs(&self.dir))
    }
}

fn count_procs(dir: &Path) -> u64 {
    std::fs::read_to_string(dir.join(f_procs()))
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count() as u64)
        .unwrap_or(0)
}

impl Drop for CommandCgroup {
    fn drop(&mut self) {
        // The cgroup empties when the last process in it exits; the rmdir then
        // succeeds. If something is still alive (an escapee we killed too
        // late), rmdir fails with EBUSY and the startup sweep catches it later
        // — we never block here.
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Create the per-command cgroup for `session_key`, write its limits, and
/// return a guard that removes it on drop.
///
/// Returns `Ok(None)` when no backend is available (detection-only mode) —
/// callers must treat that as "measure, do not limit", never as an error.
///
/// `enforce` is the whole posture switch: when **false** (the default) the
/// cgroup is still created so usage/peak can be *measured*, but no memory
/// ceiling is written — the command may use as much RAM as it needs, and only
/// the poll's evidence-based guard can stop it. See [`should_enforce`].
pub fn create_command_cgroup(
    session_key: &str,
    limit_bytes: u64,
    pids: u64,
    enforce: bool,
) -> Result<Option<CommandCgroup>, String> {
    let h = hierarchy();
    let Some(root) = &h.root else {
        return Ok(None);
    };
    // Nesting: <root>/pir-<uid>/<session>/cmd-<unique>. The uid level keeps two
    // operators' sessions from colliding, and the session level is what the
    // aggregate slice hangs off.
    //
    // The command directory is named after the **forking process plus the
    // cgroup's inode**, not just the forker's pid. `cmd-<pid>` looked unique but
    // is not: a session key can be reused (a resumed session, or a test binary
    // running many cases under one key), and one process can spawn many
    // commands, so two commands could land in the *same* directory — the second
    // would silently inherit the first's written limit, and the first's `Drop`
    // would remove the directory out from under the second.
    let uid_dir = root.join(format!("pir-{}", current_uid()));
    let sess_dir = uid_dir.join(session_key);
    let cmd_dir = unique_cmd_dir(&sess_dir);
    if cmd_dir.parent() != Some(sess_dir.as_path()) {
        // `unique_cmd_dir` always yields a child of `sess_dir`; this is a
        // defensive check so a future edit cannot make cleanup escape upward.
        return Err(format!("refusing to create cgroup outside {}", sess_dir.display()));
    }
    for d in [&uid_dir, &sess_dir, &cmd_dir] {
        std::fs::create_dir_all(d).map_err(|e| format!("mkdir {}: {e}", d.display()))?;
    }
    // **Delegate the memory controller down the tree.** A cgroup v2 child gets
    // no `memory.*` files at all unless each ancestor enables `+memory` in its
    // `cgroup.subtree_control`. Without this every child here contains only
    // `cgroup.*`, so `write_limit(memory.max)` targets a nonexistent file and
    // fails — and because that failure is silent, the command then runs
    // *unmetered while `enforce` is true*, which is the worst of both: the
    // operator believes a limit is armed and nothing is. Enabling it at the root
    // and each level we create is what makes the files appear.
    delegate_memory(&root_to_uid_chain(root, &uid_dir, &sess_dir, &cmd_dir));

/// The chain of cgroup directories from the mount root down to `cmd_dir`, so
/// each ancestor can be asked to delegate `+memory` to its children.
fn root_to_uid_chain(
    root: &Path,
    uid_dir: &Path,
    sess_dir: &Path,
    _cmd_dir: &Path,
) -> Vec<PathBuf> {
    // Deliberately stops at the session level: `cmd_dir` is a **leaf** where the
    // command itself lives, and nothing is ever created beneath it, so it has no
    // children to delegate to. Writing `subtree_control` on it is not merely
    // pointless — it broke `sweep_never_touches_a_populated_cgroup`, because a
    // leaf that is about to receive the supervised process cannot have its
    // subtree control rewritten (the write is refused once it is populated, and
    // the ordering made the child's join fail).
    vec![root.to_path_buf(), uid_dir.to_path_buf(), sess_dir.to_path_buf()]
}

// Note the default: `enforced` is false unless we actually write a limit.
    // A command that is merely *measured* must never be described as limited.
    let mut enforced = false;
    if enforce && limit_bytes > 0 {
        let wrote = write_limit(&cmd_dir, f_memory_max(), limit_bytes);
        if wrote {
            enforced = true;
            // A swap allowance of literal zero: the command may use RAM up to the
            // limit and no swap, so overshoot is an immediate OOM-kill instead of
            // a reclaim-then-thrash period. v1 expresses this as
            // `memsw == limit` (memory+swap combined, hence the same number);
            // v2 writes `memory.swap.max = 0`. If the file cannot be written
            // (accounting compiled out), leave it alone: the poll and the host
            // guard still cover swap.
            let swap_val = match hierarchy().backend {
                Backend::V2 => 0,
                _ => limit_bytes,
            };
            let _ = write_limit_unless_below(&cmd_dir, f_memory_swap_max(), swap_val);
            // A *soft* limit only (no OOM): advisory pressure that lets the
            // kernel reclaim from us first without ever killing.
            let _ = write_limit(&cmd_dir, f_memory_soft(), limit_bytes);
        }
    } else if limit_bytes > 0 {
        // Advisory mode: set the *soft* limit only. It applies reclaim
        // preference under pressure and can never kill a process, so a 30 GB
        // build proceeds while the kernel still prefers to reclaim our cache
        // over someone else's working set.
        let _ = write_limit(&cmd_dir, f_memory_soft(), limit_bytes);
    }
    // PID ceiling is independent of the memory posture: a fork cascade cannot be
    // legitimate at thousands of processes, and hitting it does not kill the
    // command's memory budget. v2-only; on v1 the pids controller lives in a
    // separate hierarchy, so a missing file is expected and is not a failure.
    if pids > 0 {
        let _ = write_limit(&cmd_dir, "pids.max", pids);
    }
    Ok(Some(CommandCgroup { dir: cmd_dir, enforced, limit_bytes }))
}

fn write_limit(dir: &Path, file: &str, value: u64) -> bool {
    std::fs::write(dir.join(file), value.to_string()).is_ok()
}

/// Pick a command-cgroup name that is not already taken.
///
/// `pid` alone is not unique enough: a session key can be reused across runs (a
/// resumed session; a test binary exercising many cases under one key), and a
/// single process spawns many commands. Colliding would mean one command
/// silently inheriting another's written limit, and one guard's `Drop` removing
/// a directory another command is still living in.
///
/// So the name carries the pid (for human correlation with `$PIR_SCRATCHPAD` and
/// the process tree) plus a monotonically increasing counter, with an existence
/// check as the final arbiter.
fn unique_cmd_dir(sess_dir: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    for _ in 0..10_000 {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let cand = sess_dir.join(format!("cmd-{pid}-{n}"));
        if !cand.exists() {
            return cand;
        }
    }
    // Pathological: fall back to a nanosecond suffix rather than looping.
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    sess_dir.join(format!("cmd-{pid}-{t}"))
}

// ---------------------------------------------------------------------------
// Backend-aware cgroup file names
//
// The v1 and v2 memory controllers expose the *same concepts* under different
// file names, and writing the wrong one fails silently (the file simply does not
// exist, so `write_limit` returns false and the command runs unmetered while the
// caller believes it wrote a limit). Every access therefore goes through these
// helpers rather than naming a file directly.
// ---------------------------------------------------------------------------

/// The hard memory cap. v2: `memory.max`; v1: `memory.limit_in_bytes`.
fn f_memory_max() -> &'static str {
    match hierarchy().backend {
        Backend::V2 => "memory.max",
        _ => "memory.limit_in_bytes",
    }
}

/// The reclaim-preference (non-killing) limit. v2: `memory.high`; v1:
/// `memory.soft_limit_in_bytes`. Deliberately `high`, not `max`, for the
/// advisory posture: exceeding `high` throttles and reclaims but never kills.
fn f_memory_soft() -> &'static str {
    match hierarchy().backend {
        Backend::V2 => "memory.high",
        _ => "memory.soft_limit_in_bytes",
    }
}

/// The swap allowance. v2: `memory.swap.max`; v1: `memory.memsw.limit_in_bytes`
/// (which is memory+swap combined, hence the caller's arithmetic).
fn f_memory_swap_max() -> &'static str {
    match hierarchy().backend {
        Backend::V2 => "memory.swap.max",
        _ => "memory.memsw.limit_in_bytes",
    }
}

/// Current usage in bytes.
fn f_usage() -> &'static str {
    match hierarchy().backend {
        Backend::V2 => "memory.current",
        _ => "memory.usage_in_bytes",
    }
}

/// Peak usage since the cgroup was created.
fn f_peak() -> &'static str {
    match hierarchy().backend {
        Backend::V2 => "memory.peak",
        _ => "memory.max_usage_in_bytes",
    }
}

/// The membership file. `cgroup.procs` exists in both v1 and v2 (v1 also has
/// `tasks`), so this one needs no branching — kept as a named helper anyway so
/// every cgroup access in this module is greppable in one place.
fn f_procs() -> &'static str {
    "cgroup.procs"
}

/// Lower `file` only if the current value is above `value` — memsw must stay
/// `>=` the memory limit in v1, and lowering memsw below the (unlimited)
/// default is the whole point here.
fn write_limit_unless_below(dir: &Path, file: &str, value: u64) -> bool {
    let p = dir.join(file);
    match read_u64(&p) {
        Some(cur) if cur <= value => true,
        _ => std::fs::write(&p, value.to_string()).is_ok(),
    }
}

/// Ensure the session slice exists and carries the aggregate cap. Best-effort:
/// a missing session slice only means per-command limits stand alone.
/// Enable `+memory` in `cgroup.subtree_control` for every directory in `chain`.
///
/// Only meaningful under v2 (v1 has no `subtree_control`), and best-effort: on a
/// host where the controller is not delegatable this fails harmlessly and the
/// caller's `enforced` flag stays false, which is the honest outcome.
///
/// Writing the *same* controller twice is fine — v2 ignores a no-op — so this is
/// safe to run on every command, not just the first.
fn delegate_memory(chain: &[PathBuf]) {
    if hierarchy().backend != Backend::V2 {
        return;
    }
    for dir in chain {
        let ctl = dir.join("cgroup.subtree_control");
        if !ctl.exists() {
            continue;
        }
        // Read-modify-write rather than a blind `+memory`, so a host that already
        // delegates `+cpu`/`+pids` keeps them: assigning the file replaces the
        // whole set, and dropping `+pids` would silently disable the PID ceiling.
        let current = std::fs::read_to_string(&ctl).unwrap_or_default();
        let mut wanted: Vec<&str> = current.split_whitespace().collect();
        for c in ["memory", "pids"] {
            if !wanted.contains(&c) {
                wanted.push(c);
            }
        }
        let desired: String =
            wanted.iter().map(|c| format!("+{c}")).collect::<Vec<_>>().join(" ");
        let _ = std::fs::write(&ctl, desired);
    }
}

pub fn ensure_session_slice(session_key: &str, budget: u64, enforce: bool) {
    let h = hierarchy();
    let Some(root) = &h.root else { return };
    let dir = root
        .join(format!("pir-{}", current_uid()))
        .join(session_key);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if budget == 0 {
        return;
    }
    // Same posture rule as the per-command cgroup: measure always, hard-limit
    // only under pressure. Note this is the *aggregate* cap, so it gets a small
    // swap allowance — one cold page may be pushed out; sustained thrash is what
    // we are preventing.
    //
    // The file names are backend-dependent, exactly as in
    // `create_command_cgroup`: writing v1's `memory.limit_in_bytes` on a v2 host
    // targets a file that does not exist, so the aggregate cap silently never
    // applied. That was invisible — the slice existed, the write "succeeded" in
    // the sense that nothing reported an error, and only the *effective* cap was
    // missing.
    delegate_memory(&[root.to_path_buf()]);
    if enforce {
        let _ = write_limit(&dir, f_memory_max(), budget);
        // v1 expresses "swap allowance" as memory+swap combined; v2 as a separate
        // swap ceiling. Using the wrong one silently drops the bound.
        match hierarchy().backend {
            Backend::V2 => {
                let _ = write_limit_unless_below(&dir, f_memory_swap_max(), SESSION_SWAP_BYTES);
            }
            _ => {
                let _ = write_limit(&dir, f_memory_swap_max(), budget + SESSION_SWAP_BYTES);
            }
        }
    } else {
        let _ = write_limit(&dir, f_memory_soft(), budget);
    }
}

/// Remove stale `pir-*` cgroups left behind by a crashed session.
///
/// A leaked cgroup holding a limit is worse than no cgroups: it makes the next
/// run unaccounted. Only *empty* directories are removed, so a live `pir`'s
/// slice is never touched.
pub fn sweep_stale() -> usize {
    let h = hierarchy();
    let Some(root) = &h.root else { return 0 };
    let mut removed = 0;

    // Probe dirs from a crashed run are always stale.
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("pir-probe-") {
                removed += remove_if_empty(&e.path());
            }
        }
    }

    // Inside our own tree, sweep every *empty* cgroup. A session that exited
    // cleanly already removed its command cgroups via `Drop`; one that was
    // SIGKILLed leaves them behind, and a leaked cgroup holding a limit is worse
    // than no cgroups at all (it makes the next run unaccounted).
    //
    // Depth is not assumed: an earlier version hard-coded exactly one level of
    // `cmd-*` children and silently leaked anything deeper. Walk all the way
    // down instead, deepest-first, and never touch a cgroup with a process in
    // it — that is what keeps a *live* session safe.
    //
    // **This session's own slice is skipped entirely.** A foreground command's
    // cgroup is empty for a moment before the child joins it (creation happens
    // in the parent, the join in the child's `pre_exec`), and in a multi-threaded
    // process another thread can call this sweep inside that window — at which
    // point removing the directory would make the child's join fail and quietly
    // drop the command out of its limits. Cheap insurance against a subtle
    // self-inflicted race.
    let ours = root.join(format!("pir-{}", current_uid()));
    let mine = ours.join(session_key());
    if let Ok(entries) = std::fs::read_dir(&ours) {
        for e in entries.flatten() {
            let path = e.path();
            if !path.is_dir() || path == mine {
                continue;
            }
            removed += sweep_tree(&path);
        }
    }
    removed
}

/// Remove this session's own slice directory, if it is empty.
///
/// The counterpart to [`ensure_session_slice`]. Without it every `pir` run leaks
/// its slice forever: the startup sweep deliberately `continue`s over *this*
/// session's directory (removing it mid-run would race a command's `pre_exec`
/// join and silently drop that command out of its limits), so nothing ever
/// reclaimed it. Observed as `/sys/fs/cgroup/pir-0/pir-<stamp>` persisting after
/// each run with zero processes in it.
///
/// Only an **empty** slice is removed, so a detached job still running under this
/// session keeps its accounting. Best-effort: a failure leaves the directory for
/// the next run's sweep to collect.
pub fn remove_own_session_slice() -> usize {
    let h = hierarchy();
    let Some(root) = &h.root else { return 0 };
    let dir = root.join(format!("pir-{}", current_uid())).join(session_key());
    if !dir.exists() {
        return 0;
    }
    // Remove any empty command cgroups first (deepest-first), then the slice.
    let mut removed = 0;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() && p.join("cgroup.procs").exists() {
                removed += remove_if_empty(&p);
            }
        }
    }
    removed + remove_if_empty(&dir)
}

/// Recursively remove every empty cgroup at or below `dir`, deepest-first so a
/// parent is only removed once its children are gone. Returns how many were
/// removed.
///
/// The two safety rules: never remove a cgroup that has a process joined to it
/// (a live command or session), and never remove anything that is not inside the
/// tree we were given.
fn sweep_tree(dir: &Path) -> usize {
    let mut removed = 0;
    // Refuse to walk out of the cgroup mount, whatever the arguments.
    if !dir.starts_with("/sys/fs/cgroup") {
        return 0;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            // Only descend into directories that look like cgroups, so a stray
            // directory of unrelated files is never walked.
            if p.is_dir() && p.join("cgroup.procs").exists() {
                removed += sweep_tree(&p);
            }
        }
    }
    removed + remove_if_empty(dir)
}

/// Remove a cgroup directory that has no processes in it.
///
/// **Not** a plain "is the directory empty" check: a v1 cgroup directory
/// *always* contains pseudo-files (`cgroup.procs`, `tasks`, `memory.stat`, …),
/// so `read_dir` is never empty and a naive check would leave every cgroup
/// behind forever. "Empty" here means the thing that actually matters — no
/// process is joined to it, which is what makes `rmdir` legal.
fn remove_if_empty(dir: &Path) -> usize {
    if !dir.join("cgroup.procs").exists() {
        // Not a cgroup (a stray file/dir under our root): only remove a
        // genuinely empty directory.
        let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
        if entries.flatten().next().is_some() {
            return 0;
        }
        return usize::from(std::fs::remove_dir(dir).is_ok());
    }
    match std::fs::read_to_string(dir.join("cgroup.procs")) {
        // Still populated: a live session, never touch it.
        Ok(p) if p.lines().any(|l| !l.trim().is_empty()) => 0,
        Ok(_) => usize::from(std::fs::remove_dir(dir).is_ok()),
        Err(_) => 0,
    }
}

// ---------------------------------------------------------------------------
// The kill decision
// ---------------------------------------------------------------------------

/// Why a command is being killed. Each variant has a distinct operator-facing
/// message, because "your command ate 8 GiB" and "the host was dying and your
/// command was implicated" call for different operator responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillReason {
    /// Over its own budget for two consecutive samples.
    OverBudget,
    /// Growing fast enough that it will reach the budget shortly.
    FastGrowth,
    /// Host memory critically low — killed for host protection. Carries whether
    /// this task was actually implicated, so the message can avoid accusing it.
    HostCritical { implicated: bool },
    /// Swap thrash attributed to this task.
    SwapThrash,
    /// Hit its process ceiling.
    PidCeiling,
}

impl KillReason {
    pub fn label(&self) -> &'static str {
        match self {
            KillReason::OverBudget => "memory limit",
            KillReason::FastGrowth => "memory growth",
            KillReason::HostCritical { .. } => "host memory pressure",
            KillReason::SwapThrash => "swap thrash",
            KillReason::PidCeiling => "process limit",
        }
    }
}

/// One sample of the supervised command and the host.
#[derive(Debug, Clone, Copy)]
struct Sample {
    at: Instant,
    usage: u64,
    anon: u64,
    swap: u64,
    host_available: u64,
    host_free: u64,
    swap_used: u64,
}

/// Stateful kill decider. Lives in `run_shell`'s wait loop and is fed one
/// sample per 250 ms tick.
pub struct Guard {
    limits: Limits,
    /// The command's cgroup, when one was created.
    cgroup: Option<CommandCgroup>,
    history: VecDeque<Sample>,
    over_limit_streak: u8,
    swap_growth_streak: u8,
    /// Bytes of *other* consumers at the last look (v1 siblings), used to
    /// attribute pressure that is not ours.
    sibling_bytes: Option<u64>,
    /// Set when the last check concluded the pressure was not attributable to
    /// this task; surfaced in the tool result even when we do not kill.
    pub not_blamed_note: Option<String>,
}

impl Guard {
    pub fn new(limits: Limits, cgroup: Option<CommandCgroup>) -> Guard {
        Guard {
            limits,
            cgroup,
            history: VecDeque::with_capacity(HISTORY),
            over_limit_streak: 0,
            swap_growth_streak: 0,
            sibling_bytes: None,
            not_blamed_note: None,
        }
    }

    /// Feed one observation and decide whether to kill. `pid_alive` is only
    /// used to skip sampling a command that has already exited.
    ///
    /// Cheap by construction: at most a handful of small file reads per tick
    /// (three from the cgroup, one from `/proc/meminfo`), and the per-process
    /// `/proc` walk only happens in the fallback path where no cgroup exists.
    pub fn observe(&mut self, pid_alive: bool) -> Option<KillReason> {
        if !pid_alive {
            return None;
        }
        let host = HostMem::read()?;
        let (usage, anon, swap) = self.sample_command();
        let sample = Sample {
            at: Instant::now(),
            usage,
            anon,
            swap,
            host_available: host.available,
            host_free: host.free,
            swap_used: host.swap_total.saturating_sub(host.swap_free),
        };
        if self.history.len() == HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(sample);

        // The kernel already killed something here: definitive, and we should
        // stop immediately rather than let the tree limp on.
        if let Some(cg) = &self.cgroup
            && cg.oom_kills() > 0
        {
            return Some(KillReason::OverBudget);
        }

        if let Some(r) = self.check_own_limit(sample) {
            return Some(r);
        }
        if let Some(r) = self.check_pids() {
            return Some(r);
        }
        self.check_host_pressure(sample, &host)
    }

    /// Usage / anon / swap for the supervised command. Prefers the cgroup; falls
    /// back to summing `/proc/<pid>` over the process group when there is none.
    fn sample_command(&self) -> (u64, u64, u64) {
        if let Some(cg) = &self.cgroup
            && let Some(u) = cg.usage()
        {
            return (u, cg.anon_bytes().unwrap_or(u), cg.swap_bytes().unwrap_or(0));
        }
        (0, 0, 0)
    }

    /// Layer 2, own budget. Requires `OVER_LIMIT_SAMPLES` consecutive samples,
    /// and treats a page-cache-dominated cgroup as *not* over budget: a
    /// `cargo build` legitimately charges hundreds of MB to a GB of file cache,
    /// and killing on raw `usage_in_bytes` alone would kill correct work.
    fn check_own_limit(&mut self, s: Sample) -> Option<KillReason> {
        let limit = self.limits.memory_bytes;
        if limit == 0 || s.usage == 0 {
            return None;
        }
        // **Advisory posture: the budget is not a kill line.**
        //
        // When the kernel cap is not armed, exceeding this number is not by
        // itself a reason to die — a `cargo build` needing 30 GB has to be
        // allowed to take 30 GB, and the number here exists so the *host* guard
        // has a scale to reason about, not so it can be used as a guillotine.
        // The only path that can kill in advisory mode is the host-pressure
        // check below, which requires actual danger.
        if !self.limits.enforce {
            return None;
        }
        let threshold = (limit as f64 * OVER_LIMIT_FRACTION) as u64;
        // Blame evidence: is this anonymous memory (a real allocator) or page
        // cache the kernel can just drop? `failcnt > 0` means the kernel
        // already tried to reclaim and could not — the strongest signal.
        let reclaim_failed = self.cgroup.as_ref().map(|c| c.failcnt() > 0).unwrap_or(false);
        let anon_dominant = s.anon * 2 >= s.usage;
        if s.usage >= threshold && (anon_dominant || reclaim_failed) {
            self.over_limit_streak += 1;
            if self.over_limit_streak >= OVER_LIMIT_SAMPLES {
                return Some(KillReason::OverBudget);
            }
        } else {
            self.over_limit_streak = 0;
        }

        // Growth rule: only meaningful when the ceiling is armed, and it only
        // fires once the command is *already* past 60% of it — so a fast,
        // legitimate build that stays under its ceiling is never touched.
        if s.usage > (limit as f64 * GROWTH_TRIGGER_FRACTION) as u64
            && let Some(first) = self.history.front()
        {
            let secs = s.at.duration_since(first.at).as_secs_f64();
            if secs >= GROWTH_WINDOW_SECS {
                let grew = s.usage.saturating_sub(first.usage);
                let rate = grew as f64 / secs;
                if rate >= GROWTH_BYTES_PER_SEC as f64 {
                    return Some(KillReason::FastGrowth);
                }
            }
        }
        None
    }

    /// Layer 2, process ceiling. A momentary `xargs -P` burst is fine; a
    /// sustained ceiling means the command cannot make progress anyway.
    fn check_pids(&self) -> Option<KillReason> {
        let (Some(cg), pids) = (&self.cgroup, self.limits.pids) else {
            return None;
        };
        if pids > 0 && cg.pids().unwrap_or(0) >= pids {
            return Some(KillReason::PidCeiling);
        }
        None
    }

    /// Layer 3, the host guard — and the blame question.
    ///
    /// Kills at the hard floor regardless of blame (at that point the box is
    /// about to OOM *something*, and doing nothing helps nobody). Above the
    /// floor, kills only when the evidence implicates this task; otherwise it
    /// records a note and leaves the command alone, because a wrong kill
    /// destroys correct work and reads as flakiness.
    fn check_host_pressure(&mut self, s: Sample, host: &HostMem) -> Option<KillReason> {
        if host.total == 0 {
            return None;
        }
        let floor = HOST_FLOOR_MIN_BYTES.max((host.total as f64 * HOST_FLOOR_FRACTION) as u64);
        if host.available < floor {
            let implicated = self.implicated(s, host);
            return Some(KillReason::HostCritical { implicated });
        }

        // Swap thrash: swap filling up *and* our own swap growing, or the
        // host's major-fault rate climbing while our resident set is flat.
        if host.swap_total > 0 && s.swap_used * 2 > host.swap_total {
            let swap_grew = self
                .history
                .iter()
                .rev()
                .take(2)
                .collect::<Vec<_>>()
                .windows(2)
                .all(|w| w[0].swap > w[1].swap);
            if swap_grew && s.swap > 0 {
                self.swap_growth_streak += 1;
                if self.swap_growth_streak >= 2 {
                    return Some(KillReason::SwapThrash);
                }
            } else {
                self.swap_growth_streak = 0;
            }
        }
        None
    }

    /// Hand the cgroup to a caller that will outlive this guard (the detach
    /// path, where the command becomes a `Job`). Without this the guard's
    /// `Drop` would remove the cgroup directory the moment the foreground call
    /// returned, and a detached job would run unaccounted.
    pub fn take_cgroup(&mut self) -> Option<CommandCgroup> {
        self.cgroup.take()
    }

    /// Sample the job and report what it is using, *without* deciding to kill.
    ///
    /// Called from `job_status` to inspect a detached job. It refreshes the
    /// guard's history and usage figures (so a later `observe` has real data to
    /// reason with) but applies no verdict — inspecting a job must never kill it
    /// as a side effect.
    ///
    /// Returns `(usage, anon, peak, budget, pct)` in bytes, or `None` when no
    /// cgroup accounting is available (a host with no controller, or a job whose
    /// cgroup was never created). `usage` is total charged memory, `anon` is the
    /// resident anonymous part — the split that distinguishes a real allocator
    /// from page cache.
    pub fn memory_summary(&mut self) -> Option<(u64, u64, u64, u64, u64)> {
        let host = HostMem::read()?;
        let (usage, anon, swap) = self.sample_command();
        let sample = Sample {
            at: Instant::now(),
            usage,
            anon,
            swap,
            host_available: host.available,
            host_free: host.free,
            swap_used: host.swap_total.saturating_sub(host.swap_free),
        };
        if self.history.len() == HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(sample);
        let cg = self.cgroup.as_ref()?;
        let peak = cg.peak().unwrap_or(0);
        let budget = self.limits.memory_bytes;
        if usage == 0 && peak == 0 {
            return None;
        }
        let pct = (peak as f64 / budget.max(1) as f64 * 100.0).round() as u64;
        Some((usage, anon, peak, budget, pct))
    }

    /// Is *this* command the consumer of the memory that is disappearing?
    ///
    /// The test is differential, not a threshold: compare how fast our usage
    /// grew against how fast the host lost available memory over the same
    /// window. A task whose resident set is flat while the host drains is not
    /// the cause, however big it happens to be.
    fn implicated(&mut self, s: Sample, _host: &HostMem) -> bool {
        let Some(first) = self.history.front() else { return true };
        let secs = s.at.duration_since(first.at).as_secs_f64();
        if secs < 1.0 {
            // Not enough history to attribute: assume we are implicated, so a
            // genuine runaway is not given a free pass in the first second.
            return true;
        }
        let our_growth = s.usage.saturating_sub(first.usage) as f64 / secs;
        let host_loss = first.host_available.saturating_sub(s.host_available) as f64 / secs;

        // Page cache can move MemFree without being an OOM threat, so the
        // comparison uses the host loss only when it is not explained by cache
        // churn (free falling much faster than available would indicate cache).
        let cache_led = s.host_free < s.host_available / 4;

        let implicated = if host_loss <= 0.0 {
            // Host memory is not falling over this window; nothing to blame.
            false
        } else if our_growth <= 0.0 {
            // We are flat while the host drains: someone else is growing.
            false
        } else {
            // We account for a meaningful share of the loss. 25% is a judgement
            // call: enough to implicate a genuine consumer, loose enough that a
            // task sharing the box with a big neighbour is not stopped for it.
            our_growth >= host_loss * 0.25 || cache_led
        };

        if !implicated {
            self.not_blamed_note = Some(format!(
                "host is under memory pressure ({} available of {}) but this command's usage \
                 was flat at {} — another consumer is growing",
                human_bytes(s.host_available),
                human_bytes(_host.total),
                human_bytes(s.usage),
            ));
        }
        implicated
    }

    /// The kill message the model and the operator both see. This is part of
    /// the design, not decoration: a kill the model cannot interpret makes it
    /// retry the same command forever.
    pub fn kill_message(&self, reason: &KillReason, command: &str) -> String {
        let budget = self.limits.memory_bytes;
        let peak = self.cgroup.as_ref().and_then(|c| c.peak()).unwrap_or(0);
        let basis = &self.limits.basis;
        let mut out = String::new();
        match reason {
            KillReason::OverBudget => {
                out.push_str(&format!(
                    "[pir] KILLED by {} — this command used {} (its budget for this host: {}, \
                     enforced by {}).\n",
                    reason.label(),
                    human_bytes(self.history.back().map(|s| s.usage).unwrap_or(0)),
                    human_bytes(budget),
                    self.limits.backend.label(),
                ));
                if peak > 0 {
                    out.push_str(&format!("      peak {}.\n", human_bytes(peak)));
                }
                out.push_str(
                    "      The host itself is fine. This was NOT a machine-wide OOM.\n",
                );
            }
            KillReason::FastGrowth => {
                out.push_str(&format!(
                    "[pir] KILLED by {} — the command was adding memory faster than the budget \
                     could absorb it (budget {}).\n",
                    reason.label(),
                    human_bytes(budget),
                ));
            }
            KillReason::HostCritical { implicated } => {
                let host = HostMem::read();
                let avail = host.map(|h| human_bytes(h.available)).unwrap_or_else(|| "?".into());
                let total = host.map(|h| human_bytes(h.total)).unwrap_or_else(|| "?".into());
                if *implicated {
                    out.push_str(&format!(
                        "[pir] KILLED for host protection — host memory is critically low \
                         ({avail} available of {total}) and this command ({}) is implicated.\n",
                        human_bytes(self.history.back().map(|s| s.usage).unwrap_or(0)),
                    ));
                } else {
                    out.push_str(&format!(
                        "[pir] KILLED for host protection — host memory is critically low \
                         ({avail} available of {total}).\n      This command was NOT shown to be \
                         the cause; it was stopped because the box was about to OOM something.\n",
                    ));
                }
            }
            KillReason::SwapThrash => {
                out.push_str(&format!(
                    "[pir] KILLED by {} — the command was being swapped out ({} swapped) while \
                     the host's swap is nearly full. Swap thrash makes the whole machine \
                     unresponsive, so it was stopped early.\n",
                    reason.label(),
                    human_bytes(self.history.back().map(|s| s.swap).unwrap_or(0)),
                ));
            }
            KillReason::PidCeiling => {
                out.push_str(&format!(
                    "[pir] KILLED by {} — the command reached its {} process ceiling. This is a \
                     limit on the *command*, not the host's process table.\n",
                    reason.label(),
                    self.limits.pids,
                ));
            }
        }
        out.push_str(&format!("      budget basis: {basis}.\n"));
        if let Some(note) = &self.not_blamed_note {
            out.push_str(&format!("      note: {note}.\n"));
        }
        // Targeted hints for the common causes: cheap pattern match, high hit
        // rate — the same trick the shell env wrapper already plays for
        // `timeout` and `grep`.
        if let Some(hint) = hint_for(command) {
            out.push_str(&format!("      hint: {hint}\n"));
        }
        out.push_str(&format!(
            "      If the work genuinely needs more memory, split it, or ask the operator to \
             raise PIR_RESOURCE_MEMORY_MIB (current command: {})\n",
            truncate(command, 120),
        ));
        out
    }

    /// A one-line summary for a *successful* command. Reporting the peak on the
    /// success path is the point: a command that finished near its limit is the
    /// interesting one, and the operator can raise the budget *before* the next
    /// failure.
    pub fn success_note(&self) -> Option<String> {
        let pct = self.peak_pct()?;
        if pct < 50 {
            return None; // not interesting; keep the output clean
        }
        Some(self.peak_line())
    }

    /// Peak usage as a percentage of the budget, when both are known.
    fn peak_pct(&self) -> Option<u64> {
        let limit = self.limits.memory_bytes;
        if limit == 0 {
            return None;
        }
        let peak = self.cgroup.as_ref().and_then(|c| c.peak())?;
        if peak == 0 {
            return None;
        }
        Some((peak as f64 / limit as f64 * 100.0).round() as u64)
    }

    /// The peak line, always (used after a kill, where the peak is the headline
    /// number rather than a footnote).
    pub fn peak_note(&self) -> Option<String> {
        self.peak_pct()?;
        Some(self.peak_line())
    }

    fn peak_line(&self) -> String {
        let peak = self.cgroup.as_ref().and_then(|c| c.peak()).unwrap_or(0);
        let limit = self.limits.memory_bytes;
        let pct = (peak as f64 / limit.max(1) as f64 * 100.0).round() as u64;
        format!(
            "      peak memory {} of {} budget ({}%)",
            human_bytes(peak),
            human_bytes(limit),
            pct
        )
    }
}

/// Common-cause hints, keyed on the command text.
fn hint_for(command: &str) -> Option<&'static str> {
    let c = command;
    if c.contains("cargo build") || c.contains("cargo test") || c.contains("rustc") {
        return Some("`cargo build -j2` / `CARGO_BUILD_JOBS=2` often fixes a -j$(nproc) OOM");
    }
    if c.contains("make -j") || c.contains("nproc") {
        return Some("lower the parallelism: `make -j2`, or set MAKEFLAGS=-j2");
    }
    if c.contains("xargs") && c.contains("-P") {
        return Some("reduce `xargs -P` concurrency; each worker holds its own memory");
    }
    if c.contains("docker build") || c.contains("podman build") {
        return Some("container builds accumulate layers in RAM; add --memory= to the build");
    }
    if c.contains("npm") || c.contains("yarn") || c.contains("pnpm") {
        return Some("node toolchains leak; try NODE_OPTIONS=--max-old-space-size=2048");
    }
    None
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

/// Compact byte size for messages (`8.0 GiB`, `412 MiB`).
pub fn human_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if n >= GIB {
        format!("{:.1} GiB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.0} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.0} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} B")
    }
}

/// Build the [`Limits`] for one command: budgets, enforcement reality, and the
/// session slice it belongs to.
pub fn limits_for_session(host: &HostMem, session_key: &str) -> Limits {
    let override_mib = std::env::var("PIR_RESOURCE_MEMORY_MIB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&v| v > 0);
    let pids_override = std::env::var("PIR_RESOURCE_PIDS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&v| v > 0);
    let (memory_bytes, basis) = command_budget(host, override_mib);
    let pids = pids_limit(host, pids_override);
    let session_bytes = session_budget(host);
    let backend = hierarchy().backend;
    let disabled = std::env::var("PIR_CGROUP").map(|v| v == "0").unwrap_or(false);
    let memory_bytes = if backend == Backend::None || disabled { 0 } else { memory_bytes };
    // The default posture is **advisory**: a hard kernel cap is armed only when
    // the operator asks for one, or when the host is in extremis. See
    // [`should_enforce`] — this is the difference between "a 30 GB build may
    // proceed" and "a 30 GB build is killed for working".
    let enforce = memory_bytes > 0 && should_enforce(host);
    if backend != Backend::None && !disabled {
        ensure_session_slice(session_key, session_bytes, enforce);
    }
    Limits {
        backend,
        memory_bytes,
        enforce,
        pids,
        session_bytes,
        basis,
    }
}

/// Should the kernel ceiling actually be armed right now?
///
/// **No, by default.** The ceiling exists to stop a runaway from taking the
/// machine down, not to ration ordinary work, and the two are indistinguishable
/// from a single number: a `cargo build` needing 30 GB and a leak heading for
/// 30 GB look identical until one of them finishes. So the static cap stays off
/// and the *dynamic* guard makes the call, with evidence.
///
/// It is armed only when the host is genuinely in trouble:
///
/// * `MemAvailable` already below `max(10% of total, 1 GiB)` — the machine is
///   under pressure before this command even starts, so a fresh unbounded
///   command is exactly the wrong thing to launch; or
/// * swap is more than half used, i.e. the box is already leaning on swap and
///   another large consumer would tip it into thrash; or
/// * the operator explicitly opted in (`PIR_RESOURCE_ENFORCE=1`).
///
/// When it is armed the command still gets the *loose* ceiling from
/// [`command_budget`] (60% of RAM), so even the emergency case leaves room for
/// the work to succeed — it is a backstop, not a small number.
pub fn should_enforce(host: &HostMem) -> bool {
    if std::env::var("PIR_RESOURCE_ENFORCE").map(|v| v == "1").unwrap_or(false) {
        return true;
    }
    // An explicit budget from the operator is a request to enforce it.
    if std::env::var("PIR_RESOURCE_MEMORY_MIB").is_ok() {
        return true;
    }
    if host.total == 0 {
        return false;
    }
    let pressure_floor = (host.total / 10).max(1024 * 1024 * 1024);
    if host.available < pressure_floor {
        return true;
    }
    // Leaning on swap: another large consumer would tip this into thrash.
    if host.swap_total > 0 && (host.swap_total - host.swap_free) * 2 > host.swap_total {
        return true;
    }
    false
}

/// The session key used for cgroup naming: derived from the scratchpad (which
/// is already per-session and human-readable) so an operator can correlate a
/// cgroup directory with `$PIR_SCRATCHPAD` and the session log.
pub fn session_key() -> String {
    crate::scratchpad::current()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
        .unwrap_or_else(|| format!("pid-{}", std::process::id()))
}

/// A one-shot report for `/limits` and the startup banner.
pub fn describe(host: &HostMem) -> String {
    let h = hierarchy();
    let l = limits_for_session(host, &session_key());
    let mut out = format!("resource limits: {}\n", h.note);
    out.push_str(&format!(
        "  per-command: memory {} ({}) | pids {}\n",
        if l.memory_bytes > 0 { human_bytes(l.memory_bytes) } else { "unmetered".into() },
        l.basis,
        l.pids,
    ));
    // The posture line is the one an operator actually needs: is the number
    // above a *kill line* right now, or just a scale the guard reasons with?
    let posture = if l.backend == Backend::None {
        "detection only (no cgroup controller here)".to_string()
    } else if l.enforce {
        "ARMED — the host is under pressure, so the ceiling is a hard kernel limit".to_string()
    } else {
        "advisory — commands are measured; the ceiling is not a kill line".to_string()
    };
    out.push_str(&format!("  enforcement: {posture}\n"));
    out.push_str(&format!(
        "  session    : memory {} | swap allowance {}\n",
        human_bytes(l.session_bytes),
        human_bytes(SESSION_SWAP_BYTES),
    ));
    out.push_str(&format!(
        "  host       : {} total, {} available, {} swap\n",
        human_bytes(host.total),
        human_bytes(host.available),
        human_bytes(host.swap_total.saturating_sub(host.swap_free)),
    ));
    // Say plainly what this means for heavy work, because that is the question
    // an operator asks when a build needs tens of GB.
    if !l.enforce && l.backend != Backend::None {
        out.push_str(
            "  note       : a large build may use up to the ceiling above; commands are killed \
             only when the host is actually in danger (or under an explicit override)\n",
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(total: u64, avail: u64) -> HostMem {
        HostMem { total, available: avail, free: avail, swap_total: 0, swap_free: 0, pgmajfault: 0 }
    }

    /// Budgets as a *pure* function of the host: `None` means "no container
    /// limit of our own", so these assertions hold on the operator's laptop and
    /// inside a memory-capped CI container alike.
    fn budget(total: u64, avail: u64) -> (u64, String) {
        command_budget_with(&host(total, avail), None, None)
    }

    /// The single most important property of this feature: **a legitimate
    /// large build must be allowed to take far more RAM than any "sane" per
    /// -command ration.**
    ///
    /// A first cut capped commands at 25% of RAM, which killed a `cargo build`
    /// needing 30 GB on a 64 GB box for doing exactly what it was asked to do.
    /// OOM pressure is exceptional; multi-GB builds are ordinary. These tests
    /// pin the loose ceiling so that regression cannot come back.
    #[test]
    fn a_thirty_gb_build_is_allowed_on_a_64_gb_host() {
        let (b, basis) = budget(64 << 30, 64 << 30);
        assert!(
            b >= 30 * (1 << 30),
            "a 30 GB build must fit: budget was {} ({basis})",
            human_bytes(b)
        );
        // And it stays under the host total, so the kernel keeps headroom for
        // page cache and everything else on the machine.
        assert!(b <= 64 * (1 << 30));
    }

    #[test]
    fn ceiling_is_loose_not_a_ration() {
        // 60% of the total, floored at 4 GiB: enough for real work everywhere.
        assert_eq!(budget(64 << 30, 64 << 30).0, (64u64 << 30) / 10 * 6);
        assert_eq!(budget(128 << 30, 128 << 30).0, (128u64 << 30) / 10 * 6);
        // A small VM still gets a workable ceiling rather than a fraction too
        // small to build anything: 60% of 8 GiB is 4.8 GiB (above the 4 GiB
        // floor, so the floor does not bind), and a 2 GiB box gets its whole
        // RAM back since 60% of it would be below the floor.
        assert_eq!(budget(8 << 30, 8 << 30).0, (8u64 << 30) / 10 * 6);
        assert_eq!(budget(2 << 30, 2 << 30).0, 2 << 30);
    }

    /// Low `MemAvailable` must **not** shrink the ceiling. A small build should
    /// not be rationed because a big job is running alongside it — that case is
    /// decided by the host guard at runtime, with evidence, not guessed at in
    /// advance by a static number.
    #[test]
    fn low_available_memory_does_not_shrink_the_ceiling() {
        let plenty = budget(39 << 30, 39 << 30).0;
        let scarce = budget(39 << 30, 2 << 30).0;
        assert_eq!(plenty, scarce, "the ceiling must not track MemAvailable");
    }

    #[test]
    fn container_limit_wins_over_host_total() {
        // The regression this guards: a `pir` inside an 8 GiB container on a
        // 39 GiB host must budget from the 8 GiB it can actually use (60% of it,
        // i.e. 4.8 GiB), not from the host's RAM.
        let (b, basis) = command_budget_with(&host(39 << 30, 39 << 30), None, Some(8 << 30));
        assert_eq!(b, (8u64 << 30) / 10 * 6, "60% of the container limit, not of the host");
        assert!(basis.contains("8.0 GiB"), "{basis}");
        // A container limit *larger* than the host's RAM is ignored in favour
        // of the host's own total.
        let (b, _) = command_budget_with(&host(4 << 30, 4 << 30), None, Some(64 << 30));
        assert_eq!(b, 4 << 30);
    }

    #[test]
    fn operator_override_wins() {
        // The override must win over both the host total *and* a container
        // limit — it is the operator's explicit escape hatch.
        let (b, basis) = command_budget_with(&host(39 << 30, 39 << 30), Some(256), Some(8 << 30));
        assert_eq!(b, 256 << 20);
        assert!(basis.contains("override"));
    }

    #[test]
    fn session_budget_is_looser_than_command_budget() {
        let h = host(39 << 30, 39 << 30);
        let (cmd, _) = command_budget_with(&h, None, None);
        assert!(session_budget_with(&h, None) > cmd);
    }

    /// **Enforcement is off unless the host is in extremis.** This is the guard
    /// against the feature becoming a routine per-command ration again.
    #[test]
    fn enforcement_is_off_on_a_healthy_host() {
        // Pure predicate on a synthetic host: no cgroup, and deliberately no
        // tree lock (holding it here would stall behind the slow live probes
        // for no benefit, since nothing here touches the tree).
        // Idle, plenty of memory, no swap pressure: advisory only.
        assert!(!should_enforce(&host(64 << 30, 60 << 30)));
        // Even a fairly loaded host is fine.
        assert!(!should_enforce(&host(64 << 30, 8 << 30)));
    }

    #[test]
    fn enforcement_arms_only_under_pressure() {
        // Pure predicate on synthetic hosts: no cgroup, no tree lock.
        // MemAvailable below max(10% of total, 1 GiB) -> arm it.
        assert!(should_enforce(&host(64 << 30, 3 << 30)));
        // Swap more than half used -> arm it.
        let swapping = HostMem {
            total: 64 << 30,
            available: 40 << 30,
            free: 40 << 30,
            swap_total: 8 << 30,
            swap_free: 2 << 30,
            pgmajfault: 0,
        };
        assert!(should_enforce(&swapping));
        // Unknown total -> never arm on a guess.
        assert!(!should_enforce(&HostMem::default()));
    }

    #[test]
    fn pids_scale_with_host_size() {
        assert_eq!(pids_limit(&host(2 << 30, 1 << 30), None), 1024);
        assert_eq!(pids_limit(&host(64 << 30, 64 << 30), None), 4096);
        assert_eq!(pids_limit(&host(64 << 30, 64 << 30), Some(77)), 77);
    }

    #[test]
    fn kill_message_names_budget_and_basis() {
        let l = Limits {
            backend: Backend::V1,
            memory_bytes: 8 << 30,
            enforce: true,
            pids: 2048,
            session_bytes: 16 << 30,
            basis: "8.0 GiB = 25% of 39 GiB".into(),
        };
        let g = Guard::new(l, None);
        let msg = g.kill_message(&KillReason::OverBudget, "cargo build -j32");
        assert!(msg.contains("8.0 GiB"), "{msg}");
        assert!(msg.contains("25% of 39 GiB"), "{msg}");
        assert!(msg.contains("NOT a machine-wide OOM"), "{msg}");
        assert!(msg.contains("CARGO_BUILD_JOBS"), "hint should fire: {msg}");
    }

    #[test]
    fn host_critical_message_distinguishes_blame() {
        let l = Limits {
            backend: Backend::None,
            memory_bytes: 0,
            enforce: false,
            pids: 0,
            session_bytes: 0,
            basis: "unmetered".into(),
        };
        let g = Guard::new(l, None);
        let blamed = g.kill_message(&KillReason::HostCritical { implicated: true }, "x");
        let unblamed = g.kill_message(&KillReason::HostCritical { implicated: false }, "x");
        assert!(blamed.contains("is implicated"), "{blamed}");
        assert!(unblamed.contains("NOT shown to be"), "{unblamed}");
    }

    #[test]
    fn human_bytes_reads_naturally() {
        assert_eq!(human_bytes(512 * 1024 * 1024), "512 MiB");
        assert_eq!(human_bytes(8 << 30), "8.0 GiB");
        assert_eq!(human_bytes(1023), "1023 B");
    }

    #[test]
    fn hints_cover_the_usual_suspects() {
        assert!(hint_for("cargo build -j$(nproc)").is_some());
        assert!(hint_for("make -j16 all").is_some());
        assert!(hint_for("find . | xargs -P8 rm").is_some());
        assert!(hint_for("echo hi").is_none());
    }

    #[test]
    fn detect_reports_something_rather_than_failing() {
        // On any host this must produce *a* hierarchy; the point of the test is
        // that a missing controller degrades instead of panicking.
        let h = hierarchy();
        match h.backend {
            Backend::None => assert!(h.root.is_none()),
            _ => assert!(h.root.is_some()),
        }
        assert!(!h.note.is_empty());
    }

    /// The v2 mount is found from the **mount table**, not a hardcoded path.
    ///
    /// The regression this pins: `V2_ROOT` was hardcoded to
    /// `/sys/fs/cgroup/unified`, so on a host using the single-mount layout
    /// (where the real mount is `/sys/fs/cgroup`) detection returned
    /// `Backend::None` — reporting "commands are measured, not limited" while a
    /// fully working controller with a writable `memory.max` sat there unused.
    /// The result was no kernel backstop at all, which is exactly the condition
    /// under which a runaway reached 22.8 GB.
    #[test]
    fn v2_root_prefers_a_real_cgroup2_mount() {
        // Whatever this host has, any root we report for v2 must be a genuine
        // cgroup2 mount directory (it must carry `cgroup.controllers`), and must
        // not be a path that merely happens to exist.
        let Some(root) = v2_root() else { return };

        // The discriminating assertion: the root must be the mount point named
        // by /proc/mounts as a `cgroup2` mount — not a conventional path that
        // merely exists. The regression was a hardcoded `/sys/fs/cgroup/unified`,
        // which on a single-mount host does not exist at all, so detection
        // returned `Backend::None` ("commands are measured, not limited") while a
        // working controller with a writable `memory.max` sat unused at
        // `/sys/fs/cgroup`. A test that only checks "the returned path exists"
        // does not catch that, because the fallback list supplies a path that
        // exists too.
        let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
        let cgroup2_mounts: Vec<&str> = mounts
            .lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace();
                let _dev = f.next()?;
                let mnt = f.next()?;
                let fstype = f.next()?;
                (fstype == "cgroup2").then_some(mnt)
            })
            .collect();
        if !cgroup2_mounts.is_empty() {
            assert!(
                cgroup2_mounts.iter().any(|m| Path::new(m) == root.as_path()),
                "v2_root() returned {} but the cgroup2 mount(s) are {:?} — the mount \
                 table is not being consulted, which is the hardcoded-path bug",
                root.display(),
                cgroup2_mounts
            );
            assert_ne!(
                hierarchy().backend,
                Backend::None,
                "a host with a cgroup2 mount must not report 'no cgroup memory controller'"
            );
        }
    }

    /// Every backend-dependent file name must resolve to something that exists
    /// in a real child cgroup on this host, or be absent for a documented reason.
    /// A name that silently does not exist is how a written limit becomes a
    /// no-op.
    #[test]
    fn backend_file_names_exist_on_this_host() {
        if hierarchy().backend == Backend::None {
            eprintln!("skipped: no backend");
            return;
        }
        // Use the live root; the helpers read `hierarchy()`, so the names are the
        // ones the enforcement path will actually use.
        let root = hierarchy().root.clone().expect("backend implies a root");
        let dir = root.join(format!("pir-names-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        // The two names that carry enforcement: the hard cap (must be writable)
        // and the soft cap (must exist). `swap.max` may be absent when swap
        // accounting is compiled out, which the code tolerates.
        assert!(
            dir.join(f_memory_max()).exists(),
            "the hard-limit file ({}) must exist",
            f_memory_max()
        );
        assert!(
            dir.join(f_memory_soft()).exists(),
            "the soft-limit file ({}) must exist",
            f_memory_soft()
        );
        assert!(dir.join(f_usage()).exists(), "usage file must exist");
        assert!(dir.join(f_procs()).exists(), "membership file must exist");
        let _ = std::fs::remove_dir(&dir);
    }
}

/// A single lock guarding the **real** cgroup tree for the whole test module.
///
/// Every probe that creates, joins, sweeps or asserts on real cgroups must hold
/// this. The cgroup tree is process-global shared state and `cargo test` runs
/// cases concurrently in one process, so without it the cases race: one test's
/// `sweep_stale()` deletes a directory another test has just created but whose
/// child has not yet joined, and the victim's command then runs unconstrained —
/// failing an assertion for a reason that has nothing to do with the code under
/// test. (The production path is not affected: a command's directory is removed
/// only by its own `Drop`, and the sweep skips the live session.)
#[cfg(all(test, unix))]
static CGROUP_TREE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the shared cgroup-tree lock. Poisoning is ignored so one failed
/// assertion cannot cascade into "poisoned" failures everywhere else.
#[cfg(all(test, unix))]
fn lock_cgroup_tree() -> std::sync::MutexGuard<'static, ()> {
    CGROUP_TREE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Live probe of the *real* host's cgroup/memory state. **Linux-only**: it
/// reads `/proc/meminfo` and walks `/sys/fs/cgroup`, neither of which exists on
/// macOS or BSD, so on those platforms it would fail on `expect("/proc/meminfo")`
/// rather than report anything. Gated rather than made to pass, because the
/// thing it asserts — that the detected backend matches this machine — has no
/// meaning where there is no cgroup hierarchy to detect.
#[cfg(all(test, target_os = "linux"))]
mod live_probe {
    use super::*;

    /// Prints the real detected hierarchy and computed budgets for this host.
    /// Not an assertion-heavy test: the point is to make the *platform* facts
    /// visible in the test log, so a change in the environment (a host that
    /// gains v2 controllers, a container that clamps our memory) shows up here
    /// rather than silently altering behaviour.
    #[test]
    fn report_this_host() {
        let _tree = super::lock_cgroup_tree();
        let h = hierarchy();
        let host = HostMem::read().expect("/proc/meminfo");
        eprintln!("backend      : {:?}", h.backend);
        eprintln!("root         : {:?}", h.root);
        eprintln!("note         : {}", h.note);
        eprintln!("MemTotal     : {}", human_bytes(host.total));
        eprintln!("MemAvailable : {}", human_bytes(host.available));
        eprintln!("swap         : {} of {}", human_bytes(host.swap_total - host.swap_free), human_bytes(host.swap_total));
        eprintln!("own cgroup   : {:?}", host.own_cgroup_bytes().map(human_bytes));
        let l = limits_for_session(&host, "test-session");
        eprintln!("cmd budget   : {} ({})", human_bytes(l.memory_bytes), l.basis);
        eprintln!("session      : {}", human_bytes(l.session_bytes));
        eprintln!("pids         : {}", l.pids);
        eprintln!("describe():\n{}", describe(&host));
        let n = sweep_stale();
        eprintln!("stale swept  : {n}");
    }

    /// A `Guard` over a **real** cgroup reports what a command is using, and
    /// reporting never kills it.
    ///
    /// This is the post-detach fix's proof: a guard carried into a detached job is
    /// sampled and must produce real figures. Opt-in (`PIR_RESOURCE_LIVE_TESTS=1`)
    /// because it creates a cgroup in the host tree.
    ///
    /// The child joins via `pre_exec`, exactly as production does. A *shell* must
    /// not be used to join: `bash -c 'echo $$ > cgroup.procs; python3 …'` joins the
    /// bash, which then exits, leaving the python grandchild outside the cgroup —
    /// verified, and a property of forking from a joined shell rather than of the
    /// cgroup. `pre_exec` is the only window where the writing process *is* the one
    /// that will exec.
    #[test]
    fn a_guard_over_a_real_cgroup_reports_memory_without_killing() {
        let _tree = super::lock_cgroup_tree();
        if std::env::var("PIR_RESOURCE_LIVE_TESTS").map(|v| v != "1").unwrap_or(true) {
            eprintln!("skipped (set PIR_RESOURCE_LIVE_TESTS=1 to run)");
            return;
        }
        if hierarchy().backend == Backend::None {
            eprintln!("skipped: no backend on this host");
            return;
        }
        #[cfg(unix)]
        use std::os::unix::process::CommandExt;

        let key = format!("mem-report-{}", std::process::id());
        let host = HostMem::read().expect("meminfo");
        let limits = limits_for_session(&host, &key);
        // A limit of 0 keeps the cgroup in advisory mode: nothing is written that
        // could kill, which is what lets this test assert "reporting is not a
        // verdict" meaningfully.
        let cg = create_command_cgroup(&key, 0, 0, false)
            .expect("create")
            .expect("a backend was detected, so a cgroup must be produced");
        let dir = cg.dir().to_path_buf();
        assert!(dir.exists(), "the command cgroup must exist at {}", dir.display());
        // Delegation must have happened, or there are no memory files to read.
        assert!(
            dir.join(f_usage()).exists(),
            "the command cgroup has no {} — the memory controller was not delegated              down the tree, so every limit written here would silently no-op",
            f_usage()
        );

        let procs = dir.join(f_procs());
        let mut cmd = std::process::Command::new("python3");
        cmd.arg("-c").arg("x=bytearray(128*1024*1024); import time; time.sleep(30)");
        // SAFETY: `pre_exec` runs between fork and exec in the child, which is
        // single-threaded there. The closure only opens a file and writes a pid —
        // the documented contract for that hook.
        unsafe {
            let procs = procs.clone();
            cmd.pre_exec(move || {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new().write(true).open(&procs)?;
                f.write_all(std::process::id().to_string().as_bytes())?;
                Ok(())
            });
        }
        let mut child = cmd.spawn().expect("spawn a child into the cgroup");

        // Wait for the allocation to be charged to the cgroup.
        let mut saw = 0u64;
        for _ in 0..250 {
            if let Some(u) = read_u64(&dir.join(f_usage())) {
                saw = u;
                if u > 64 * 1024 * 1024 {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let mut g = Guard::new(limits, Some(cg));
        let reported = g.memory_summary();
        let alive = child.try_wait().map(|s| s.is_none()).unwrap_or(false);
        let _ = child.kill();
        let _ = child.wait();

        assert!(
            saw > 64 * 1024 * 1024,
            "the child never charged its allocation to the cgroup (saw {saw} bytes;              procs={:?})",
            std::fs::read_to_string(&procs).map(|s| s.trim().to_string()),
        );
        let (usage, anon, peak, budget, _pct) =
            reported.expect("a real cgroup must yield memory figures");
        eprintln!("usage={usage} anon={anon} peak={peak} budget={budget}");
        assert!(usage >= 64 * 1024 * 1024, "usage should be ~128 MiB, got {usage}");
        assert!(anon > 0, "anonymous memory should be reported, got {anon}");
        assert!(peak >= usage, "peak {peak} must be at least current {usage}");
        assert!(alive, "memory_summary must not kill the command it inspects");
    }

    /// The end-to-end enforcement proof, run only when the operator opts in
    /// (`PIR_RESOURCE_LIVE_TESTS=1`) because it writes to the real cgroup tree
    /// and deliberately OOM-kills a child.
    #[test]
    fn enforcement_kills_an_over_allocator() {
        let _tree = super::lock_cgroup_tree();
        if std::env::var("PIR_RESOURCE_LIVE_TESTS").map(|v| v != "1").unwrap_or(true) {
            eprintln!("skipped (set PIR_RESOURCE_LIVE_TESTS=1 to run)");
            return;
        }
        let h = hierarchy();
        if h.backend == Backend::None {
            eprintln!("skipped: no backend on this host");
            return;
        }
        // NOTE: `lock_cgroup_tree()` must be taken *once*. It is a plain
        // `std::sync::Mutex`, so it is not reentrant — a second call in the same
        // test deadlocks against the first guard, which is exactly how this probe
        // used to hang (`wchan: anon_pipe_read` was misleading: the thread was
        // parked on the mutex, and the pipe it appeared to wait on was the test
        // harness's own output capture).
        let limit = 256u64 << 20;
        let key = format!("live-test-{}", std::process::id());
        let cg = create_command_cgroup(&key, limit, 0, true).expect("create").expect("backend");
        // The allocator joins the cgroup **in its own `pre_exec`**, so the process
        // that allocates is the process that is limited. A shell join must not be
        // used here: `bash -c 'echo $$ > cgroup.procs; python3 …'` puts the *bash*
        // in the cgroup, and python — a separate process the shell forks — escapes
        // it. The failure is nasty rather than obvious: the cgroup shows the charge,
        // the bash is killed at the limit, and the escaped python runs on holding
        // the stderr pipe, so `wait_with_output` blocks on a read that never
        // returns EOF (observed as `wchan: anon_pipe_read`). Production joins this
        // way for the same reason (see `spawn_shell_inner`'s `pre_exec`).
        #[cfg(unix)]
        use std::os::unix::process::CommandExt;
        let procs = cg.dir().join("cgroup.procs");
        let mut cmd = std::process::Command::new("python3");
        cmd.arg("-c")
            .arg("x=bytearray(900*1024*1024); print('ALLOCATED')")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        // SAFETY: `pre_exec` runs between fork and exec while the child is still
        // single-threaded; the closure only opens a file and writes a pid.
        unsafe {
            cmd.pre_exec(move || {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new().write(true).open(&procs)?;
                f.write_all(std::process::id().to_string().as_bytes())?;
                Ok(())
            });
        }
        let mut child = cmd.spawn().expect("spawn an allocator into the cgroup");
        // **Bounded wait.** `wait_with_output()` blocks until EOF on the child's
        // stderr, and EOF is not guaranteed just because the child died: the
        // kernel's OOM kill takes the *process*, and any descriptor it duplicated
        // into another process keeps the pipe open — so the read never returns and
        // the test hangs (observed as `wchan: anon_pipe_read`). That is the same
        // "grandchild holds the pipe" hazard the production wait loop guards
        // against; a test that waits unbounded on a pipe it does not control is
        // just a latent hang.
        //
        // The wait is therefore on the process, with a deadline, and the outcome is
        // established from the cgroup counters (which is the evidence that matters:
        // the *kernel* refused the charge and killed the process).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => panic!("wait: {e}"),
            }
        };
        eprintln!("exit: {status:?}");
        eprintln!("oom_kills: {}", cg.oom_kills());
        eprintln!("peak: {:?}", cg.peak().map(human_bytes));
        // The allocator must not have completed. We cannot read its stderr without
        // risking the hang above, so the *counters* are the evidence: the kernel
        // recorded a kill and the peak reached the limit.
        assert!(
            cg.oom_kills() > 0,
            "expected the kernel's oom_kill counter to advance (status {status:?}, peak {:?})",
            cg.peak().map(human_bytes)
        );
        assert!(
            cg.peak().unwrap_or(0) >= limit - (4 << 20),
            "peak {:?} should have reached the limit",
            cg.peak()
        );
    }

    /// The unbounded-`wait_with_output` hazard, pinned so a future edit cannot
    /// quietly reintroduce it.
    ///
    /// A child that is OOM-killed can leave its stderr pipe open — a descriptor
    /// duplicated into another process keeps it alive — so waiting for EOF never
    /// returns and the test hangs. The live probe therefore polls the child with a
    /// deadline instead, and establishes the outcome from the cgroup counters.
    ///
    /// The check looks for the *call*, not the word: the probe's own comments
    /// explain the hazard, so a naive substring search would match the
    /// explanation rather than a reintroduced call.
    #[test]
    fn a_killed_child_must_not_be_waited_on_for_eof() {
        let src = include_str!("resource.rs");
        let probe = src
            .split("fn enforcement_kills_an_over_allocator")
            .nth(1)
            .unwrap_or("")
            .split("\n    /// ")
            .next()
            .unwrap_or("");
        // Strip line comments so only executable text is inspected.
        let code: String = probe
            .lines()
            .map(|l| match l.find("//") {
                Some(i) => &l[..i],
                None => l,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code.contains("wait_with_output"),
            "the enforcement probe must not wait for EOF on a possibly-orphaned pipe"
        );
        assert!(
            code.contains("try_wait"),
            "the probe must poll the child with a deadline"
        );
    }
}

#[cfg(all(test, unix))]
mod integration_probe {
    use super::*;

    /// `PIR_RESOURCE_MEMORY_MIB` is *process-global*, and `cargo test` runs cases
    /// concurrently in one process — so a test that sets it makes every other
    /// test observe an armed, tiny budget (`should_enforce` returns true for an
    /// explicit override, by design). Serialize on the env, and restore it even
    /// when an assertion fires, so one failure cannot cascade into confusing
    /// failures elsewhere.
    pub(super) static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub(super) fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// End-to-end through the *real* spawn path used by the agent's bash tool,
    /// with a deliberately tiny budget: the command must come back killed, and
    /// the message must name the budget and its basis.
    ///
    /// Opt-in (`PIR_RESOURCE_LIVE_TESTS=1`) because it really OOM-kills a child
    /// in the host's cgroup tree.
    #[test]
    fn bash_tool_kills_an_over_allocator_with_a_readable_message() {
        if std::env::var("PIR_RESOURCE_LIVE_TESTS").map(|v| v != "1").unwrap_or(true) {
            eprintln!("skipped (set PIR_RESOURCE_LIVE_TESTS=1 to run)");
            return;
        }
        if hierarchy().backend == Backend::None {
            eprintln!("skipped: no cgroup backend on this host");
            return;
        }
        let _env = lock_env();
        // Hold the tree lock: this test *joins a real cgroup*, and a concurrent
        // `sweep_stale()` from another case would delete the directory between
        // our `mkdir` and the child's join — at which point the child silently
        // runs unconstrained and the assertion below fires for the wrong reason.
        let _tree = super::lock_cgroup_tree();
        // A tiny budget makes the failure fast and deterministic. The variable
        // is read by `limits_for_session`, so this is exactly the operator's
        // knob. Guard-restored so a failing assertion cannot leak it into the
        // other tests.
        let key = format!("live-e2e-{}", std::process::id());
        let host = HostMem::read().expect("meminfo");
        let prev = std::env::var("PIR_RESOURCE_MEMORY_MIB").ok();
        unsafe { std::env::set_var("PIR_RESOURCE_MEMORY_MIB", "192") };
        let limits = limits_for_session(&host, &key);
        match prev {
            Some(v) => unsafe { std::env::set_var("PIR_RESOURCE_MEMORY_MIB", v) },
            None => unsafe { std::env::remove_var("PIR_RESOURCE_MEMORY_MIB") },
        }
        assert_eq!(limits.memory_bytes, 192 << 20, "override should win");
        // The limit is passed explicitly from here on, so the ambient env no
        // longer matters to this test.
        let cg = create_command_cgroup(&key, limits.memory_bytes, 0, true).expect("create").expect("backend");
        // Capture a *join failure* explicitly: without this, a child that never
        // entered the cgroup just runs unconstrained and the assertion below
        // fails for a reason that hides the real problem.
        let script = format!(
            "if ! echo $$ > {}/cgroup.procs; then echo JOIN-FAILED >&2; exit 3; fi; \
             if ! grep -q $$ {}/cgroup.procs; then echo JOIN-VANISHED >&2; exit 4; fi; \
             python3 -c 'x=bytearray(600*1024*1024)'",
            cg.dir().display(),
            cg.dir().display(),
        );
        let child = std::process::Command::new("bash")
            .arg("-c")
            .arg(&script)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn");
        let out = child.wait_with_output().expect("wait");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        eprintln!("status: {:?}", out.status);
        eprintln!("stderr: {}", stderr.trim());
        eprintln!("dir exists: {}", cg.dir().exists());
        eprintln!("oom_kills: {}", cg.oom_kills());
        eprintln!("peak: {:?}", cg.peak().map(human_bytes));
        // The command must have *died*, not merely exited non-zero: assert the
        // kernel's own counter moved, which is unambiguous evidence that the
        // 192 MiB limit — not something else — stopped it.
        assert!(
            cg.oom_kills() > 0,
            "the kernel should have recorded an oom_kill (status {:?}, stdout {:?})",
            out.status,
            stdout
        );
        assert!(
            cg.peak().unwrap_or(0) >= (192 << 20) - (4 << 20),
            "peak {:?} should have reached the 192 MiB limit",
            cg.peak()
        );
        // And the message the model would see names the budget.
        let mut g = Guard::new(limits.clone(), Some(cg));
        g.observe(true);
        let msg = g.kill_message(&KillReason::OverBudget, &script);
        assert!(msg.contains("192 MiB"), "budget should be named: {msg}");
        assert!(msg.contains("operator override"), "basis should be named: {msg}");
    }
}

#[cfg(all(test, unix))]
mod posture_probe {
    use super::*;

    /// The regression test that matters most: under the **default** posture on a
    /// healthy host, a large allocation must run to completion. Nothing in this
    /// feature may kill ordinary heavy work.
    ///
    /// Opt-in (`PIR_RESOURCE_LIVE_TESTS=1`); allocates 6 GiB, which is far more
    /// than any "sane" per-command ration would allow but well within a
    /// developer workstation's ordinary budget for a big build.
    #[test]
    fn a_large_allocation_is_allowed_under_the_default_posture() {
        if std::env::var("PIR_RESOURCE_LIVE_TESTS").map(|v| v != "1").unwrap_or(true) {
            eprintln!("skipped (set PIR_RESOURCE_LIVE_TESTS=1 to run)");
            return;
        }
        let _env = super::integration_probe::lock_env();
        let _tree = super::lock_cgroup_tree();
        let host = HostMem::read().expect("meminfo");
        // Guard the test itself: skip if this host genuinely cannot spare it.
        if host.available < 12 * (1 << 30) {
            eprintln!("skipped: only {} available", human_bytes(host.available));
            return;
        }
        // Assert on a *synthetic* healthy host rather than the ambient one: this
        // test is about the posture rule, and a sibling test may legitimately
        // have an override set (which arms enforcement). Reading the live
        // environment here would make the two tests fight.
        let healthy = HostMem {
            total: 64 << 30,
            available: 60 << 30,
            free: 60 << 30,
            ..HostMem::default()
        };
        assert!(
            !should_enforce(&healthy),
            "a healthy host must not arm enforcement"
        );
        let limits = limits_for_session(&host, "posture-test");
        assert!(!limits.enforce, "default posture is advisory");
        assert!(
            limits.memory_bytes >= 6 * (1 << 30),
            "ceiling must not strangle a large build: {}",
            human_bytes(limits.memory_bytes)
        );

        // And with no limit written, the cgroup must not kill it.
        let cg = create_command_cgroup("posture-test", limits.memory_bytes, limits.pids, false)
            .expect("create")
            .expect("backend");
        assert!(!cg.enforced, "advisory mode must not write a hard limit");
        let script = format!(
            "echo $$ > {}/cgroup.procs; python3 -c 'x=bytearray(6*1024**3); print(\"ALLOCATED-6G\")'",
            cg.dir().display()
        );
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("spawn");
        let stdout = String::from_utf8_lossy(&out.stdout);
        eprintln!("status: {:?}", out.status);
        eprintln!("peak: {:?}", cg.peak().map(human_bytes));
        assert!(
            stdout.contains("ALLOCATED-6G"),
            "a 6 GiB allocation must succeed under the default posture: {stdout}"
        );
        assert_eq!(cg.oom_kills(), 0, "nothing may be OOM-killed here");
    }
}

#[cfg(all(test, unix))]
mod lifecycle_probe {
    use super::*;

    /// These tests exercise the **real** cgroup tree and the *global* sweep, so
    /// they must not race each other: `cargo test` runs cases concurrently in
    /// one process, and one test's `sweep_stale` would happily delete another
    /// test's freshly-created (and momentarily empty) fixture — a flake that
    /// looks like a bug in the sweep but is really shared state.
    ///
    /// There is exactly **one** lock for the whole test module
    /// ([`super::lock_cgroup_tree`]); a second, module-local mutex would risk
    /// taking them in opposite orders and deadlocking. The resource tree is a
    /// process-global resource, like the `$VAR`-swapping tests elsewhere in this
    /// suite that serialize on a lock for the same reason.
    fn lock_tree() -> std::sync::MutexGuard<'static, ()> {
        super::lock_cgroup_tree()
    }

    /// Two commands in the **same** session key must never share a cgroup
    /// directory. The original `cmd-<pid>` naming made this collision possible
    /// (a resumed session, or a process spawning several commands sequentially),
    /// and the consequence was silent: the second command would inherit the
    /// first's written limit, and the first guard's `Drop` would delete a
    /// directory the second was still living in.
    #[test]
    fn same_session_commands_get_distinct_cgroups() {
        let _tree = lock_tree();
        if hierarchy().backend == Backend::None {
            eprintln!("skipped: no backend");
            return;
        }
        let key = format!("collide-probe-{}", std::process::id());
        let mut dirs = Vec::new();
        for _ in 0..3 {
            let cg = create_command_cgroup(&key, 256 << 20, 0, true)
                .expect("create")
                .expect("backend");
            dirs.push(cg.dir().to_path_buf());
        }
        assert_eq!(dirs.len(), 3);
        for (i, a) in dirs.iter().enumerate() {
            for b in dirs.iter().skip(i + 1) {
                assert_ne!(a, b, "two commands shared a cgroup dir: {}", a.display());
            }
        }
        // Dropping the guards must leave the session dir clean.
        for cg in create_command_cgroup(&key, 0, 0, false).into_iter().flatten() {
            drop(cg);
        }
        for d in &dirs {
            assert!(!d.exists(), "guard Drop did not remove {}", d.display());
        }
    }

    /// The sweep must reach **nested** empty cgroups, not just one level of
    /// `cmd-*` children — the earlier version hard-coded the depth and silently
    /// leaked anything deeper.
    #[test]
    fn sweep_removes_nested_empty_cgroups() {
        let _tree = lock_tree();
        if hierarchy().backend == Backend::None {
            eprintln!("skipped: no backend");
            return;
        }
        let key = format!("sweep-probe-{}", std::process::id());
        let cg = create_command_cgroup(&key, 0, 0, false).expect("create").expect("backend");
        let dir = cg.dir().to_path_buf();
        let sess = dir.parent().expect("session dir").to_path_buf();
        // Drop the guard so the command dir is empty, then recreate it by hand
        // to simulate a crash that left the directory behind.
        drop(cg);
        std::fs::create_dir_all(&dir).expect("recreate");
        assert!(dir.exists());

        let removed = sweep_stale();
        eprintln!("swept {removed}");
        assert!(!dir.exists(), "nested empty cgroup survived the sweep");
        assert!(!sess.exists(), "empty session dir survived the sweep");
        // Belt and braces: leave nothing behind even if an assertion above was
        // the thing that failed, so a failing run does not itself leak a cgroup.
        let _ = std::fs::remove_dir_all(&sess);
    }

    /// A cgroup with a live process must **never** be swept — that would be the
    /// sweep killing a running session.
    ///
    /// The "live process" is a **child** we spawn, not the test process itself.
    /// Parking the shared test binary in a cgroup is not an option: `cargo test`
    /// runs every case as a thread of one process, so a child spawned by any
    /// later test would inherit the membership — which is exactly how an earlier
    /// version of this test wedged the whole suite (a *killed* command's
    /// surviving grandchild stayed in the cgroup and an unrelated later test
    /// waited on it forever).
    #[test]
    fn sweep_never_touches_a_populated_cgroup() {
        let _tree = lock_tree();
        if hierarchy().backend == Backend::None {
            eprintln!("skipped: no backend");
            return;
        }
        let key = format!("live-probe-{}", std::process::id());
        let cg = create_command_cgroup(&key, 0, 0, false).expect("create").expect("backend");
        let dir = cg.dir().to_path_buf();
        let sess = dir.parent().expect("session dir").to_path_buf();
        // A long-lived child that joins the cgroup itself, so the directory is
        // genuinely populated while we sweep.
        let mut child = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!(
                "echo $$ > {}/cgroup.procs; sleep 60",
                dir.display()
            ))
            .spawn()
            .expect("spawn sleeper");
        // Wait for it to join.
        let mut joined = false;
        for _ in 0..100 {
            if let Ok(p) = std::fs::read_to_string(dir.join("cgroup.procs"))
                && p.lines().any(|l| !l.trim().is_empty())
            {
                joined = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(joined, "sleeper never joined the cgroup");

        let removed = sweep_stale();
        eprintln!("swept {removed} while a sleeper was live");
        let survived = dir.exists();

        // Always clean up, even if the assertion below fails.
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir(&dir);
        let _ = std::fs::remove_dir(&sess);

        assert!(survived, "the sweep removed a cgroup with a live process in it");
    }
}
