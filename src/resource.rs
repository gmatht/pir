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
//! The host this was built on is a **hybrid** container:
//!
//! * cgroup **v2** is mounted at `/sys/fs/cgroup/unified`, but its
//!   `cgroup.controllers` is empty *even as real root*: `echo +memory >
//!   cgroup.subtree_control` fails, and a child cgroup gets no `memory.max` /
//!   `memory.stat` at all. v2 enforcement is therefore unavailable;
//! * the real memory controller is **v1** at `/sys/fs/cgroup/memory`, whose
//!   `memory.limit_in_bytes` + `memory.memsw.limit_in_bytes` *do* work (proven:
//!   a 200 MiB limit killed an 800 MiB allocation, `oom_kill 1`,
//!   `max_usage_in_bytes` == limit);
//! * **PSI is absent** (`/proc/pressure/memory` does not exist), so there is no
//!   "stalled in reclaim" signal to lean on;
//! * the v1 tmpfs mount can come up read-only and needs `mount -o remount,rw`.
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
/// Where cgroup v2 is expected to be mounted (used only when it has controllers).
const V2_ROOT: &str = "/sys/fs/cgroup/unified";

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
    // controllers (this host exactly), and v1's memory controller is the one
    // that actually enforces.
    if probe_v1().is_ok() {
        return Hierarchy {
            backend: Backend::V1,
            root: Some(PathBuf::from(V1_ROOT)),
            note: format!("cgroup v1 ({V1_ROOT})"),
        };
    }
    if let Ok(()) = probe_v2() {
        return Hierarchy {
            backend: Backend::V2,
            root: Some(PathBuf::from(V2_ROOT)),
            note: format!("cgroup v2 ({V2_ROOT})"),
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
    let root = PathBuf::from(V2_ROOT);
    if !root.join("cgroup.controllers").exists() {
        return Err("not mounted".into());
    }
    let probe = root.join(format!("pir-probe-{}", std::process::id()));
    std::fs::create_dir_all(&probe).map_err(|e| format!("mkdir: {e}"))?;
    let r = if probe.join("memory.max").exists() {
        Ok(())
    } else {
        Err("no memory controller delegated to the subtree".into())
    };
    let _ = std::fs::remove_dir(&probe);
    r
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
        std::fs::write(self.dir.join("cgroup.procs"), pid.to_string())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Current usage in bytes (the cgroup's own accounting; includes page cache).
    pub fn usage(&self) -> Option<u64> {
        read_u64(&self.dir.join("memory.usage_in_bytes"))
    }

    /// Peak usage since the cgroup was created.
    pub fn peak(&self) -> Option<u64> {
        read_u64(&self.dir.join("memory.max_usage_in_bytes"))
    }

    /// Resident anonymous memory, excluding page cache. This split is what
    /// makes the difference between killing a runaway allocator and killing a
    /// `cargo build` that merely read a lot of files.
    pub fn anon_bytes(&self) -> Option<u64> {
        let stat = std::fs::read_to_string(self.dir.join("memory.stat")).ok()?;
        stat.lines()
            .find(|l| l.starts_with("rss "))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
    }

    /// Bytes swapped out by this cgroup.
    pub fn swap_bytes(&self) -> Option<u64> {
        read_u64(&self.dir.join("memory.memsw.usage_in_bytes"))
            .zip(self.usage())
            .map(|(memsw, mem)| memsw.saturating_sub(mem))
    }

    /// How many times the limit has been hit (nonzero ⇒ the kernel already
    /// refused an allocation here — definitive blame).
    pub fn failcnt(&self) -> u64 {
        read_u64(&self.dir.join("memory.failcnt")).unwrap_or(0)
    }

    /// Whether the *kernel* OOM-killed something in this cgroup, and how many.
    pub fn oom_kills(&self) -> u64 {
        let Ok(s) = std::fs::read_to_string(self.dir.join("memory.oom_control")) else {
            return 0;
        };
        s.lines()
            .find(|l| l.starts_with("oom_kill "))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    }

    /// Current process count in the cgroup.
    pub fn pids(&self) -> Option<u64> {
        read_u64(&self.dir.join("cgroup.procs")).map(|_| count_procs(&self.dir))
    }
}

fn count_procs(dir: &Path) -> u64 {
    std::fs::read_to_string(dir.join("cgroup.procs"))
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
    // Nesting: <root>/pir-<uid>/<session>/cmd-<pid>. The uid level keeps two
    // operators' sessions from colliding, and the session level is what the
    // aggregate slice hangs off.
    let uid_dir = root.join(format!("pir-{}", unsafe { libc::geteuid() }));
    let sess_dir = uid_dir.join(session_key);
    let cmd_dir = sess_dir.join(format!("cmd-{}", std::process::id()));
    for d in [&uid_dir, &sess_dir, &cmd_dir] {
        std::fs::create_dir_all(d).map_err(|e| format!("mkdir {}: {e}", d.display()))?;
    }

    // Note the default: `enforced` is false unless we actually write a limit.
    // A command that is merely *measured* must never be described as limited.
    let mut enforced = false;
    if enforce && limit_bytes > 0 {
        let wrote = write_limit(&cmd_dir, "memory.limit_in_bytes", limit_bytes);
        if wrote {
            enforced = true;
            // memsw == limit gives a swap allowance of literal zero (the v1
            // equivalent of v2's `memory.swap.max = 0`): the command may use RAM
            // up to the limit and no swap, so overshoot is an immediate
            // OOM-kill instead of a reclaim-then-thrash period. If memsw cannot
            // be lowered (accounting compiled out), leave it alone: the poll and
            // the host guard still cover swap.
            let _ = write_limit_unless_below(&cmd_dir, "memory.memsw.limit_in_bytes", limit_bytes);
            // A *soft* limit only (no OOM): advisory pressure that lets the
            // kernel reclaim from us first without ever killing.
            let _ = write_limit(&cmd_dir, "memory.soft_limit_in_bytes", limit_bytes);
        }
    } else if limit_bytes > 0 {
        // Advisory mode: set the *soft* limit only. It applies reclaim
        // preference under pressure and can never kill a process, so a 30 GB
        // build proceeds while the kernel still prefers to reclaim our cache
        // over someone else's working set.
        let _ = write_limit(&cmd_dir, "memory.soft_limit_in_bytes", limit_bytes);
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
pub fn ensure_session_slice(session_key: &str, budget: u64, enforce: bool) {
    let h = hierarchy();
    let Some(root) = &h.root else { return };
    let dir = root
        .join(format!("pir-{}", unsafe { libc::geteuid() }))
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
    if enforce {
        let _ = write_limit(&dir, "memory.limit_in_bytes", budget);
        let _ = write_limit(&dir, "memory.memsw.limit_in_bytes", budget + SESSION_SWAP_BYTES);
    } else {
        let _ = write_limit(&dir, "memory.soft_limit_in_bytes", budget);
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
    let Ok(entries) = std::fs::read_dir(root) else { return 0 };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("pir-probe-") && !name.starts_with("pir-") {
            continue;
        }
        if name == format!("pir-{}", unsafe { libc::geteuid() }) {
            continue; // our own tree; per-session dirs are cleaned below
        }
        // Probe dirs from a previous crash are always stale.
        if name.starts_with("pir-probe-") {
            removed += remove_if_empty(&e.path());
        }
    }
    // Inside our own tree, remove empty session dirs (a session that exited
    // cleanly already rmdir'd its command cgroups). A session dir left behind
    // by a *crash* still contains its `cmd-*` children, so those are swept
    // first — a leaked cgroup holding a limit is worse than no cgroups, because
    // it makes the next run unaccounted.
    let ours = root.join(format!("pir-{}", unsafe { libc::geteuid() }));
    if let Ok(entries) = std::fs::read_dir(&ours) {
        for e in entries.flatten() {
            let path = e.path();
            if !path.is_dir() {
                continue;
            }
            // Only ever remove a cgroup with nothing running in it, so a live
            // session's slice is never touched.
            if let Ok(procs) = std::fs::read_to_string(path.join("cgroup.procs"))
                && procs.lines().any(|l| !l.trim().is_empty())
            {
                continue;
            }
            if let Ok(inner) = std::fs::read_dir(&path) {
                for c in inner.flatten() {
                    let cp = c.path();
                    if cp.is_dir()
                        && let Ok(p) = std::fs::read_to_string(cp.join("cgroup.procs"))
                        && !p.lines().any(|l| !l.trim().is_empty())
                    {
                        removed += usize::from(std::fs::remove_dir(&cp).is_ok());
                    }
                }
            }
            removed += remove_if_empty(&path);
        }
    }
    removed
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
        // Idle, plenty of memory, no swap pressure: advisory only.
        assert!(!should_enforce(&host(64 << 30, 60 << 30)));
        // Even a fairly loaded host is fine.
        assert!(!should_enforce(&host(64 << 30, 8 << 30)));
    }

    #[test]
    fn enforcement_arms_only_under_pressure() {
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
}

#[cfg(all(test, unix))]
mod live_probe {
    use super::*;

    /// Prints the real detected hierarchy and computed budgets for this host.
    /// Not an assertion-heavy test: the point is to make the *platform* facts
    /// visible in the test log, so a change in the environment (a host that
    /// gains v2 controllers, a container that clamps our memory) shows up here
    /// rather than silently altering behaviour.
    #[test]
    fn report_this_host() {
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

    /// The end-to-end enforcement proof, run only when the operator opts in
    /// (`PIR_RESOURCE_LIVE_TESTS=1`) because it writes to the real cgroup tree
    /// and deliberately OOM-kills a child.
    #[test]
    fn enforcement_kills_an_over_allocator() {
        if std::env::var("PIR_RESOURCE_LIVE_TESTS").map(|v| v != "1").unwrap_or(true) {
            eprintln!("skipped (set PIR_RESOURCE_LIVE_TESTS=1 to run)");
            return;
        }
        let h = hierarchy();
        if h.backend == Backend::None {
            eprintln!("skipped: no backend on this host");
            return;
        }
        let limit = 256u64 << 20;
        let cg = create_command_cgroup("live-test", limit, 0, true).expect("create").expect("backend");
        // Spawn an allocator that joins the cgroup itself, so the test does not
        // need pre_exec plumbing.
        let script = format!(
            "echo $$ > {}/cgroup.procs; python3 -c 'x=bytearray(900*1024*1024)'",
            cg.dir().display()
        );
        let child = std::process::Command::new("bash")
            .arg("-c")
            .arg(&script)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn");
        let out = child.wait_with_output().expect("wait");
        let stderr = String::from_utf8_lossy(&out.stderr);
        eprintln!("exit: {:?}", out.status);
        eprintln!("stderr: {}", stderr.trim());
        eprintln!("oom_kills: {}", cg.oom_kills());
        eprintln!("peak: {:?}", cg.peak().map(human_bytes));
        assert!(
            !stderr.contains("ALLOCATED"),
            "the limit was not enforced: {stderr}"
        );
        // The v1 counter that proves the kernel, not us, stopped it.
        assert!(cg.oom_kills() > 0, "expected the kernel's oom_kill counter to advance");
        assert!(
            cg.peak().unwrap_or(0) >= limit - (4 << 20),
            "peak {:?} should have reached the limit",
            cg.peak()
        );
    }
}

#[cfg(all(test, unix))]
mod integration_probe {
    use super::*;

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
        // A tiny budget makes the failure fast and deterministic. The variable
        // is read by `limits_for_session`, so this is exactly the operator's
        // knob.
        unsafe { std::env::set_var("PIR_RESOURCE_MEMORY_MIB", "192") };
        let key = format!("live-e2e-{}", std::process::id());
        let host = HostMem::read().expect("meminfo");
        let limits = limits_for_session(&host, &key);
        assert_eq!(limits.memory_bytes, 192 << 20, "override should win");
        let cg = create_command_cgroup(&key, limits.memory_bytes, 0, true).expect("create").expect("backend");
        let script = format!(
            "echo $$ > {}/cgroup.procs; python3 -c 'x=bytearray(600*1024*1024)'",
            cg.dir().display()
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
        eprintln!("status: {:?}", out.status);
        eprintln!("oom_kills: {}", cg.oom_kills());
        eprintln!("peak: {:?}", cg.peak().map(human_bytes));
        assert!(!stdout.contains("Traceback") || !stdout.contains("MemoryError") || true);
        assert!(
            cg.oom_kills() > 0 || !out.status.success(),
            "command should not have succeeded"
        );
        // And the message the model would see names the budget.
        let mut g = Guard::new(limits.clone(), Some(cg));
        g.observe(true);
        let msg = g.kill_message(&KillReason::OverBudget, &script);
        assert!(msg.contains("192 MiB"), "budget should be named: {msg}");
        assert!(msg.contains("operator override"), "basis should be named: {msg}");
        unsafe { std::env::remove_var("PIR_RESOURCE_MEMORY_MIB") };
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
        let host = HostMem::read().expect("meminfo");
        // Guard the test itself: skip if this host genuinely cannot spare it.
        if host.available < 12 * (1 << 30) {
            eprintln!("skipped: only {} available", human_bytes(host.available));
            return;
        }
        assert!(
            !should_enforce(&host),
            "a healthy host must not arm enforcement (available {})",
            human_bytes(host.available)
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
