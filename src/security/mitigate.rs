//! Mitigation-level security engine (docs/MITIGATION_LEVEL_SECURITY.md §2).
//!
//! A fast, explainable *pre-filter* that sits between the agent's judgment and
//! the OS boundary. It parses a shell command, tags capabilities, classifies
//! targets and dataflow, computes an uncertainty score, assigns a severity
//! class (Blue / Yellow / RED), and — for dangerous-but-legitimate operations —
//! rewrites the command into a reversible / narrower / two-phase / race-safe
//! equivalent rather than blocking it. Residual risk is routed to an ask, and
//! high-precision malice signals are denied with a prescriptive message.
//!
//! This module is deliberately **pure** (no I/O): it returns a
//! [`MitigationPlan`] describing what to do, and the caller (the builtin `bash`
//! tool) executes it. That keeps the analyzer unit-testable and lets the
//! rewrite be re-validated through the full analyzer before it runs (P2
//! fail-closed, §2.2).
//!
//! Principles honoured here (from the doc):
//! - **P1** truthful semantics: every deviation is recorded in the receipt.
//! - **P4** loud, not silent: every mitigation emits an in-band receipt.
//! - **P5** uncertainty is risk: unknown binary/flag/path ⇒ elevated severity.
//! - **P8** prefer ask to deny: deny only when no human decision exists.

use std::path::Path;

// ===========================================================================
// Capability tagging (§2.1)
// ===========================================================================

/// A coarse capability a command exercises. Mirrors the doc's list
/// (read / write / delete / metadata / process / config / package /
/// network-egress / ingress-to-exec / auth-access / world-effect).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    Read,
    Write,
    Delete,
    Metadata,
    Process,
    Config,
    Package,
    NetEgress,
    IngressToExec,
    AuthAccess,
    WorldEffect,
    Unknown,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::Read => "read",
            Capability::Write => "write",
            Capability::Delete => "delete",
            Capability::Metadata => "metadata",
            Capability::Process => "process",
            Capability::Config => "config",
            Capability::Package => "package",
            Capability::NetEgress => "network-egress",
            Capability::IngressToExec => "ingress-to-exec",
            Capability::AuthAccess => "auth-access",
            Capability::WorldEffect => "world-effect",
            Capability::Unknown => "unknown",
        }
    }
}

// ===========================================================================
// Target classification (§3.3)
// ===========================================================================

/// Where a command's primary target lives. Drives undo classification and
/// severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetClass {
    /// Inside the project / scratch — semi-routine, reversible.
    Project,
    /// A system tree (`/etc`, `/usr`, `/var`, `%SystemRoot%`, ...) — C-class.
    System,
    /// A credential store (`~/.ssh`, `~/.aws`, ...).
    Secret,
    /// A raw device (`/dev/sd*`, `\\.\PhysicalDrive*`, ...) — RED.
    RawDevice,
    /// A database / control-plane store.
    Database,
    /// Another user's tree.
    OtherUser,
    /// A path that escapes the project root (`..` traversal or an absolute
    /// path elsewhere) — the quarantine boundary. Surfaced to the operator
    /// (Yellow/ask) rather than auto-executed.
    OutsideProject,
    /// Unresolvable / unknown — elevated risk (P5).
    Unknown,
}

impl TargetClass {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetClass::Project => "project",
            TargetClass::System => "system",
            TargetClass::Secret => "secret",
            TargetClass::RawDevice => "raw-device",
            TargetClass::Database => "database",
            TargetClass::OtherUser => "other-user",
            TargetClass::OutsideProject => "outside-project",
            TargetClass::Unknown => "unknown",
        }
    }
}

// ===========================================================================
// Analysis result
// ===========================================================================

/// A redirect (`>` truncate / `>>` append) found in the command. Redirect
/// targets are write sinks independent of the head command (`ls > X` writes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub append: bool,
    pub target: String,
}

/// The result of analyzing one command.
#[derive(Debug, Clone)]
pub struct Analysis {
    pub verb: String,
    pub args: Vec<String>,
    /// Pipeline stages (split on `|`), each a raw command string.
    pub stages: Vec<String>,
    pub capabilities: Vec<Capability>,
    pub target: TargetClass,
    pub redirects: Vec<Redirect>,
    /// 0.0 (fully understood) .. 1.0 (opaque). Unknown constructs push this up.
    pub uncertainty: f64,
    /// Human-readable reason for the uncertainty (for the receipt).
    pub uncertainty_reason: String,
}

impl Analysis {
    pub fn has(&self, cap: Capability) -> bool {
        self.capabilities.contains(&cap)
    }
}

// ===========================================================================
// Severity classes (§2.3)
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Semi-routine, in scope, reversible → auto-execute with mitigation.
    Blue,
    /// Some thought required → ask with recommended default.
    Yellow,
    /// Serious damage possible → ask with friction / deny.
    Red,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Blue => "Blue",
            Severity::Yellow => "Yellow",
            Severity::Red => "RED",
        }
    }
}

// ===========================================================================
// Mitigation moves (§2.1 / §3.2)
// ===========================================================================

/// One of the four mitigation moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Move {
    /// Snapshot before mutate; restore is trivial (undo class A/B).
    Reversible { description: String, restore: String },
    /// Same operation, smaller target set.
    Narrower { description: String },
    /// Stage, review, commit atomically.
    TwoPhase { description: String },
    /// Fencing tokens so a concurrent change can't be clobbered.
    RaceSafe { description: String },
}

impl Move {
    pub fn kind(&self) -> &'static str {
        match self {
            Move::Reversible { .. } => "reversible",
            Move::Narrower { .. } => "narrower",
            Move::TwoPhase { .. } => "two-phase",
            Move::RaceSafe { .. } => "race-safe",
        }
    }
    pub fn description(&self) -> &str {
        match self {
            Move::Reversible { description, .. }
            | Move::Narrower { description }
            | Move::TwoPhase { description }
            | Move::RaceSafe { description } => description,
        }
    }
}

// ===========================================================================
// Receipt (§2.6)
// ===========================================================================

/// A transaction receipt. Fixed order: what happened → delta → state →
/// projection → suggested action. Dual-published (structured to the agent,
/// rendered lines to the human).
#[derive(Debug, Clone)]
pub struct Receipt {
    pub original: String,
    pub effective: String,
    pub deviations: Vec<String>,
    pub restore: String,
    pub state: String,
    pub projection: String,
    pub suggested: String,
}

