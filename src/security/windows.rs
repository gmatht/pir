//! Windows backend for the security module (structural stub).
//!
//! The cross-platform core compiles on Windows; the platform-specific bits
//! that would live here — the `AppContainer` ACL plumbing, ProjFS union layer,
//! and the ETW/Sysmon denial capture described in `docs/SECURITY_MODEL.md`
//! §2/§7 — are not implemented in this build. We provide a default `Platform`
//! so the abstraction is real and `pir` compiles, but host-level confinement on
//! Windows is currently a no-op beyond the shared write/secret guardrail.

use crate::security::Platform;
use std::path::Path;

/// The Windows `Platform` impl. Without the AppContainer plumbing this matches
/// the generic behaviour; it is the seam where `CreateAppContainerProfile` /
/// `GrantAcl` / ProjFS callbacks would attach.
#[derive(Default)]
pub struct WindowsPlatform;

impl Platform for WindowsPlatform {
    fn is_other_users(&self, p: &Path) -> bool {
        super::is_other_users(p)
    }
    fn is_system_state(&self, p: &Path) -> bool {
        super::is_system_state(p)
    }
}

/// Surface a deferred denial from a headless agent. The Windows operator-side
/// enqueuer (`perm-enforcer`/Settings->`ai-perm-request`) is not wired in this
/// build; we log a structured line so the request is at least visible.
pub fn queue_perm_request(d: &crate::security::Denial) {
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
}
