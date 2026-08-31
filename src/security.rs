//! Security posture for `pir`: a configurable, OS-abstracted guardrail layer.
//!
//! Reads are allowed by default (configurable); only credential/secret reads
//! are restricted, and only when the operator opts into `GuardedSecrets` read
//! mode. Writes to a short list of high-value targets are denied/redirected.
//! Every blocked operation can be turned into a request the user answers — so
//! security is "at worst naggy, never broken."
//!
//! The cross-platform bits (verdicts, parcels, the ask channel, config parsing,
//! the guardrail decision) live here in plain Rust; the genuinely
//! platform-specific bits (how a write is intercepted, how a request is
//! surfaced to the operator) sit behind the `Platform` trait. A `unix` backend
//! is implemented; a `windows` backend is structurally present (stubbed) so the
//! abstraction is real.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// ===========================================================================
// Core vocabulary
// ===========================================================================

/// A class of operation the confinement layer can intercept.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Op {
    Read,
    Write,
    Exec,
    Connect,
    Bind,
    Apt,
    BecomeRoot,
    Custom(String),
}

impl Op {
    pub fn verb(&self) -> String {
        match self {
            Op::Read => "read".into(),
            Op::Write => "write".into(),
            Op::Exec => "exec".into(),
            Op::Connect => "connect".into(),
            Op::Bind => "bind".into(),
            Op::Apt => "apt".into(),
            Op::BecomeRoot => "become-root".into(),
            Op::Custom(c) => c.clone(),
        }
    }
}

/// The outcome of checking an operation against the active policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny { parcel: Parcel, risk: Risk },
}

/// Coarse risk rating surfaced to the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    Low,
    Medium,
    High,
}

impl Risk {
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::Low => "low",
            Risk::Medium => "med",
            Risk::High => "high",
        }
    }
}

/// A pre-vetted "right bundle" the operator can grant. `Custom` is the escape
/// hatch: anything unanticipated is still requestable and approvable.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Parcel {
    ToolchainRo,
    DocsRo,
    VendorRo,
    SecretRead { path: PathBuf },
    ScratchRw,
    ConfigStaging,
    CacheRw,
    NetNone,
    NetWebSearch,
    NetPackages,
    NetGithub,
    NetDevTunnel,
    NoCaps,
    CapNetBind,
    AptInstall { packages: Vec<String> },
    GuardDb,
    GuardSecrets,
    GuardOtherUsers,
    GuardSystem,
    GuardTestOracle,
    /// A write targeting the repository's `.git` metadata (history, refs,
    /// hooks, config). The agent must never mutate the real repo directly; it
    /// works on a branch and submits a pull request instead. The blast radius
    /// message tells the agent exactly how.
    GuardRepoGit,
    Custom(String),
}

impl Parcel {
    pub fn id(&self) -> String {
        match self {
            Parcel::ToolchainRo => "toolchain-ro".into(),
            Parcel::DocsRo => "docs-ro".into(),
            Parcel::VendorRo => "vendor-ro".into(),
            Parcel::SecretRead { path } => format!("secret-read:{}", path.display()),
            Parcel::ScratchRw => "scratch-rw".into(),
            Parcel::ConfigStaging => "config-staging".into(),
            Parcel::CacheRw => "cache-rw".into(),
            Parcel::NetNone => "net-none".into(),
            Parcel::NetWebSearch => "net-web-search".into(),
            Parcel::NetPackages => "net-packages".into(),
            Parcel::NetGithub => "net-github".into(),
            Parcel::NetDevTunnel => "net-dev-tunnel".into(),
            Parcel::NoCaps => "no-caps".into(),
            Parcel::CapNetBind => "cap-net-bind".into(),
            Parcel::AptInstall { packages } => format!("apt-install:{}", packages.join(",")),
            Parcel::GuardDb => "guard-db".into(),
            Parcel::GuardSecrets => "guard-secrets".into(),
            Parcel::GuardOtherUsers => "guard-other-users".into(),
            Parcel::GuardSystem => "guard-system".into(),
            Parcel::GuardTestOracle => "guard-test-oracle".into(),
            Parcel::GuardRepoGit => "guard-repo-git".into(),
            Parcel::Custom(s) => format!("custom:{s}"),
        }
    }

    pub fn blast_radius(&self) -> &'static str {
        match self {
            Parcel::ToolchainRo => "read+exec of compiler/SDK/include dirs",
            Parcel::DocsRo => "read of shared reference docs",
            Parcel::VendorRo => "read of vendored deps / mirrors",
            Parcel::SecretRead { .. } => "read of ONE credential file only",
            Parcel::ScratchRw => "read+write to the per-session scratch dir",
            Parcel::ConfigStaging => "writes land in overlay; reviewed before apply",
            Parcel::CacheRw => "writes to a package cache",
            Parcel::NetNone => "no network egress",
            Parcel::NetWebSearch => "https/443 to one search API host",
            Parcel::NetPackages => "https/443 to package registries",
            Parcel::NetGithub => "https/443 to github.com",
            Parcel::NetDevTunnel => "inbound+outbound on one chosen port",
            Parcel::NoCaps => "no Linux capabilities",
            Parcel::CapNetBind => "CAP_NET_BIND_SERVICE (bind low ports)",
            Parcel::AptInstall { .. } => "host package install (human-reviewed)",
            Parcel::GuardDb => "corrupt/truncate production DB",
            Parcel::GuardSecrets => "read/overwrite credentials",
            Parcel::GuardOtherUsers => "touch another user's / ai group tree",
            Parcel::GuardSystem => "mutate boot/system config",
            Parcel::GuardTestOracle => "overwrite its own test oracle",
            Parcel::GuardRepoGit => {
                "rewrite repo history/refs/hooks directly — instead commit on a branch and submit a pull request (e.g. `git push -u origin <branch>` then `gh pr create`, or `pir submit`)"
            }
            Parcel::Custom(_) => "a right we didn't anticipate — review carefully",
        }
    }

    pub fn default_risk(&self) -> Risk {
        match self {
            Parcel::ToolchainRo
            | Parcel::DocsRo
            | Parcel::VendorRo
            | Parcel::ScratchRw
            | Parcel::CacheRw
            | Parcel::NetNone
            | Parcel::NoCaps => Risk::Low,
            Parcel::SecretRead { .. }
            | Parcel::ConfigStaging
            | Parcel::NetWebSearch
            | Parcel::NetPackages
            | Parcel::NetGithub
            | Parcel::NetDevTunnel
            | Parcel::CapNetBind => Risk::Medium,
            Parcel::AptInstall { .. }
            | Parcel::GuardDb
            | Parcel::GuardSecrets
            | Parcel::GuardOtherUsers
            | Parcel::GuardSystem
            | Parcel::GuardTestOracle
            | Parcel::GuardRepoGit => Risk::High,
            Parcel::Custom(_) => Risk::High,
        }
    }
}