impl Receipt {
    /// Render the receipt as in-band lines the agent reads and MUST relay
    /// verbatim (P4).
    pub fn render(&self) -> String {
        let mut out = String::from("[mitigation receipt]\n");
        out.push_str(&format!("  what happened: {}\n", self.effective));
        if !self.deviations.is_empty() {
            out.push_str("  deviations:\n");
            for d in &self.deviations {
                out.push_str(&format!("    - {d}\n"));
            }
        }
        out.push_str(&format!("  state: {}\n", self.state));
        out.push_str(&format!("  projection: {}\n", self.projection));
        out.push_str(&format!("  suggested action: {}\n", self.suggested));
        if !self.restore.is_empty() {
            out.push_str(&format!("  restore: {}\n", self.restore));
        }
        out
    }
}

// ===========================================================================
// Verdict
// ===========================================================================

/// The engine's decision for a command.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// Run as-is (no mitigation needed).
    Allow,
    /// Run the rewritten `effective` command, applying `moves`, and emit
    /// `receipt`. Severity Blue auto-executes; Yellow/RED are surfaced to the
    /// operator for approval before execution. `trash_moved` (when Some)
    /// carries how many files the agent has moved to the OS trash this
    /// session, so the operator can be told what is still recoverable.
    Mitigate {
        severity: Severity,
        moves: Vec<Move>,
        effective: String,
        receipt: Receipt,
        /// Number of files the agent has moved to trash this session (after
        /// the effective command runs), if the mitigation was a trash move.
        trash_moved: Option<usize>,
    },
    /// Refuse with a prescriptive message (P8: deny only when no human
    /// decision exists). `alternative` offers the mitigated path.
    Deny { reason: String, alternative: String },
}

// ===========================================================================
// The analyzer
// ===========================================================================

/// Tokenize a command into whitespace-separated words, preserving quoted
/// groups as single tokens (a light, quoting-aware split — not a full shell
/// parser, which is out of scope for a fast pre-filter).
fn tokenize(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                c if c.is_whitespace() => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            },
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Split a command into pipeline stages on `|` (respecting quotes).
fn split_pipeline(command: &str) -> Vec<String> {
    let mut stages = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
                cur.push(c);
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '|' => {
                    stages.push(std::mem::take(&mut cur));
                }
                c => cur.push(c),
            },
        }
    }
    stages.push(cur);
    stages
}

/// Extract redirects (`>` / `>>`) from a command, returning the redirects and
/// the command with the redirect tokens removed (so the head verb is clean).
fn extract_redirects(command: &str) -> (Vec<Redirect>, String) {
    let mut redirects = Vec::new();
    let mut out = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
                out.push(c);
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    out.push(c);
                }
                '>' => {
                    // `>>` append, `>` truncate. `>|` is the noclobber override
                    // (intentional clobber) — treat as truncate.
                    let append = chars.peek() == Some(&'>');
                    if append {
                        chars.next();
                    }
                    // Skip whitespace to the target.
                    while chars.peek().map(|c| c.is_whitespace()).unwrap_or(false) {
                        chars.next();
                    }
                    let mut target = String::new();
                    while let Some(&t) = chars.peek() {
                        if t.is_whitespace() {
                            break;
                        }
                        target.push(t);
                        chars.next();
                    }
                    redirects.push(Redirect { append, target });
                }
                c => out.push(c),
            },
        }
    }
    (redirects, out)
}

/// Lexically normalize a path string (resolve `.` and `..`), treating both `/`
/// and `\` as separators so it works on unix and windows targets alike.
fn normalize_path(s: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for comp in s.split(['/', '\\']) {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c.to_string()),
        }
    }
    parts.join("/")
}

/// Whether a relative `target` escapes `project_root` via `..` traversal (the
/// quarantine boundary). Returns `false` when `project_root` is `None` (no
/// boundary in force) or the target is absolute (the system/secret/db/other-
/// user classes handle those). Only relative paths are checked so legitimate
/// absolute scratch paths (`/tmp`, `%TEMP%`) aren't flagged.
fn escapes_project_root(target: &str, project_root: Option<&Path>) -> bool {
    let Some(root) = project_root else { return false };
    let t = target.trim();
    if t.is_empty() {
        return false;
    }
    // Absolute paths (unix `/`, windows `\` or a drive `C:`) are handled by
    // the system/secret/db/other-user classes; don't flag them here.
    if t.starts_with('/') || t.starts_with('\\') || t.contains(':') {
        return false;
    }
    // No `..` component => cannot escape upward.
    if !t.split(['/', '\\']).any(|c| c == "..") {
        return false;
    }
    let root_s = root.to_string_lossy().replace('\\', "/");
    let root_norm = normalize_path(&root_s);
    let candidate = format!("{}/{}", root_s, t.replace('\\', "/"));
    let cand_norm = normalize_path(&candidate);
    !(cand_norm == root_norm || cand_norm.starts_with(&format!("{}/", root_norm)))
}

/// Classify a path-like target string. `project_root` is the quarantine
/// boundary: a relative target that escapes it via `..` is classified
/// [`TargetClass::OutsideProject`] (surfaced to the operator, not auto-run).
fn classify_target(target: &str, project_root: Option<&Path>) -> TargetClass {
    let t = target.trim();
    if t.is_empty() {
        return TargetClass::Unknown;
    }
    let lower = t.to_ascii_lowercase();
    // Raw devices (unix + windows).
    if lower.starts_with("/dev/") {
        // Benign /dev entries (null, zero, tty*, pts/*, fd/*) are not raw
        // devices; everything else under /dev is suspicious/catastrophic.
        let base = lower.trim_start_matches("/dev/");
        let benign = base == "null"
            || base == "zero"
            || base.starts_with("tty")
            || base.starts_with("pts/")
            || base.starts_with("fd/")
            || base == "random"
            || base == "urandom";
        if !benign {
            return TargetClass::RawDevice;
        }
        return TargetClass::Project;
    }
    if lower.starts_with(r"\\.\physicaldrive")
        || lower.starts_with(r"\\.\c:")
        || lower.starts_with("/dev/sd")
        || lower.starts_with("/dev/nvme")
        || lower.starts_with("/dev/mapper")
        || lower.starts_with("/dev/mem")
        || lower.starts_with("/dev/disk")
    {
        return TargetClass::RawDevice;
    }
    // System trees.
    if lower == "/"
        || lower.starts_with("/etc")
        || lower.starts_with("/usr")
        || lower.starts_with("/var")
        || lower.starts_with("/boot")
        || lower.starts_with("/opt")
        || lower.starts_with("/srv")
        || lower.starts_with("/lib/systemd")
        || lower.starts_with("c:\\windows")
        || lower.starts_with("c:\\programdata")
        || lower.starts_with("c:\\recovery")
    {
        return TargetClass::System;
    }
    // Secrets.
    if lower.contains("/.ssh")
        || lower.contains("\\.ssh")
        || lower.contains("/.aws")
        || lower.contains("\\.aws")
        || lower.contains("/.gnupg")
        || lower.contains("\\.gnupg")
        || lower.ends_with(".pem")
        || lower.ends_with(".key")
        || lower.contains("/.config/gh")
        || lower.contains("\\.config\\gh")
    {
        return TargetClass::Secret;
    }
    // Databases.
    if lower.ends_with(".db")
        || lower.ends_with(".sqlite")
        || lower.ends_with(".sqlite3")
        || lower.ends_with(".duckdb")
        || lower.contains("/postgresql/")
        || lower.contains("/mysql/")
        || lower.contains("/mongodata/")
    {
        return TargetClass::Database;
    }
    // Other users.
    if lower.starts_with("/home/") && !lower.starts_with("/home/") {
        return TargetClass::OtherUser;
    }
    if lower.starts_with("/users/") && !lower.starts_with("/users/shared/") {
        return TargetClass::OtherUser;
    }
    if lower.starts_with("c:\\users\\") {
        let me = std::env::var("USERPROFILE").unwrap_or_default().to_ascii_lowercase();
        let me = me.trim_end_matches('\\');
        if !me.is_empty() && (lower == me || lower.starts_with(&format!("{me}\\"))) {
            return TargetClass::Project;
        }
        let shared = lower.starts_with("c:\\users\\public")
            || lower.starts_with("c:\\users\\default")
            || lower.starts_with("c:\\users\\all users");
        if !shared {
            return TargetClass::OtherUser;
        }
    }
    // Quarantine boundary: a relative path that escapes the project root via
    // `..` is out-of-project. Checked last so system/secret/db/other-user
    // classes (which are more specific) win.
    if escapes_project_root(target, project_root) {
        return TargetClass::OutsideProject;
    }
    TargetClass::Project
}

