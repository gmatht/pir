//! Unix backend for the security module.
//!
//! The genuinely platform-specific bits live here: the `Platform` impl (path
//! heuristics the cross-platform core delegates to) and the headless request
//! queue (`queue_perm_request`), which surfaces a deferred denial to the
//! operator out-of-band via the `permctl`/`ai-perm-request` channel so a full
//! auto `ai_*` agent never blocks on a TTY prompt it can't answer.

use crate::security::{Denial, Platform};
use std::path::Path;
use std::process::Command;

/// The unix `Platform` impl. Currently a thin wrapper over the shared core
/// heuristics; it exists so the `Platform` trait has a concrete unix backend
/// and is the seam where host-specific interception (Landlock, auditd watch
/// wiring, mount-namespace checks) would be attached.
#[derive(Default)]
pub struct UnixPlatform;

impl Platform for UnixPlatform {
    fn is_system_state(&self, p: &Path) -> bool {
        super::is_system_state(p)
    }
    fn is_other_users(&self, p: &Path) -> bool {
        super::is_other_users(p)
    }
}

/// Surface a deferred denial from a headless (full-auto `ai_*`) agent. We log a
/// structured line and, when `ai-perm-request` is on PATH, enqueue it into the
/// operator's request queue so it can be reviewed out-of-band. Best-effort: any
/// failure is only logged, never fatal — the agent must keep working.
pub fn queue_perm_request(d: &Denial) {
    let what = d
        .ask
        .path
        .as_ref()
        .map(|p| p.display().to_string())
        .or_else(|| d.ask.target.clone())
        .unwrap_or_else(|| d.ask.op.verb().to_string());
    eprintln!(
        "[pir] security request (deferred, headless): {} {} -> parcel {} (risk {})",
        d.ask.op.verb(),
        what,
        d.parcel.id(),
        d.risk.as_str(),
    );
    // If the operator-side enqueuer exists, raise a queued request so a human
    // can approve/deny it later (the `permctl` channel from SKYNET-AI-PERMS).
    let reason = if d.ask.reason.is_empty() {
        format!("agent needs {} on {}", d.ask.op.verb(), what)
    } else {
        d.ask.reason.clone()
    };
    let _ = Command::new("ai-perm-request")
        .args(["ask", &d.ask.op.verb(), &what, "--reason", &reason])
        .status();
}