// ===========================================================================
// Policy / posture
// ===========================================================================

/// The overall confinement level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SecurityLevel {
    Sandbox,
    Strict,
    Worktree,
    #[default]
    Guard,
    Off,
}

impl SecurityLevel {
    pub fn parse(s: &str) -> Option<SecurityLevel> {
        match s.trim().to_ascii_lowercase().as_str() {
            "sandbox" => Some(SecurityLevel::Sandbox),
            "strict" => Some(SecurityLevel::Strict),
            "worktree" => Some(SecurityLevel::Worktree),
            "guard" => Some(SecurityLevel::Guard),
            "off" | "none" | "disabled" => Some(SecurityLevel::Off),
            _ => None,
        }
    }
    pub fn guards_writes(self) -> bool {
        matches!(
            self,
            SecurityLevel::Guard | SecurityLevel::Sandbox | SecurityLevel::Strict | SecurityLevel::Worktree
        )
    }
    pub fn as_str(self) -> &'static str {
        match self {
            SecurityLevel::Sandbox => "sandbox",
            SecurityLevel::Strict => "strict",
            SecurityLevel::Worktree => "worktree",
            SecurityLevel::Guard => "guard",
            SecurityLevel::Off => "off",
        }
    }
    /// True when the agent is repo-isolated: it owns a git worktree + branch
    /// and can only submit PRs (never write trunk directly). In this mode the
    /// existing `wt` extension is driven into PR-submission mode.
    pub fn is_worktree(self) -> bool {
        matches!(self, SecurityLevel::Worktree)
    }
}

/// Default posture for read operations. Reads are allowed by default
/// (reads default-open, configurable); the only read the guardrail ever blocks
/// is a credential/secret path, and only when the operator opts into
/// `GuardedSecrets`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReadMode {
    #[default]
    Open,
    GuardedSecrets,
}

impl ReadMode {
    pub fn parse(s: &str) -> Option<ReadMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "open" | "on" | "allow" | "yes" => Some(ReadMode::Open),
            "guarded" | "guarded-secrets" | "secrets" => Some(ReadMode::GuardedSecrets),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            ReadMode::Open => "open",
            ReadMode::GuardedSecrets => "guarded-secrets",
        }
    }
}

/// How unanticipated requests behave.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AskMode {
    #[default]
    Ask,
    AutoYes,
    AutoNo,
}

impl AskMode {
    pub fn parse(s: &str) -> Option<AskMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ask" | "prompt" | "nag" => Some(AskMode::Ask),
            "auto" | "auto-yes" | "yes" => Some(AskMode::AutoYes),
            "auto-no" | "no" | "deny" => Some(AskMode::AutoNo),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            AskMode::Ask => "ask",
            AskMode::AutoYes => "auto-yes",
            AskMode::AutoNo => "auto-no",
        }
    }
}

/// The effective security policy for this session.
#[derive(Debug, Clone)]
pub struct SecurityPolicy {
    pub level: SecurityLevel,
    pub apt: AptMode,
    pub network: NetworkMode,
    pub ask: AskMode,
    /// Read posture: reads allowed by default; `GuardedSecrets` makes
    /// credential/secret reads require an `ask`.
    pub read: ReadMode,
    pub extra_guard: Vec<String>,
    /// Overlayfs write-quarantine: when `pir` can mount (root), the agent still
    /// *all* commands, but writes to the configured system trees are
    /// redirected into an overlay `upperdir` so the real fs is untouched until
    /// the operator reviews + applies them (the `/quarantine` command). This is
    /// the default-on safe posture: run everything, stage the writes. Set
    /// `quarantine = false` to disable (falls back to the in-process guardrail).
    pub quarantine: bool,
    /// Project-scoped write-quarantine: when a worktree is whitelisted (worktree
    /// mode), overlay the *repo root* with a staging upper and bind-mount the
    /// agent's own worktree read-write on top, so only the worktree is written
    /// to the real fs — every other write (central `.git`, trunk, other
    /// worktrees) is quarantined and visible only to the agent. On by default;
    /// the `wt` extension engages it the moment a worktree is created. Set
    /// `quarantine-project = false` (or `PIR_QUARANTINE=0`) to disable.
    pub quarantine_project: bool,
    /// Directories to overlay (stage) when quarantine is active. Empty => the
    /// module default (`/etc`, `/usr/local`, `/opt`, `/srv`, `/var`, `/boot`).
    pub quarantine_dirs: Vec<String>,
    /// Where the staging upper/work layers live (must not be inside a staged dir).
    pub quarantine_staging: PathBuf,
    /// Paths that must never be staged even under quarantine — they're hard-
    /// denied by the in-process guardrail instead (critical DBs, secret stores).
    pub quarantine_critical: Vec<String>,
    /// Worktree-mode idle policy (only meaningful when `level == Worktree`).
    /// What an idle agent auto-tasks itself with when it has no user prompt:
    /// `errors` => fix snuck-in build/test failures first; `warnings` => also
    /// clear compiler warnings/lints once clean; `hygiene` => also low-risk
    /// hygiene (fmt/doc-comments); `off` => stay idle.
    pub idle: IdlePolicy,
    /// Repo-isolation: the single worktree path the agent is allowed to write
    /// to directly. When set, any write outside it — including the central
    /// `.git`, the trunk checkout, or *other* agents' worktrees — is denied (or
    /// quarantined) rather than applied to the real fs. Each agent gets its own
    /// whitelisted worktree; everything else is read-only to it. Empty => no
    /// per-agent worktree whitelist is in force (the §9.3 critical-target
    /// guardrail still applies globally).
    pub allow_worktree: Option<PathBuf>,
    /// Max open self-PRs an idle agent keeps in flight (rate-limit the swarm).
    pub idle_max_open_prs: usize,
    /// Whether a dedicated fixer agent owns failing merge requests.
    pub fixer_agent: bool,
    pub denials: Arc<Mutex<Vec<Denial>>>,
}