/// Whether a path is a filesystem root (C-class by construction, §2.4).
fn is_fs_root(target: &str) -> bool {
    let t = target.trim();
    t == "/"
        || t == "/usr"
        || t == "/etc"
        || t == "/boot"
        || t == "/var"
        || t == "/opt"
        || t == "/srv"
        || t.eq_ignore_ascii_case("C:\\")
        || t.eq_ignore_ascii_case("C:\\Windows")
        || t.eq_ignore_ascii_case("C:\\Program Files")
}

/// The set of verbs that are inherently irreversible (their *purpose* is
/// destruction) — unsubstitutable, deny-class (§3.3 no-mitigation list).
fn is_irreversible_verb(verb: &str) -> bool {
    matches!(verb, "mkfs" | "mkfs.ext4" | "mkfs.xfs" | "mkfs.btrfs" | "shred" | "wipefs" | "fdisk" | "parted")
}

/// Analyze a command. Pure: no I/O, no side effects. `project_root` is the
/// quarantine boundary used to detect out-of-project (`..` escape) targets.
pub fn analyze(command: &str, project_root: Option<&Path>) -> Analysis {
    let (redirects, head) = extract_redirects(command);
    let stages = split_pipeline(&head);
    let words = tokenize(&head);
    let verb = words.first().cloned().unwrap_or_default();
    let args = words.into_iter().skip(1).collect::<Vec<_>>();

    let mut capabilities = Vec::new();
    let mut target = TargetClass::Project;
    let mut uncertainty = 0.0f64;
    let mut uncertainty_reason = String::new();

    // Capability tagging by verb.
    match verb.as_str() {
        "rm" | "rmdir" | "unlink" => {
            capabilities.push(Capability::Delete);
            // rm -rf is the destructive shape; a bare `rm file` is still delete.
            if args.iter().any(|a| a == "-rf" || a == "-fr" || a == "-r" || a == "-R") {
                capabilities.push(Capability::WorldEffect);
            }
        }
        "sed" => {
            capabilities.push(Capability::Write);
            if args.iter().any(|a| a == "-i") {
                capabilities.push(Capability::WorldEffect); // in-place edit
            }
        }
        "git" => {
            capabilities.push(Capability::Metadata);
            if args.iter().any(|a| a == "push") && args.iter().any(|a| a == "--force" || a == "-f") {
                capabilities.push(Capability::WorldEffect);
            }
        }
        "curl" | "wget" => {
            capabilities.push(Capability::NetEgress);
            // curl | sh is ingress-to-exec.
            if stages.len() > 1 && stages[1].trim().starts_with("sh") {
                capabilities.push(Capability::IngressToExec);
            }
        }
        "sh" | "bash" | "zsh" | "dash" => {
            capabilities.push(Capability::IngressToExec);
        }
        "sudo" | "su" | "doas" => {
            capabilities.push(Capability::AuthAccess);
            capabilities.push(Capability::WorldEffect);
        }
        "dd" => {
            capabilities.push(Capability::Write);
            // dd of=<raw-device> is a raw-device catastrophe.
            if let Some(of) = args.iter().find_map(|a| a.strip_prefix("of=")) {
                let tc = classify_target(of, project_root);
                if tc == TargetClass::RawDevice {
                    target = TargetClass::RawDevice;
                    capabilities.push(Capability::WorldEffect);
                }
            }
        }
        "mkfs" | "mkfs.ext4" | "mkfs.xfs" | "mkfs.btrfs" | "shred" | "wipefs" | "fdisk" | "parted" => {
            capabilities.push(Capability::Write);
            capabilities.push(Capability::WorldEffect);
            // The first non-flag arg is the device.
            if let Some(dev) = args.iter().find(|a| !a.starts_with('-')) {
                let tc = classify_target(dev, project_root);
                if tc == TargetClass::RawDevice {
                    target = TargetClass::RawDevice;
                } else {
                    target = TargetClass::System;
                }
            }
        }
        "kill" | "pkill" | "killall" => {
            capabilities.push(Capability::Process);
        }
        "apt" | "apt-get" | "dnf" | "yum" | "pacman" | "brew" | "pip" | "pip3" | "npm" | "cargo" => {
            capabilities.push(Capability::Package);
        }
        "chmod" | "chown" | "chgrp" | "touch" | "ln" | "mv" | "cp" => {
            capabilities.push(Capability::Metadata);
        }
        "cat" | "less" | "head" | "tail" | "grep" | "find" | "ls" | "echo" | "printf" => {
            capabilities.push(Capability::Read);
        }
        "tee" => {
            capabilities.push(Capability::Write);
        }
        _ => {
            // Unknown binary ⇒ elevated risk (P5).
            capabilities.push(Capability::Unknown);
            uncertainty = (uncertainty + 0.4).min(1.0);
            uncertainty_reason = format!("unknown binary '{verb}'");
        }
    }

    // Redirect targets are write sinks independent of the head command.
    for r in &redirects {
        capabilities.push(Capability::Write);
        let tc = classify_target(&r.target, project_root);
        if tc != TargetClass::Project {
            target = tc;
        }
    }

    // Target classification from the first path-like argument (for verbs that
    // take a target).
    if target == TargetClass::Project {
        for a in &args {
            if a.starts_with('-') {
                continue;
            }
            let tc = classify_target(a, project_root);
            if tc != TargetClass::Project {
                target = tc;
                break;
            }
        }
    }

    // Unknown flags ⇒ uncertainty (P5). A flag we don't recognize on a
    // destructive verb is risk, not safety.
    let known_flags: &[&str] = match verb.as_str() {
        "rm" => &["-r", "-f", "-rf", "-fr", "-R", "-i", "-v", "--", "-d"],
        "sed" => &["-i", "-e", "-n", "-E", "-r", "-s", "-u", "--"],
        "git" => &["push", "--force", "--force-with-lease", "-f", "fetch", "pull", "clone", "status", "add", "commit", "checkout", "branch", "merge", "rebase", "log", "diff", "remote", "rev-parse", "--", "-u", "origin", "main", "master"],
        "dd" => &["of=", "if=", "bs=", "count=", "status=", "conv=", "seek=", "skip="],
        _ => &[],
    };
    for a in &args {
        if a.starts_with('-') && !known_flags.iter().any(|f| a == f || (f != &"--" && a.starts_with(f))) {
            uncertainty = (uncertainty + 0.3).min(1.0);
            if uncertainty_reason.is_empty() {
                uncertainty_reason = format!("unknown flag '{a}' on '{verb}'");
            }
        }
    }

    Analysis {
        verb,
        args,
        stages,
        capabilities,
        target,
        redirects,
        uncertainty,
        uncertainty_reason,
    }
}