/// What an idle (unprompted) agent should auto-task itself with, in priority
/// order. Mirrors `docs/SECURITY_MODEL.md` §11.4: errors first, then warnings,
/// then optional hygiene; always scoped to its own worktree + branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IdlePolicy {
    /// Idle agent does nothing when there's no user prompt.
    Off,
    /// Fix snuck-in errors (red trunk / failing tests) only.
    Errors,
    /// Errors, then warnings/lints once the tree builds.
    #[default]
    Warnings,
    /// Errors, warnings, then low-risk hygiene (fmt / doc comments / trivial TODOs).
    Hygiene,
}

impl IdlePolicy {
    pub fn parse(s: &str) -> Option<IdlePolicy> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Some(IdlePolicy::Off),
            "errors" | "error" => Some(IdlePolicy::Errors),
            "warnings" | "warning" | "lints" | "lint" => Some(IdlePolicy::Warnings),
            "hygiene" | "cleanup" => Some(IdlePolicy::Hygiene),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            IdlePolicy::Off => "off",
            IdlePolicy::Errors => "errors",
            IdlePolicy::Warnings => "warnings",
            IdlePolicy::Hygiene => "hygiene",
        }
    }
    /// True when this policy allows the agent to auto-task itself with the
    /// given health tier.
    pub fn covers(self, tier: HealthTier) -> bool {
        match tier {
            HealthTier::Error => true, // errors are always the highest priority
            HealthTier::Warning => matches!(self, IdlePolicy::Warnings | IdlePolicy::Hygiene),
            HealthTier::Hygiene => matches!(self, IdlePolicy::Hygiene),
        }
    }
}

/// A code-health tier an idle agent can be assigned to fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HealthTier {
    /// Build/test failures — the worst state; always fixed first.
    Error,
    /// Compiler warnings / lints (unused, dead code, clippy, deprecations).
    Warning,
    /// Low-risk hygiene (fmt, doc comments, trivial TODOs).
    Hygiene,
}

/// Package-manager behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AptMode {
    #[default]
    Auto,
    Human,
    Stage,
    Project,
}

impl AptMode {
    pub fn parse(s: &str) -> Option<AptMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(AptMode::Auto),
            "human" => Some(AptMode::Human),
            "stage" => Some(AptMode::Stage),
            "project" => Some(AptMode::Project),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            AptMode::Auto => "auto",
            AptMode::Human => "human",
            AptMode::Stage => "stage",
            AptMode::Project => "project",
        }
    }
}

/// Network behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkMode {
    #[default]
    On,
    AllowList,
    Off,
}

impl NetworkMode {
    pub fn parse(s: &str) -> Option<NetworkMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "on" | "yes" | "open" => Some(NetworkMode::On),
            "allowlist" | "allow-list" | "allow_list" => Some(NetworkMode::AllowList),
            "off" | "none" | "blocked" => Some(NetworkMode::Off),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            NetworkMode::On => "on",
            NetworkMode::AllowList => "allowlist",
            NetworkMode::Off => "off",
        }
    }
}

impl Default for SecurityPolicy {
    fn default() -> Self {
        SecurityPolicy {
            level: SecurityLevel::Guard,
            apt: AptMode::Auto,
            network: NetworkMode::On,
            ask: AskMode::Ask,
            read: ReadMode::Open,
            extra_guard: Vec::new(),
            quarantine: true,
            quarantine_project: true,
            quarantine_dirs: Vec::new(),
            quarantine_staging: PathBuf::from(""),
            quarantine_critical: Vec::new(),
            idle: IdlePolicy::Warnings,
            idle_max_open_prs: 1,
            fixer_agent: true,
            allow_worktree: None,
            denials: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl SecurityPolicy {
    /// Classify and decide on an operation. This is the single cross-platform
    /// decision point.
    pub fn decide(&self, ask: &Ask) -> Verdict {
        if self.level == SecurityLevel::Off {
            return Verdict::Allow;
        }
        if self.ask == AskMode::AutoYes {
            return Verdict::Allow;
        }
        match &ask.op {
            // Reads allowed by default everywhere. Only a credential/secret read
            // is denied, and only in `GuardedSecrets` read mode.
            Op::Read => {
                if self.read == ReadMode::GuardedSecrets {
                    if let Some(path) = &ask.path {
                        if is_secret(path) {
                            return Verdict::Deny {
                                parcel: Parcel::GuardSecrets,
                                risk: Risk::High,
                            };
                        }
                    }
                }
                Verdict::Allow
            }
            Op::Write => {
                // When an overlayfs write-quarantine is mounted (the project
                // overlay and/or the system-tree overlay over /var, /etc, ...),
                // the overlay itself intercepts the write and stages it (visible
                // only to the agent) instead of mutating the real fs. The
                // in-process guardrail steps aside so the write reaches the
                // overlay and stages — this is exactly how writes to /var and
                // friends are *quarantined* (reviewable via /quarantine) rather
                // than hard-blocked. The in-process hard-deny (is_system_state /
                // GuardSystem) only fires as a fallback when NO overlay is mounted
                // (e.g. a non-root `ai_*` agent).
                if crate::security::overlay::project_quarantine_engaged()
                    || crate::security::overlay::system_quarantine_engaged()
                {
                    return Verdict::Allow;
                }
                if let Some(path) = &ask.path {
                    // Repo-isolation guard: in `worktree` mode (or whenever a
                    // whitelisted worktree is active) the agent may only write
                    // inside *its own* worktree. Writes to the central `.git`
                    // metadata or to other worktrees are denied — they must go
                    // through a PR, never a direct write.
                    if let Some(parcel) = self.guard_worktree_write(path) {
                        return Verdict::Deny {
                            parcel: parcel.clone(),
                            risk: parcel.default_risk(),
                        };
                    }
                    if let Some(parcel) = self.guard_write_target(path) {
                        return Verdict::Deny { parcel: parcel.clone(), risk: parcel.default_risk() };
                    }
                }
                Verdict::Allow
            }
            Op::Exec => Verdict::Allow,
            Op::Connect => match self.network {
                NetworkMode::On => Verdict::Allow,
                NetworkMode::AllowList => Verdict::Deny {
                    parcel: classify_connect(&ask.target),
                    risk: Risk::Medium,
                },
                NetworkMode::Off => Verdict::Deny {
                    parcel: Parcel::NetNone,
                    risk: Risk::Medium,
                },
            },
            Op::Bind => Verdict::Deny { parcel: Parcel::CapNetBind, risk: Risk::Medium },
            Op::Apt => match self.apt {
                AptMode::Auto => Verdict::Allow,
                _ => Verdict::Deny {
                    parcel: Parcel::AptInstall { packages: ask.packages.clone() },
                    risk: Risk::High,
                },
            },
            Op::BecomeRoot => Verdict::Deny { parcel: Parcel::Custom("host-root".into()), risk: Risk::High },
            Op::Custom(c) => Verdict::Deny { parcel: Parcel::Custom(c.clone()), risk: Risk::High },
        }
    }

    /// Repo-isolation guard: returns a `Deny` parcel when a write targets
    /// anywhere outside the agent's own whitelisted worktree. When
    /// `allow_worktree` is set, the agent may only write inside it — the
    /// central `.git`, the trunk checkout, and every *other* agent's worktree
    /// are off-limits (write-denied; the model is told to submit a PR instead).
    fn guard_worktree_write(&self, path: &Path) -> Option<Parcel> {
        let Some(wt) = &self.allow_worktree else {
            return None;
        };
        let canon = canonicalize_lenient(path);
        // Also deny writes to any `.git` metadata dir regardless of worktree.
        if is_repo_git(&canon) {
            return Some(Parcel::GuardRepoGit);
        }
        // Allow only paths that live under the whitelisted worktree.
        if under_dir(&canon, wt) {
            return None;
        }
        Some(Parcel::GuardOtherUsers)
    }

    fn guard_write_target(&self, path: &Path) -> Option<Parcel> {
        let canon = canonicalize_lenient(path);
        if is_database(&canon) {
            return Some(Parcel::GuardDb);
        }
        if is_secret(&canon) {
            return Some(Parcel::GuardSecrets);
        }
        if is_other_users(&canon) {
            return Some(Parcel::GuardOtherUsers);
        }
        if is_system_state(&canon) {
            return Some(Parcel::GuardSystem);
        }
        if is_test_oracle(&canon) {
            return Some(Parcel::GuardTestOracle);
        }
        if is_repo_git(&canon) {
            return Some(Parcel::GuardRepoGit);
        }
        if self.extra_guard.iter().any(|p| path_matches(&canon, p)) {
            return Some(Parcel::Custom("operator-guarded".into()));
        }
        None
    }

    pub fn record_denial(&self, d: Denial) {
        if let Ok(mut v) = self.denials.lock() {
            v.push(d);
        }
    }
}

// ===========================================================================
// The "ask" channel — request anything, never broken
// ===========================================================================

/// A structured description of an operation the agent attempted (or wants to
/// attempt).
#[derive(Debug, Clone)]
pub struct Ask {
    pub op: Op,
    pub path: Option<PathBuf>,
    pub target: Option<String>,
    pub packages: Vec<String>,
    pub reason: String,
    pub ttl: Option<u64>,
}

impl Ask {
    pub fn new(op: Op) -> Self {
        Ask {
            op,
            path: None,
            target: None,
            packages: Vec::new(),
            reason: String::new(),
            ttl: Some(2 * 3600),
        }
    }
    pub fn read(path: impl Into<PathBuf>) -> Self {
        let mut a = Ask::new(Op::Read);
        a.path = Some(path.into());
        a
    }
    pub fn write(path: impl Into<PathBuf>) -> Self {
        let mut a = Ask::new(Op::Write);
        a.path = Some(path.into());
        a
    }
    pub fn connect(target: impl Into<String>) -> Self {
        let mut a = Ask::new(Op::Connect);
        a.target = Some(target.into());
        a
    }
    pub fn apt(packages: Vec<String>) -> Self {
        let mut a = Ask::new(Op::Apt);
        a.packages = packages;
        a
    }
    pub fn with_reason(mut self, r: impl Into<String>) -> Self {
        self.reason = r.into();
        self
    }
    pub fn with_ttl(mut self, secs: u64) -> Self {
        self.ttl = Some(secs);
        self
    }
}

/// A captured denial.
#[derive(Debug, Clone)]
pub struct Denial {
    pub ask: Ask,
    pub parcel: Parcel,
    pub risk: Risk,
    pub ts: u64,
}

/// Where a request goes.
pub trait RequestSink: Send + Sync {
    fn surface(&self, denial: &Denial) -> Decision;
}

/// The operator's answer to a surfaced denial.
#[derive(Default, PartialEq, Eq)]
pub enum Decision {
    AllowOnce,
    AllowSession,
    #[default]
    Deny,
    Defer,
}

impl Decision {
    pub fn allows(&self) -> bool {
        matches!(self, Decision::AllowOnce | Decision::AllowSession)
    }
}

// ===========================================================================
// OS abstraction
// ===========================================================================

pub trait Platform: Send + Sync {
    fn canonicalize(&self, p: &Path) -> PathBuf {
        canonicalize_lenient(p)
    }
    fn is_other_users(&self, _p: &Path) -> bool {
        false
    }
    fn is_system_state(&self, p: &Path) -> bool {
        is_system_state(p)
    }
}

#[cfg(unix)]
pub type ActivePlatform = crate::security::unix::UnixPlatform;
#[cfg(windows)]
pub type ActivePlatform = crate::security::windows::WindowsPlatform;
#[cfg(not(any(unix, windows)))]
pub type ActivePlatform = GenericPlatform;

#[derive(Default)]
pub struct GenericPlatform;
impl Platform for GenericPlatform {}

// ===========================================================================
// Security context — the object the agent actually holds
// ===========================================================================

/// A live snapshot of the agent's recent activity, shared with the request sink
/// so an approval dialog can show *context* (the last few prompts and the
/// agent's recent thinking) — the operator sees *why* the agent is asking.
pub struct ApprovalContext {
    inner: Mutex<ApprovalContextInner>,
}

impl Default for ApprovalContext {
    fn default() -> Self {
        ApprovalContext { inner: Mutex::new(ApprovalContextInner::default()) }
    }
}

impl ApprovalContext {
    /// Record a user prompt (keeps the last `n`).
    pub fn note_prompt(&self, p: &str) {
        let mut g = self.inner.lock().unwrap();
        g.recent_prompts.push(p.to_string());
        let n = g.recent_prompts.len();
        if n > 8 {
            g.recent_prompts.drain(0..n - 8);
        }
    }
    /// Record a thinking line (keeps the last `n`).
    pub fn note_thinking(&self, t: &str) {
        let mut g = self.inner.lock().unwrap();
        g.recent_thinking.push(t.to_string());
        let n = g.recent_thinking.len();
        if n > 16 {
            g.recent_thinking.drain(0..n - 16);
        }
    }
    /// Snapshot the recent prompts + thinking for a dialog.
    pub fn snapshot(&self) -> (Vec<String>, Vec<String>) {
        let g = self.inner.lock().unwrap();
        (g.recent_prompts.clone(), g.recent_thinking.clone())
    }
}

#[derive(Default)]
struct ApprovalContextInner {
    recent_prompts: Vec<String>,
    recent_thinking: Vec<String>,
}

/// The live security context threaded through the agent.
pub struct SecurityContext {
    pub policy: SecurityPolicy,
    pub platform: Box<dyn Platform>,
    pub sink: Box<dyn RequestSink>,
    pub headless: AtomicBool,
    /// Live overlayfs-quarantine toggle (mirrors `policy.quarantine` at
    /// construction; flippable at runtime via `set_quarantine`).
    pub quarantine: AtomicBool,
    /// Shared approval context (recent prompts + thinking) for the request sink.
    pub approval: Arc<ApprovalContext>,
}

impl SecurityContext {
    pub fn new(policy: SecurityPolicy, headless: bool) -> Arc<Self> {
        let platform: Box<dyn Platform> = Box::new(ActivePlatform::default());
        let approval = Arc::new(ApprovalContext::default());
        let sink: Box<dyn RequestSink> = if headless {
            Box::new(QueuedSink::default())
        } else {
            Box::new(TtySink { approval: Some(approval.clone()) })
        };
        let q = policy.quarantine;
        Arc::new(SecurityContext {
            policy,
            platform,
            sink,
            headless: AtomicBool::new(headless),
            quarantine: AtomicBool::new(q),
            approval,
        })
    }

    /// The universal entry point. Never panics.
    pub fn check(&self, ask: &Ask) -> Verdict {
        let verdict = self.policy.decide(ask);
        if let Verdict::Deny { parcel, risk } = &verdict {
            let denial = Denial {
                ask: ask.clone(),
                parcel: parcel.clone(),
                risk: *risk,
                ts: epoch(),
            };
            self.policy.record_denial(denial.clone());
            match self.sink.surface(&denial) {
                Decision::AllowOnce | Decision::AllowSession => Verdict::Allow,
                Decision::Deny | Decision::Defer => Verdict::Deny {
                    parcel: parcel.clone(),
                    risk: *risk,
                },
            }
        } else {
            verdict
        }
    }

    pub fn can_write(&self, path: &Path) -> bool {
        matches!(self.check(&Ask::write(path.to_path_buf())), Verdict::Allow)
    }
    pub fn can_read(&self, path: &Path) -> bool {
        matches!(self.check(&Ask::read(path.to_path_buf())), Verdict::Allow)
    }

    /// Enable/disable overlayfs write-quarantine on this already-built context
    /// (the launcher may flip it after probing mount capability, or a user
    /// command may toggle it). Only flips the in-process flag; the actual
    /// overlay mounts are set up / torn down by `overlay`.
    pub fn set_quarantine(&self, enabled: bool) {
        self.quarantine.store(enabled, Ordering::SeqCst);
    }

    /// Whether overlayfs write-quarantine is currently active.
    pub fn is_quarantined(&self) -> bool {
        self.quarantine.load(Ordering::SeqCst)
    }
}

// ===========================================================================
// Request sinks
// ===========================================================================

#[derive(Default)]
pub struct TtySink {
    /// Shared approval context (recent prompts + thinking) for the dialog.
    pub approval: Option<Arc<ApprovalContext>>,
}
impl RequestSink for TtySink {
    fn surface(&self, d: &Denial) -> Decision {
        use crate::term;
        // Try the alternate-screen dialog first. If the terminal isn't a tty
        // (piped/scripted) or the dialog can't start, fall back to the plain
        // line prompt. The dialog reads a key directly (not via `read_answer`),
        // so it works even while a turn has stdin in raw non-blocking mode —
        // fixing the mid-turn auto-deny bug.
        if let Some(approval) = &self.approval {
            if let Some(decision) = crate::modal::tool_approval(d, approval) {
                return match decision {
                    crate::modal::Approval::AllowOnce => Decision::AllowOnce,
                    crate::modal::Approval::AllowSession => Decision::AllowSession,
                    crate::modal::Approval::Deny => Decision::Deny,
                };
            }
        }
        // Fallback: plain line prompt (non-tty or dialog unavailable).
        let what = match &d.ask.path {
            Some(p) => p.display().to_string(),
            None => match &d.ask.target {
                Some(t) => t.clone(),
                None => d.ask.op.verb().to_string(),
            },
        };
        eprintln!(
            "{} {} {}  -> parcel: {} (risk: {})\n      {}",
            term::yellow("[denied]"),
            d.ask.op.verb(),
            what,
            term::bold(&d.parcel.id()),
            d.parcel.default_risk().as_str(),
            term::dim(d.parcel.blast_radius()),
        );
        if !d.ask.reason.is_empty() {
            eprintln!("      {}", term::dim(&format!("reason: {}", d.ask.reason)));
        }
        eprintln!(
            "      {}",
            term::dim("[o] allow  [s] allow session  [n] no  [i] info")
        );
        let ans = term::read_answer("      choice [o/s/n/i, default n]: ");
        match ans.trim().to_ascii_lowercase().as_str() {
            "o" | "once" => Decision::AllowOnce,
            "s" | "session" => Decision::AllowSession,
            "i" | "info" => {
                eprintln!("      {}", term::dim(&format!("blast radius: {}", d.parcel.blast_radius())));
                let again = term::read_answer("      choice [o/s/n, default n]: ");
                match again.trim().to_ascii_lowercase().as_str() {
                    "o" | "once" => Decision::AllowOnce,
                    "s" | "session" => Decision::AllowSession,
                    _ => Decision::Deny,
                }
            }
            _ => Decision::Deny,
        }
    }
}

#[derive(Default)]
pub struct QueuedSink;
impl RequestSink for QueuedSink {
    fn surface(&self, d: &Denial) -> Decision {
        queue_request(d);
        Decision::Defer
    }
}

// ===========================================================================
// Config loading (tolerant, dependency-free)
// ===========================================================================

/// Load the security policy from `~/.pi/agent/security.toml` (if present),
/// else the documented defaults. Tolerant of unknown keys / malformed lines.
pub fn load_policy() -> SecurityPolicy {
    let path = crate::config::pi_dir().join("agent").join("security.toml");
    let mut policy = SecurityPolicy::default();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return policy;
    };
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let k = k.trim().to_ascii_lowercase();
        let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
        match k.as_str() {
            "security.level" | "level" => {
                if let Some(l) = SecurityLevel::parse(&v) {
                    policy.level = l;
                }
            }
            "security.apt" | "apt" => {
                if let Some(a) = AptMode::parse(&v) {
                    policy.apt = a;
                }
            }
            "security.network" | "network" => {
                if let Some(n) = NetworkMode::parse(&v) {
                    policy.network = n;
                }
            }
            "security.ask" | "ask" => {
                if let Some(a) = AskMode::parse(&v) {
                    policy.ask = a;
                }
            }
            "security.read" | "read" => {
                if let Some(r) = ReadMode::parse(&v) {
                    policy.read = r;
                }
            }
            "security.idle" | "idle" => {
                if let Some(i) = IdlePolicy::parse(&v) {
                    policy.idle = i;
                }
            }
            "security.idle-max-open-prs" | "idle-max-open-prs" | "idle_max_open_prs" => {
                if let Ok(n) = v.trim().parse::<usize>() {
                    policy.idle_max_open_prs = n.max(1);
                }
            }
            "security.fixer-agent" | "fixer-agent" | "fixer_agent" => {
                policy.fixer_agent = !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no");
            }
            "security.guard" | "guard" => {
                for pat in v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
                    policy.extra_guard.push(pat);
                }
            }
            "security.quarantine" | "quarantine" => {
                policy.quarantine = !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no");
            }
            "security.quarantine-dirs" | "quarantine-dirs" | "overlay" => {
                for d in v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
                    let p = crate::config::path_from_string(&d);
                    policy.quarantine_dirs.push(p.to_string_lossy().to_string());
                }
            }
            "security.quarantine-project" | "quarantine-project" => {
                policy.quarantine_project =
                    !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no");
            }
            "security.allow-worktree" | "allow-worktree" => {
                let p = crate::config::path_from_string(&v);
                if !p.as_os_str().is_empty() {
                    policy.allow_worktree = Some(p);
                }
            }
            "security.quarantine-staging" | "quarantine-staging" => {
                policy.quarantine_staging = crate::config::path_from_string(&v);
            }
            "security.quarantine-critical" | "quarantine-critical" => {
                for p in v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
                    policy.quarantine_critical.push(p);
                }
            }
            _ => {}
        }
    }
    // Honour PIR_WT_WHITELIST (set by the `wt` extension when it whitelists the
    // agent's own worktree) so the in-process guardrail — the fallback used when
    // the overlay can't mount — also allows writes there and denies the central
    // `.git` / other worktrees.
    if let Some(wt) = std::env::var_os("PIR_WT_WHITELIST") {
        if !wt.is_empty() {
            policy.allow_worktree = Some(PathBuf::from(wt));
        }
    }
    policy
}