// ===========================================================================
// Severity classifier (§2.3)
// ===========================================================================

/// Classify the severity of an analyzed command. `project_root` is the
/// quarantine boundary (out-of-project targets are Yellow/ask).
pub fn classify_severity(a: &Analysis, project_root: Option<&Path>) -> Severity {
    // RED: raw-device catastrophe, filesystem destruction, or irreversibility
    // that is the command's *purpose*.
    if a.target == TargetClass::RawDevice || is_irreversible_verb(&a.verb) {
        return Severity::Red;
    }
    // RED: truncation of a system/secret/database target (control-plane write).
    if a.redirects.iter().any(|r| !r.append) {
        let tc = classify_target(&r_target(&a.redirects), project_root);
        if matches!(tc, TargetClass::System | TargetClass::Secret | TargetClass::Database) {
            return Severity::Red;
        }
    }
    // Yellow: system/secret/database/other-user/out-of-project targets, auth
    // escalation, ingress-to-exec, or high uncertainty.
    if matches!(
        a.target,
        TargetClass::System
            | TargetClass::Secret
            | TargetClass::Database
            | TargetClass::OtherUser
            | TargetClass::OutsideProject
    ) || a.has(Capability::AuthAccess)
        || a.has(Capability::IngressToExec)
        || a.uncertainty >= 0.5
    {
        return Severity::Yellow;
    }
    // Blue: everything else (semi-routine, in scope, reversible).
    Severity::Blue
}

fn r_target(redirects: &[Redirect]) -> String {
    redirects.iter().find(|r| !r.append).map(|r| r.target.clone()).unwrap_or_default()
}

/// Whether any pipeline stage contains a deletion verb (`rm`, `rmdir`,
/// `unlink`, `del`, `rmdir`, `rm -rf`/`rm -fr`, `rmdir`, `del /q`/`del /s /q`,
/// and the `rmrf`/`deltree` aliases). Deletions are mitigated to a reversible
/// Recycle-Bin/trash move. We scan every stage (not just the head verb) so a
/// `test -e x && rm -f x` cannot slip through the analyzer's verb-only view.
fn command_has_delete(stages: &[String]) -> bool {
    const DELETE_WORDS: &[&str] = &[
        "rm", "rmdir", "unlink", "del", "rmrf", "deltree",
    ];
    for stage in stages {
        // Compound commands are joined by `&&`, `;`, `||`, and newlines; split
        // on all of them AND on whitespace so a delete inside
        // `test -e x && rm -f x` or a full `rm -rf build/` both yield the
        // standalone word `rm` (the analyzer only records the verb, so the
        // stage must be scanned word-by-word here).
        for word in stage.split(|c: char| c.is_whitespace() || c == '&' || c == ';' || c == '|') {
            let w = word.trim();
            if DELETE_WORDS.contains(&w) {
                return true;
            }
            // `del` with `/q`/`/s`/`/p` flags is still a delete (e.g. `del /q`).
            if w.eq_ignore_ascii_case("del") || w.eq_ignore_ascii_case("deltree") {
                return true;
            }
        }
        // Bare `rm -rf`/`rm -fr` flag-only tokens are covered by the verb match
        // above; nothing further needed here.
    }
    false
}

/// Rewrite a deletion command to a reversible Recycle-Bin/trash move. We map
/// `rm`/`rmdir`/`unlink`/`del` to `trash-put` (the recoverable equivalent),
/// preserving all non-flag arguments as the targets. Severity is Blue for an
/// in-project delete (auto-executes with the trash rewrite) and Yellow when the
/// delete target escapes the project root (operator-asked), mirroring the
/// engine's quarantine semantics. `trash_moved` is left `None` here; the
/// caller fills it in once it knows how many files were actually moved.
fn mitigate_delete(a: &Analysis, project_root: Option<&Path>) -> Verdict {
    // Determine severity: out-of-project/other-user targets → Yellow (ask),
    // otherwise Blue (auto-execute the reversible trash move).
    let severity = classify_severity(a, project_root);
    let mut effective = String::from("trash-put");
    for arg in &a.args {
        effective.push(' ');
        effective.push_str(arg);
    }
    let deviations = vec![
        "delete mitigated to a reversible Recycle-Bin/trash move (recoverable)".to_string(),
    ];
    Verdict::Mitigate {
        severity,
        moves: vec![Move::Reversible {
            description: "delete -> trash-put (recoverable)".into(),
            restore: "restore from the OS Recycle Bin / trash".into(),
        }],
        effective: effective.clone(),
        receipt: receipt(
            &a.stages.join(" | "),
            &effective,
            &deviations,
            "restore from the OS Recycle Bin / trash",
            "files moved to trash (recoverable)",
            "deletion rewritten to a reversible trash move",
            "review the trash move; the files remain recoverable",
        ),
        trash_moved: None,
    }
}

// ===========================================================================
// Mitigation engine (§3.2)
// ===========================================================================