// ===========================================================================
// Shared path heuristics (platform-independent core)
// ===========================================================================

pub fn canonicalize_lenient(p: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        std::fs::canonicalize(p).unwrap_or_else(|_| lexical_abs(p))
    }
    #[cfg(not(unix))]
    {
        lexical_abs(p)
    }
}

/// Whether `path` is lexicographically contained within `dir` (after a lenient
/// canonicalize). Used by the repo-isolation guard to confine an agent's writes
/// to its own whitelisted worktree. A trailing separator is appended to both
/// sides so `/a/b` is *not* considered under `/a/bee`, but `/a/b` is under
/// `/a/b`.
pub fn under_dir(path: &Path, dir: &Path) -> bool {
    let p = canonicalize_lenient(path).to_string_lossy().to_string();
    let d = canonicalize_lenient(dir).to_string_lossy().to_string();
    let p = if p.ends_with('/') { p } else { format!("{p}/") };
    let d = if d.ends_with('/') { d } else { format!("{d}/") };
    p.starts_with(&d)
}

/// Whether a path looks like a secret/credential store.
pub fn is_secret(path: &Path) -> bool {
    let s = path.to_string_lossy().to_ascii_lowercase();
    s.contains("/.ssh/")
        || s.ends_with("/.ssh")
        || s.contains("/.aws/")
        || s.contains("/.gnupg/")
        || s.ends_with(".key")
        || s.ends_with(".pem")
        || s.contains("/.config/gh/")
        || s.contains("/.config/google-chrome/")
        || s.contains("/.mozilla/")
        || s.contains("/.config/gcloud/")
}