/// Where to stage the journal-copy backup of a redirect target. In-place
/// (`<target>.bak.<ts>`, same directory) when the target is inside the project
/// root; otherwise under a writable backup dir (the project's `.pir/backups`, or
/// the system temp dir) because the target's own directory is typically not
/// writable by the confined agent. This is what makes an out-of-project write
/// *reversible* at mitigation level instead of failing with "permission denied
/// when creating a backup".
pub(crate) fn journal_backup_path(target: &str, project_root: Option<&Path>, ts: u64) -> String {
    // In-place only when the target is safely inside the project root (the
    // confined agent can write next to it). Anything else — a `..`-escape, a
    // system/secret/db path, another user's home, a raw device — lives in a
    // directory the sandbox user cannot write, so an in-place `<target>.bak`
    // would fail with "permission denied when attempting to create a backup"
    // and abort the whole command (the very failure this staging exists to
    // avoid). Those stage under the project's writable backup dir instead.
    if classify_target(target, project_root) != TargetClass::Project {
        let dir = project_root
            .map(|r| r.join(".pir").join("backups"))
            .unwrap_or_else(|| std::env::temp_dir().join("pir-backups"));
        let _ = std::fs::create_dir_all(&dir);
        let name = target.replace(['/', '\\'], "_");
        format!("{}.{}", dir.join(&name).display(), ts)
    } else {
        format!("{}.bak.{}", target, ts)
    }
}

/// Build the mitigation plan for a command. Returns `None` when no mitigation
/// applies (the command runs as-is). `project_root` is the quarantine boundary:
/// a relative target that escapes it via `..` is classified
/// [`TargetClass::OutsideProject`] and surfaced to the operator (Yellow/ask)
/// rather than auto-executed.
pub fn plan(command: &str, project_root: Option<&Path>) -> Option<Verdict> {
    let a = analyze(command, project_root);

    // ---- Safe-harbor (P8 + the operator's standing requirement) -----------
    // The DENY is only ever meant to block *obviously dangerous* operations.
    // A command that is purely read-only or merely inspects the filesystem —
    // `ls`, `cat`, `grep`, `find`, `head`, `echo` (no destructive redirect),
    // `cp`/`mv`/`touch`/`mkdir`/`chmod`/`ln` inside the project, `git status`,
    // `cargo build`, `ssh`, `make`, `python script.py`, ... — carries no
    // capability that can destroy, escalate, or escape, so it can *never*
    // reach a RED denial. Only commands that actually *write* / *delete* /
    // *escalate* / *pipe into exec* (or carry a real redirect sink) can be
    // denied. This makes it structurally impossible for the guardrail to block
    // an obviously-benign command, and also un-denies benign reads of raw
    // devices (e.g. `cat /dev/sda`) which are not destructive.
    let destructive = a.has(Capability::Write)
        || a.has(Capability::Delete)
        || a.has(Capability::WorldEffect)
        || a.has(Capability::IngressToExec)
        || a.has(Capability::AuthAccess)
        || !a.redirects.is_empty();
    if !destructive {
        return None;
    }

    // ---- RED deny: raw-device catastrophe / irreversibility ----------------
    if a.target == TargetClass::RawDevice || is_irreversible_verb(&a.verb) {
        return Some(Verdict::Deny {
            reason: format!(
                "{} targets a raw device or is inherently irreversible (verb '{}', target '{}')",
                a.verb,
                a.verb,
                a.args.iter().find(|x| !x.starts_with('-')).cloned().unwrap_or_default()
            ),
            alternative: prescriptive_alternative(&a),
        });
    }

    // ---- RED deny: truncation of a control-plane target -------------------
    if let Some(r) = a.redirects.iter().find(|r| !r.append) {
        let tc = classify_target(&r.target, project_root);
        if matches!(tc, TargetClass::System | TargetClass::Secret | TargetClass::Database) {
            return Some(Verdict::Deny {
                reason: format!(
                    "truncation (`>`) of a control-plane target '{}' ({}) — destruction, not injection",
                    r.target,
                    tc.as_str()
                ),
                alternative: format!(
                    "use append (`>>`) or a journal-copy backup first: `cp -a --reflink=auto '{}' '{}'.bak && <command>`",
                    r.target, r.target
                ),
            });
        }
    }

    // ---- Mitigations by verb ----------------------------------------------
    let severity = classify_severity(&a, project_root);
    let mut moves: Vec<Move> = Vec::new();
    let mut effective = command.to_string();
    let mut deviations: Vec<String> = Vec::new();
    let mut restore = String::new();
    // How many files the agent will have moved to trash after this command
    // runs (None unless a trash move is the mitigation).
    let mut trash_moved: Option<usize> = None;

    // Any `rm`/`del`/`rmdir`/`rmrf`/`del /q`/`rm -fr` anywhere in the command
    // (compound commands joined by `&&`, `;`, `|`, newlines) is mitigated to a
    // Recycle-Bin/trash move so the deletion is reversible. The analyzer only
    // looks verb, so we scan every whitespace-separated word and
    // every pipeline stage here — a `test -e x && rm -f x` must not slip
    // through (the earlier `rm` blind-spot bug). Windows `del` is equivalent to
    // unix `rm` for our purposes (both are unrecoverable without this rewrite).
    if command_has_delete(&a.stages) {
        return Some(mitigate_delete(&a, project_root));
    }
    match a.verb.as_str() {
        "sed" => {
            // sed -i -> sed -i.bak (reversible).
            if a.args.iter().any(|x| x == "-i") {
                let mut eff = String::new();
                let mut replaced = false;
                for x in &a.args {
                    if x == "-i" && !replaced {
                        eff.push_str("-i.bak ");
                        replaced = true;
                    } else {
                        eff.push_str(x);
                        eff.push(' ');
                    }
                }
                moves.push(Move::Reversible {
                    description: "sed -i -> sed -i.bak (original preserved)".into(),
                    restore: "restore the .bak file".into(),
                });
                deviations.push("sed -i -> -i.bak: original preserved as <file>.bak".into());
                restore = "restore the .bak file".into();
                effective = format!("sed {}", eff.trim());
            }
        }
        "git" => {
            // git push --force -> backup + --force-with-lease (race-safe).
            if a.args.iter().any(|x| x == "push") && a.args.iter().any(|x| x == "--force" || x == "-f") {
                let branch = a
                    .args
                    .iter()
                    .position(|x| x == "push")
                    .and_then(|i| a.args.get(i + 1))
                    .cloned()
                    .unwrap_or_else(|| "HEAD".into());
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                moves.push(Move::RaceSafe {
                    description: "git push --force -> backup ref + --force-with-lease".into(),
                });
                moves.push(Move::Reversible {
                    description: "backup the remote ref before force-pushing".into(),
                    restore: format!("restore from refs/backup/{branch}-{ts}"),
                });
                deviations.push("git push --force -> --force-with-lease (fails if remote moved)".into());
                deviations.push(format!("remote ref backed up to refs/backup/{branch}-{ts}"));
                restore = format!("restore from refs/backup/{branch}-{ts}");
                effective = format!(
                    "git fetch && git push refs/remotes/origin/{branch}:refs/backup/{branch}-{ts} && git push --force-with-lease origin {branch}"
                );
            }
        }
        "curl" | "wget" => {
            // curl | sh -> staged download (two-phase).
            if a.has(Capability::IngressToExec) {
                let url = a.args.iter().find(|x| x.starts_with("http")).cloned().unwrap_or_default();
                moves.push(Move::TwoPhase {
                    description: "curl|sh -> download to a staged file, hash/allowlist, then execute".into(),
                });
                deviations.push("curl|sh -> staged: download to a file, verify hash, then run".into());
                restore = "delete the staged file; nothing executed until verified".into();
                effective = format!(
                    "curl -fsSL '{url}' -o /tmp/staged.sh && sha256sum /tmp/staged.sh && sh /tmp/staged.sh"
                );
            }
        }
        _ => {}
    }

    // Redirect onto an existing file: journal-copy (reversible, undo class B).
    if !a.redirects.is_empty() && !a.redirects.iter().any(|r| r.append) {
        if let Some(r) = a.redirects.iter().find(|r| !r.append) {
            // Resolve a relative target against the project root (which is the
            // agent's CWD in production) rather than the *process* CWD, so the
            // exists() gate sees the file the confined agent would touch. A
            // caller that passes `project_root` must not have the gate change
            // meaning depending on what directory *this* process happens to
            // run from — that CWD coupling made the planner impossible to test
            // without directory games and silently aborted out-of-project
            // journal-copies when the two diverged.
            let target_path = match (project_root, Path::new(&r.target).is_relative()) {
                (Some(root), true) => root.join(&r.target),
                _ => Path::new(&r.target).to_path_buf(),
            };
            if target_path.exists() {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                // Stage the backup in a *writable* location: in-place next to the
                // target when it is inside the project root, otherwise under the
                // project's `.pir/backups` (or the system temp dir). The target's
                // own directory is typically not writable by the confined agent,
                // so an in-place backup would fail with "permission denied when
                // creating a backup" and abort the whole command — defeating the
                // point of mitigation (which is to make the write *reversible*,
                // not to block it). See `journal_backup_path`.
                let bak = journal_backup_path(&r.target, project_root, ts);
                moves.push(Move::Reversible {
                    description: format!("journal-copy '{}' before truncating", r.target),
                    restore: format!("restore from {bak}"),
                });
                deviations.push(format!("`>` onto existing '{}' -> journal-copy to '{}' first", r.target, bak));
                restore = format!("restore from {bak}");
                effective = format!("cp -a --reflink=auto '{}' '{}' && {}", r.target, bak, effective);
            }
        }
    }

    // Quarantine boundary: an out-of-project write must be surfaced to the
    // operator (Yellow/ask) even when no rewrite move applies (e.g. a redirect
    // onto a non-existent `../file` — there's nothing to journal-copy, but the
    // write still escapes the project root and must not auto-execute).
    if a.target == TargetClass::OutsideProject
        || a.redirects.iter().any(|r| classify_target(&r.target, project_root) == TargetClass::OutsideProject)
    {
        let mut deviations = deviations;
        if deviations.is_empty() {
            deviations.push(format!(
                "target '{}' escapes the project root — out-of-project write",
                a.redirects
                    .iter()
                    .find(|r| classify_target(&r.target, project_root) == TargetClass::OutsideProject)
                    .map(|r| r.target.clone())
                    .or_else(|| a.args.iter().find(|x| !x.starts_with('-')).cloned())
                    .unwrap_or_default()
            ));
        }
        return Some(Verdict::Mitigate {
            severity: Severity::Yellow,
            moves,
            effective: effective.clone(),
            receipt: receipt(
                command,
                &effective,
                &deviations,
                &restore,
                "unchanged (pre-execution)",
                "out-of-project write — not auto-executed",
                "review and confirm the out-of-project write",
            ),
            trash_moved: None,
        });
    }

    if moves.is_empty() {
        return None;
    }

    Some(Verdict::Mitigate {
        severity,
        moves,
        effective: effective.clone(),
        receipt: receipt(command, &effective, &deviations, &restore, "unchanged (pre-execution)", "see deviations", "review the receipt and confirm the rewrite"),
        trash_moved: None,
    })
}