fn lexical_abs(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    cwd.join(p)
}

pub fn is_database(p: &Path) -> bool {
    let s = p.to_string_lossy().to_ascii_lowercase();
    s.ends_with(".db")
        || s.ends_with(".sqlite")
        || s.ends_with(".sqlite3")
        || s.ends_with(".duckdb")
        || s.ends_with(".db-shm")
        || s.ends_with(".db-wal")
        || s.contains("/postgresql/")
        || s.contains("/mysql/")
        || s.contains("/mongodata/")
        || s.contains("/redis/")
        || s.contains("/var/lib/mysql/")
        || s.contains("/var/lib/postgresql/")
}

pub fn is_system_state(p: &Path) -> bool {
    let s = p.to_string_lossy();
    let abs = canonicalize_lenient(p).to_string_lossy().to_string();
    abs.starts_with("/boot")
        || abs == "/etc"
        || abs.starts_with("/etc/")
        || abs.starts_with("/efi")
        || abs.starts_with("/sys/firmware/efi")
        || abs.starts_with("/lib/systemd/system/")
        // System trees the system-tree overlay quarantines (DEFAULT_OVERLAY_DIRS).
        // When that overlay can't mount (non-root `ai_*` agents), this hard-denies
        // writes here as a fallback so they still can't corrupt system state.
        || abs.starts_with("/var")
        || abs.starts_with("/usr/local")
        || abs.starts_with("/opt")
        || abs.starts_with("/srv")
        || s.starts_with("C:\\Windows\\System32")
        || s.starts_with("C:\\Windows\\boot")
}

pub fn is_test_oracle(p: &Path) -> bool {
    let s = p.to_string_lossy().to_ascii_lowercase();
    s.contains("/expected/")
        || s.contains("/fixtures/")
        || s.contains("/golden/")
        || s.ends_with(".expected")
        || s.ends_with(".expected.txt")
        || s.ends_with(".golden")
        || s.contains("/testdata/expected")
}

pub fn is_repo_git(p: &Path) -> bool {
    let s = p.to_string_lossy();
    let abs = canonicalize_lenient(p).to_string_lossy().to_string();
    // Any path component named exactly ".git" (the repo metadata dir or a
    // submodule's metadata), or a path living underneath one, is denied.
    for comp in abs.split(std::path::is_separator) {
        if comp == ".git" {
            return true;
        }
    }
    s.ends_with("/.git") || s.contains("/.git/")
}

pub fn path_matches(p: &Path, pattern: &str) -> bool {
    let p = p.to_string_lossy();
    let pat = pattern.trim();
    if pat.is_empty() {
        return false;
    }
    if let Some(stripped) = pat.strip_prefix('*') {
        return p.ends_with(stripped);
    }
    if let Some(stripped) = pat.strip_suffix('*') {
        return p.starts_with(stripped);
    }
    if pat.contains('*') {
        let chunks: Vec<&str> = pat.split('*').filter(|c| !c.is_empty()).collect();
        let mut pos = 0;
        for c in &chunks {
            match p[pos..].find(c) {
                Some(i) => pos += i + c.len(),
                None => return false,
            }
        }
        return true;
    }
    p == pat
}

pub fn classify_connect(target: &Option<String>) -> Parcel {
    let t = target.as_deref().unwrap_or("").to_ascii_lowercase();
    if t.contains("github.com") {
        Parcel::NetGithub
    } else if t.contains("crates.io") || t.contains("pypi.org") || t.contains("npmjs") || t.contains(".maven") {
        Parcel::NetPackages
    } else if t.contains("api.search") || t.contains("bing") || t.contains("googleapis") {
        Parcel::NetWebSearch
    } else {
        Parcel::NetPackages
    }
}

pub fn is_other_users(p: &Path) -> bool {
    let abs = canonicalize_lenient(p).to_string_lossy().to_string();
    if abs.starts_with("/home/") {
        return false;
    }
    abs.starts_with("/Users/") && !abs.starts_with("/Users/Shared/")
}

fn epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ===========================================================================
// Idle-agent health probe + task selection (docs/SECURITY_MODEL.md §11.4)
// ===========================================================================

/// The result of probing the repo's code health. The idle agent fixes the
/// *worst* non-clean tier it is allowed to touch, per the active `IdlePolicy`.
pub struct HealthProbe {
    /// True when `cargo build` (or the project's configured check) fails — the
    /// highest-priority "snuck-in error" state.
    pub build_failed: bool,
    /// Whether compiler warnings / lints are present (a non-empty ``
    /// or `cargo clippy` warning stream). Only meaningful when `build_failed`
    /// is false.
    pub has_warnings: bool,
    /// Number of open PRs the agent currently has in flight (for rate-limiting).
    pub open_prs: usize,
}