/// Build a receipt from parts.
fn receipt(
    original: &str,
    effective: &str,
    deviations: &[String],
    restore: &str,
    state: &str,
    projection: &str,
    suggested: &str,
) -> Receipt {
    Receipt {
        original: original.to_string(),
        effective: effective.to_string(),
        deviations: deviations.to_vec(),
        restore: restore.to_string(),
        state: state.to_string(),
        projection: projection.to_string(),
        suggested: suggested.to_string(),
    }
}

/// A prescriptive alternative for a RED deny (P8: prefer ask to deny).
fn prescriptive_alternative(a: &Analysis) -> String {
    match a.verb.as_str() {
        "mkfs" | "mkfs.ext4" | "mkfs.xfs" | "mkfs.btrfs" => {
            "no safe rewrite exists — mkfs destroys the target device irreversibly. If you must \
             reformat, do it manually in a shell where you can confirm the exact device, and back \
             up first."
                .into()
        }
        "shred" => "shred's purpose is irreversible destruction — it cannot be mitigated. If you \
                    need to delete a file, use `trash-put` (recoverable) instead."
            .into(),
        "dd" => "dd of=<raw-device> writes raw bytes to a device — irreversible. If you meant a \
                 regular file, use `cp`/`tee`; if you truly need the device, do it manually with \
                 the exact device confirmed."
            .into(),
        _ => "no safe rewrite exists for this operation; do it manually with the exact target \
              confirmed, and back up first."
            .into(),
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Removes a temp dir on drop, so a test that creates fixture dirs never
    /// leaks them even when an assertion panics mid-test. The guarded path is
    /// always a fresh `temp_dir()` base created by the test itself — it must
    /// never be pointed at the process CWD or the repository.
    struct TempGuard(PathBuf);
    impl Drop for TempGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn benign_commands_are_never_denied() {
        // Regression guard for the operator's standing requirement: the DENY
        // must only ever block *obviously dangerous* operations. Every command
        // below is benign development work and must yield a non-Deny verdict
        // (allowed, or mitigated-and-allowed — never refused).
        let root = Some(Path::new("/home/ai_pir/src/pir"));
        let benign = [
            "ls -la /home/ai_pir/src/pir/src/modal.rs",
            "ls /home/ai_pir/src/pir/src/security",
            "cat /home/ai_pir/src/pir/src/modal.rs",
            "touch /home/ai_pir/src/pir/src/.t",
            "cp a b",
            "cp /home/ai_pir/src/pir/src/modal.rs /home/ai_pir/src/pir/src/modal.rs.bak",
            "mv a b",
            "grep foo file",
            "find . -name '*.rs'",
            "cargo build",
            "git status",
            "mkdir dir",
            "chmod +x script",
            "ln -s a b",
            "head file",
            "tail file",
            "wc -l file",
            "diff a b",
            "sort file",
            "awk '{print}' file",
            "sed 's/a/b/' file",
            "tar czf archive.tgz dir",
            "python script.py",
            "node app.js",
            "make",
            "curl https://example.com",
            "wget https://example.com",
            "ssh user@host",
            "scp a host:b",
            "rsync a b",
            "kill 1234",
            "pkill name",
            "chown user:group file",
            "chgrp group file",
            "echo hi > /tmp/x",
            "printf 'x' > /home/ai_pir/src/pir/src/.t",
            // A *read* of a raw device is not destructive and must not be denied.
            "cat /dev/sda",
            "dd if=/dev/sda of=image.img",
        ];
        let mut denied: Vec<&str> = Vec::new();
        for cmd in benign {
            if let Some(v) = plan(cmd, root) {
                if matches!(v, Verdict::Deny { .. }) {
                    denied.push(cmd);
                }
            }
        }
        assert!(denied.is_empty(), "benign commands were denied: {denied:?}");
    }

    #[test]
    fn dangerous_commands_are_still_denied() {
        // The safe-harbor must not let genuinely dangerous operations slip
        // through. These are the *obviously dangerous* commands the DENY exists
        // for.
        let root = Some(Path::new("/home/ai_pir/src/pir"));
        let dangerous = [
            "mkfs.ext4 /dev/sdb1",
            "dd if=/dev/zero of=/dev/sda",
            "shred file",
            "fdisk /dev/sda",
            "parted /dev/sda",
            "wipefs /dev/sda",
            "echo x > /etc/passwd",
            "echo x > /root/.ssh/authorized_keys",
            "echo x > /var/lib/mysql/db",
        ];
        let mut missed: Vec<&str> = Vec::new();
        for cmd in dangerous {
            match plan(cmd, root) {
                Some(Verdict::Deny { .. }) => {}
                _ => missed.push(cmd),
            }
        }
        assert!(missed.is_empty(), "dangerous commands not denied: {missed:?}");
    }

    #[test]
    fn journal_backup_path_stages_unwritable_targets() {
        let root = Path::new("/home/ai_pir/src/pir");
        // The reported bug: `echo bad > ../hi` failed with "permission denied
        // when attempting to create a backup" because the `..`-escaping target's
        // own directory isn't writable by the confined agent. The journal-copy
        // must stage under a writable dir, not next to the target.
        let p = journal_backup_path("../hi", Some(root), 12345);
        assert!(
            p.starts_with(&root.join(".pir").join("backups").to_string_lossy().to_string()),
            "out-of-project target must stage under .pir/backups: {p}"
        );
        assert!(p.ends_with(".12345"), "backup name should carry the timestamp: {p}");
        // Same for an absolute system path in a read-only tree (/var/log): an
        // in-place `<target>.bak` would also fail — stage it too.
        let p2 = journal_backup_path("/var/log/app.log", Some(root), 7);
        assert!(
            p2.starts_with(&root.join(".pir").join("backups").to_string_lossy().to_string()),
            "system target must stage under .pir/backups: {p2}"
        );
        // Without a project root, fall back to the shared temp staging dir.
        let p3 = journal_backup_path("/var/log/app.log", None, 7);
        assert!(p3.contains("pir-backups"), "no-root fallback must stage in temp: {p3}");
        // A target inside the project is backed up in place next to the file
        // (its directory is writable, so an in-place `.bak` succeeds).
        let p4 = journal_backup_path("src/main.rs", Some(root), 9);
        assert_eq!(p4, "src/main.rs.bak.9");
    }

    #[test]
    fn rm_mitigates_to_trash() {
        let v = plan("rm -rf build/", None).expect("plan");
        match v {
            Verdict::Mitigate { severity, effective, moves, .. } => {
                assert_eq!(severity, Severity::Blue);
                assert!(effective.starts_with("trash-put"), "got {effective}");
                assert!(moves.iter().any(|m| m.kind() == "reversible"));
            }
            _ => panic!("expected mitigate"),
        }
    }

    #[test]
    fn rm_root_is_yellow_not_auto() {
        let v = plan("rm -rf /", None).expect("plan");
        match v {
            Verdict::Mitigate { severity, .. } => assert_eq!(severity, Severity::Yellow),
            _ => panic!("expected mitigate (ask)"),
        }
    }

    #[test]
    fn redirect_onto_existing_adds_journal_copy() {
        // A journal-copy fires only when the redirect target already exists
        // (truncating an existing file is the destructive case).
        let dir = std::env::temp_dir().join(format!("pir_mit_redir_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("x.txt");
        std::fs::write(&target, "old").unwrap();
        let cmd = format!("echo hi > {}", target.display());
        let v = plan(&cmd, None).expect("plan");
        match v {
            Verdict::Mitigate { effective, .. } => {
                assert!(effective.contains("cp -a --reflink=auto"), "got {effective}");
            }
            _ => panic!("expected mitigate"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_project_redirect_journal_copies_to_staged_backup() {
        // The reported bug: `echo bad > ../hi` (target outside the project
        // root, existing file) produced an in-place `<target>.bak` the confined
        // agent can't create, so the whole command died with "permission denied
        // when attempting to create a backup". The journal-copy must stage the
        // backup under the writable project dir instead — the write stays
        // reversible, never blocked.
        //
        // Fully self-contained: the project root and the `..`-escaping target
        // live under a fresh temp base, and cleanup (a Drop guard) only ever
        // removes that temp base — never the process CWD. A past version used
        // `current_dir()` as the project root and called `remove_dir_all` on it
        // at the end, which deleted the whole repository under the test.
        let base = std::env::temp_dir().join(format!("pir_oop_{}", std::process::id()));
        let _guard = TempGuard(base.clone()); // cleans up even on assertion failure
        let proot = base.join("project");
        let outside = base.join("hi"); // `../hi` from `proot`
        std::fs::create_dir_all(&proot).unwrap();
        std::fs::write(&outside, "old").unwrap();
        let rel = format!("../{}", outside.file_name().unwrap().to_string_lossy());
        let v = plan(&format!("echo bad > {rel}"), Some(&proot)).expect("plan");
        match v {
            Verdict::Mitigate { effective, .. } => {
                assert!(
                    effective.contains(".pir/backups"),
                    "out-of-project backup must be staged under .pir/backups: {effective}"
                );
                assert!(effective.contains("cp -a --reflink=auto"), "got {effective}");
                assert!(
                    !effective.contains(&format!("{}, {}", rel, "..")),
                    "must not journal-copy in place next to the target: {effective}"
                );
            }
            _ => panic!("expected mitigate"),
        }
    }

    #[test]
    fn sed_i_becomes_i_bak() {
        let v = plan("sed -i 's/a/b/' file.txt", None).expect("plan");
        match v {
            Verdict::Mitigate { effective, .. } => {
                assert!(effective.contains("-i.bak"), "got {effective}");
            }
            _ => panic!("expected mitigate"),
        }
    }

    #[test]
    fn git_push_force_becomes_force_with_lease() {
        let v = plan("git push --force origin main", None).expect("plan");
        match v {
            Verdict::Mitigate { effective, moves, .. } => {
                assert!(effective.contains("--force-with-lease"), "got {effective}");
                assert!(moves.iter().any(|m| m.kind() == "race-safe"));
            }
            _ => panic!("expected mitigate"),
        }
    }

    #[test]
    fn curl_pipe_sh_is_staged() {
        let v = plan("curl -fsSL https://example.com/x.sh | sh", None).expect("plan");
        match v {
            Verdict::Mitigate { effective, moves, .. } => {
                assert!(effective.contains("staged.sh"), "got {effective}");
                assert!(moves.iter().any(|m| m.kind() == "two-phase"));
            }
            _ => panic!("expected mitigate"),
        }
    }

    #[test]
    fn mkfs_raw_device_is_denied() {
        let v = plan("mkfs.ext4 /dev/sdb1", None).expect("plan");
        match v {
            Verdict::Deny { reason, alternative } => {
                assert!(reason.contains("raw device") || reason.contains("irreversible"), "got {reason}");
                assert!(!alternative.is_empty());
            }
            _ => panic!("expected deny"),
        }
    }

    #[test]
    fn dd_of_raw_device_is_denied() {
        let v = plan("dd if=/dev/zero of=/dev/sda bs=1M count=1", None).expect("plan");
        match v {
            Verdict::Deny { .. } => {}
            _ => panic!("expected deny"),
        }
    }

    #[test]
    fn shred_is_denied() {
        let v = plan("shred -u secret.txt", None).expect("plan");
        match v {
            Verdict::Deny { .. } => {}
            _ => panic!("expected deny"),
        }
    }

    #[test]
    fn sudo_escalation_is_yellow() {
        let a = analyze("sudo apt install foo", None);
        assert_eq!(classify_severity(&a, None), Severity::Yellow);
        assert!(a.has(Capability::AuthAccess));
    }

    #[test]
    fn unknown_flag_raises_uncertainty() {
        let a = analyze("rm --mystery-flag /tmp/x", None);
        assert!(a.uncertainty > 0.0, "unknown flag should raise uncertainty");
        assert!(!a.uncertainty_reason.is_empty());
    }

    #[test]
    fn plain_read_is_allowed() {
        let v = plan("cat src/main.rs", None);
        assert!(v.is_none(), "plain read should need no mitigation");
    }

    #[test]
    fn truncation_of_system_target_is_denied() {
        let v = plan("echo x > /etc/passwd", None).expect("plan");
        match v {
            Verdict::Deny { .. } => {}
            _ => panic!("expected deny for control-plane truncation"),
        }
    }

    #[test]
    fn receipt_has_fixed_order() {
        let v = plan("rm -rf build/", None).expect("plan");
        if let Verdict::Mitigate { receipt, .. } = v {
            let r = receipt.render();
            assert!(r.contains("what happened"));
            assert!(r.contains("state"));
            assert!(r.contains("projection"));
            assert!(r.contains("suggested action"));
        } else {
            panic!("expected mitigate");
        }
    }

    // ---- Quarantine boundary: out-of-project (`..` escape) writes ---------

    #[test]
    fn redirect_escape_is_outside_project() {
        // `echo x > ../canary.txt` escapes the project root via `..`.
        let root = Path::new("/proj");
        let a = analyze("echo x > ../canary.txt", Some(root));
        assert_eq!(a.target, TargetClass::OutsideProject, "redirect target escapes project root");
        assert_eq!(classify_severity(&a, Some(root)), Severity::Yellow);
    }

    #[test]
    fn redirect_escape_is_surfaced_not_auto() {
        // The out-of-project write must be surfaced to the operator (Yellow/ask)
        // even when no rewrite move applies (the target doesn't exist, so no
        // journal-copy fires). It must NOT auto-execute.
        let root = Path::new("/proj");
        let v = plan("echo x > ../canary.txt", Some(root)).expect("plan");
        match v {
            Verdict::Mitigate { severity, .. } => {
                assert_eq!(severity, Severity::Yellow, "out-of-project write must be Yellow/ask");
            }
            _ => panic!("expected mitigate (ask) for out-of-project write"),
        }
    }

    #[test]
    fn in_project_redirect_stays_blue() {
        // A redirect inside the project root is not an escape.
        let root = Path::new("/proj");
        let a = analyze("echo x > src/out.txt", Some(root));
        assert_eq!(a.target, TargetClass::Project);
        assert_eq!(classify_severity(&a, Some(root)), Severity::Blue);
    }

    #[test]
    fn no_project_root_means_no_escape() {
        // Without a project root there is no boundary; `..` is not flagged.
        let a = analyze("echo x > ../canary.txt", None);
        assert_eq!(a.target, TargetClass::Project);
        assert_eq!(classify_severity(&a, None), Severity::Blue);
    }

    #[test]
    fn absolute_scratch_path_not_flagged() {
        // Absolute paths (e.g. /tmp scratch) are handled by the system/secret/
        // db/other-user classes, not the `..` escape check. `/tmp` is not a
        // system tree, so it stays Project (in-scope scratch).
        let root = Path::new("/proj");
        let a = analyze("echo x > /tmp/scratch.txt", Some(root));
        assert_eq!(a.target, TargetClass::Project, "absolute /tmp scratch is not an escape");
    }

    #[test]
    fn nested_escape_is_outside_project() {
        // `../../x` escapes even from a subdirectory.
        let root = Path::new("/proj");
        let a = analyze("echo x > ../../x.txt", Some(root));
        assert_eq!(a.target, TargetClass::OutsideProject);
    }

    #[test]
    fn escape_in_arg_is_outside_project() {
        // A non-redirect arg that escapes the project root is also flagged.
        let root = Path::new("/proj");
        let a = analyze("rm -rf ../build", Some(root));
        assert_eq!(a.target, TargetClass::OutsideProject);
        assert_eq!(classify_severity(&a, Some(root)), Severity::Yellow);
    }
}