/// Probe the repo's code health from `repo_root` using the project's build.
/// Returns `None` when there is no recognizable build (so the caller can fall
/// back to a conservative "stay idle" rather than guessing). Best-effort: any
/// spawn error is reported as a failed probe (we'd rather try to fix than
/// silently sit idle on a broken tree). Unix-only command invocation; on
/// non-unix this degrades to `None` (idle agents are a unix-agent feature in
/// this build).
pub fn probe_health(repo_root: &Path, check_cmd: &str) -> Option<HealthProbe> {
    #[cfg(unix)]
    {
        // A "warning" is detected by running the same check a second time and
        // grepping stderr/stdout for warning markers; this is intentionally
        // cheap and conservative — if the check command carries `-D warnings`
        // the first run already returns failure, so `has_warnings` need not be
        // separately computed. We run the check once (to learn build success)
        // and infer warnings from a dedicated lint run when a Cargo project is
        // present.
        let build_failed = !run_check(repo_root, check_cmd);
        let mut has_warnings = false;
        if !build_failed {
            // Cheap warning probe for Rust projects (the common case here).
            if repo_root.join("Cargo.toml").exists() {
                let out = std::process::Command::new("cargo")
                    .args(["build", "--locked"])
                    .current_dir(repo_root)
                    .output();
                if let Ok(o) = out {
                    let log = String::from_utf8_lossy(&o.stderr).to_ascii_lowercase();
                    has_warnings = log.contains("warning:");
                }
            }
        }
        Some(HealthProbe {
            build_failed,
            has_warnings,
            open_prs: count_open_prs(repo_root),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (repo_root, check_cmd);
        None
    }
}

#[cfg(unix)]
fn run_check(repo_root: &Path, check_cmd: &str) -> bool {
    use std::process::Command;
    if check_cmd.trim().is_empty() {
        // No configured check: nothing we can verify, treat as "clean" so we
        // don't spuriously try to fix a tree we can't even build.
        return true;
    }
    Command::new("bash")
        .arg("-lc")
        .arg(check_cmd)
        .current_dir(repo_root)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn count_open_prs(repo_root: &Path) -> usize {
    use std::process::Command;
    // Only meaningful when `gh` is present and there's a remote.
    let has_gh = Command::new("gh")
        .args(["--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !has_gh {
        return 0;
    }
    let out = Command::new("gh")
        .args(["pr", "list", "--author", "@me", "--json", "number", "--jq", "length"])
        .current_dir(repo_root)
        .output();
    match out {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).trim().parse::<usize>().unwrap_or(0)
        }
        _ => 0,
    }
}

/// Decide the prompt an idle agent should give itself, or `None` to stay idle.
/// Priority order (§11.4): errors first, then warnings, then optional hygiene.
/// Always respects the `IdlePolicy` and the per-agent open-PR rate limit.
pub fn idle_prompt(policy: &SecurityPolicy, probe: &HealthProbe) -> Option<String> {
    if policy.idle == IdlePolicy::Off {
        return None;
    }
    // Rate-limit: don't open more than `idle_max_open_prs` self-PRs at once.
    if probe.open_prs >= policy.idle_max_open_prs {
        return None;
    }
    if probe.build_failed {
        // Highest priority: fix the snuck-in breakage. The agent is already in
        // its own worktree (worktree mode) or on the trunk checkout (other
        // modes); either way it must scope the fix to the failing build/test.
        return Some(
            "Repo build/test is currently failing. Diagnose and fix the failure, keeping the \
             change minimal and scoped to what broke. Re-run the build/test to confirm it's green \
             before finishing."
                .to_string(),
        );
    }
    if probe.has_warnings && policy.idle.covers(HealthTier::Warning) {
        return Some(
            "Clear compiler warnings and lints in this repo (unused vars, dead code, clippy, \
             deprecations) without changing behaviour. Keep the change tightly scoped; do not widen \
             scope or touch unrelated files. Re-run the build to confirm no new warnings appear."
                .to_string(),
        );
    }
    if policy.idle == IdlePolicy::Hygiene {
        return Some(
            "Apply low-risk hygiene to this repo: run the formatter (cargo fmt / prettier), add \
             missing doc comments on public items, and resolve trivial TODOs. Do not change \
             behaviour or signatures. Keep the change tightly scoped."
                .to_string(),
        );
    }
    None
}

/// Apply the worktree security policy to the process environment so the
/// existing `wt` extension enters PR-submission mode (repo-isolated). Called at
/// startup when `security.level = "worktree"`. Mirrors the env vars the `wt`
/// extension already on (`PIR_WT_PR=1`, `PIR_WT=1`), so no duplicate logic
/// is needed in the extension.
pub fn apply_worktree_env(policy: &SecurityPolicy) {
    if policy.level.is_worktree() {
        std::env::set_var("PIR_WT", "1");
        std::env::set_var("PIR_WT_PR", "1");
        std::env::set_var("PIR_WT_AUTO", "1");
    }
}

// ===========================================================================
// Request queue (headless / `ai_*` agents)
// ===========================================================================

pub fn queue_request(d: &Denial) {
    #[cfg(unix)]
    {
        crate::security::unix::queue_perm_request(d);
    }
    #[cfg(not(unix))]
    {
        eprintln!(
            "[pir] security request (deferred): {} {} -> parcel {}",
            d.ask.op.verb(),
            d.ask
                .path
                .as_ref()
                .map(|p| p.display().to_string())
                .or_else(|| d.ask.target.clone())
                .unwrap_or_default(),
            d.parcel.id()
        );
    }
}

// ===========================================================================
// Submodules (platform-specific)
// ===========================================================================

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;
/// Privilege-escalation contract: the single audited boundary where granted
/// escalations are exercised, and where host-root writes are reaped back to the
/// project owner so files never end up host-root-owned.
pub mod privilege;
/// Overlayfs-backed write quarantine: stage the agent's writes into an overlay
/// `upperdir` so the real filesystem is untouched until the operator reviews +
/// applies them. On by default when the launcher can mount (root).
pub mod overlay;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_off_allows_everything() {
        let p = SecurityPolicy { level: SecurityLevel::Off, ..Default::default() };
        assert_eq!(p.decide(&Ask::write("/etc/passwd")), Verdict::Allow);
        assert_eq!(p.decide(&Ask::read("/root/.ssh/id_rsa")), Verdict::Allow);
    }

    #[test]
    fn guard_blocks_db_and_secret() {
        let p = SecurityPolicy::default(); // guard, read=Open
        assert!(matches!(
            p.decide(&Ask::write("/srv/app/production.sqlite3")),
            Verdict::Deny { parcel: Parcel::GuardDb, .. }
        ));
        assert_eq!(p.decide(&Ask::read("/home/me/.ssh/id_ed25519")), Verdict::Allow);

        let mut g = SecurityPolicy::default();
        g.read = ReadMode::GuardedSecrets;
        assert!(matches!(
            g.decide(&Ask::read("/home/me/.ssh/id_ed25519")),
            Verdict::Deny { parcel: Parcel::GuardSecrets, .. }
        ));
    }

    #[test]
    fn read_mode_defaults_open() {
        assert_eq!(SecurityPolicy::default().read, ReadMode::Open);
        assert_eq!(ReadMode::parse("open"), Some(ReadMode::Open));
        assert_eq!(ReadMode::parse("guarded-secrets"), Some(ReadMode::GuardedSecrets));
        assert_eq!(ReadMode::parse("bogus"), None);
    }

    #[test]
    fn guard_allows_normal_writes() {
        let p = SecurityPolicy::default();
        assert_eq!(p.decide(&Ask::write("/home/me/project/src/main.rs")), Verdict::Allow);
        assert_eq!(p.decide(&Ask::write("/tmp/scratch/x")), Verdict::Allow);
    }

    #[test]
    fn network_modes_classify() {
        let mut p = SecurityPolicy::default();
        p.network = NetworkMode::On;
        assert_eq!(p.decide(&Ask::connect("github.com:443")), Verdict::Allow);
        p.network = NetworkMode::Off;
        assert!(matches!(
            p.decide(&Ask::connect("github.com:443")),
            Verdict::Deny { parcel: Parcel::NetNone, .. }
        ));
    }

    #[test]
    fn path_glob_matches() {
        assert!(path_matches(Path::new("/a/b/secret"), "*/secret"));
        assert!(path_matches(Path::new("/a/b/secret"), "/a/*"));
        assert!(!path_matches(Path::new("/a/b/secret"), "/x/*"));
        assert!(path_matches(Path::new("/etc/shadow"), "/etc/*"));
    }

    #[test]
    fn databaseognition() {
        assert!(is_database(Path::new("/data/app.duckdb")));
        assert!(is_database(Path::new("/var/lib/postgresql/14/main/x")));
        assert!(!is_database(Path::new("/home/me/project/x.rs")));
    }

    #[test]
    fn repo_git_recognized() {
        let p = SecurityPolicy::default();
        assert!(matches!(
            p.decide(&Ask::write("/home/me/project/.git/HEAD")),
            Verdict::Deny { parcel: Parcel::GuardRepoGit, .. }
        ));
        assert!(matches!(
            p.decide(&Ask::write("/home/me/project/.git/refs/heads/main")),
            Verdict::Deny { parcel: Parcel::GuardRepoGit, .. }
        ));
        assert!(matches!(
            p.decide(&Ask::write("/home/me/project/.git")),
            Verdict::Deny { parcel: Parcel::GuardRepoGit, .. }
        ));
        // A normal project file (no .git component) is allowed.
        assert_eq!(p.decide(&Ask::write("/home/me/project/src/main.rs")), Verdict::Allow);
        assert!(is_repo_git(Path::new("/home/me/project/.git/hooks/pre-commit")));
        assert!(!is_repo_git(Path::new("/home/me/project/src/git_util.rs")));
    }

    #[test]
    fn parse_levels_and_modes() {
        assert_eq!(SecurityLevel::parse("guard"), Some(SecurityLevel::Guard));
        assert_eq!(AptMode::parse("stage"), Some(AptMode::Stage));
        assert_eq!(NetworkMode::parse("allowlist"), Some(NetworkMode::AllowList));
        assert_eq!(AskMode::parse("auto-no"), Some(AskMode::AutoNo));
        assert_eq!(SecurityLevel::parse("bogus"), None);
    }
}
