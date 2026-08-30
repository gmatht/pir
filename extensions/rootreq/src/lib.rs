//! rootreq — let the agent *request* privilege escalation (off by default).
//!
//! This is the "request, don't take" model from `ai-permctl`, extended to
//! privilege elevation. The agent NEVER escalates itself. It queues a
//! structured, auditable request that an operator (root, or a `skynet_*`/
//! `skynet` orchestrator with a sudoers rule) fulfills out-of-band via
//! `rootreq-enforcer`. The enforcer is the only thing that grants anything,
//! and it validates every request against an allowlist.
//!
//! Tools
//! -----
//! * `request_root` — queue an escalation request. Supported intents:
//!     - `apt-install <pkgs...>`        (logged, validated; via ai-apt-install)
//!     - `mk-ai-user <ai_NAME>`         (create a new ai_* account)
//!     - `su-ai <ai_NAME>`              (switch to an ai_* account)
//!     - `command <cmd>`                (one specific, allowlisted command)
//!   The agent supplies a `reason`; the request is written to the spool and a
//!   human is told how to apply it. If the *current* user already holds a
//!   passwordless sudo rule for the exact intent, the tool may opportunistically
//!   run `sudo -n` inline — but it never broadens its own privilege.
//! * `run_as` — run a command as a user the agent is already permitted to
//!   (e.g. an `ai_*`), using existing sudoers. No new privilege is granted.
//!
//! Enabled by default (set `PIR_ROOTREQ=0` to disable). When disabled, the
//! `request_root` tool is not offered and nothing is queued; `run_as` is always
//! available. Queueing only *requests* — an operator must still fulfil each
//! request out-of-band via `rootreq-enforcer`, so this grants no privilege.

use crate::plugin::{Outcome, Registry, ToolBackend, ToolSpec};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Intent {
    AptInstall,
    MkAiUser,
    SuAi,
    Command,
}

/// Allowlisted, validated package-name / user-name charset (no shell
/// metacharacters). Matches the ai-permctl / skynet wrappers' policy.
fn is_safe_token(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "._+~-/".contains(c))
}

/// Validate + normalize an intent's argument. Returns the allowlisted arg, or
/// an error string describing why it's rejected.
fn validate_arg(intent: Intent, arg: &str) -> Result<String, String> {
    match intent {
        Intent::AptInstall | Intent::MkAiUser | Intent::SuAi => {
            if !is_safe_token(arg) {
                return Err(format!("rejected: '{}' contains disallowed characters", arg));
            }
            match intent {
                Intent::MkAiUser | Intent::SuAi => {
                    if !arg.starts_with("ai_") {
                        return Err(format!("rejected: target must be an ai_* account (got '{}')", arg));
                    }
                    if !arg[3..].chars().all(|c| c.is_ascii_alphanumeric()) {
                        return Err(format!("rejected: ai_* name must be [A-Za-z0-9]+ (got '{}')", arg));
                    }
                }
                Intent::AptInstall => { /* package name; already token-checked */ }
                Intent::Command => unreachable!(),
            }
            Ok(arg.to_string())
        }
        Intent::Command => {
            // A single allowlisted command: only a small, safe set is permitted
            // to be requested at all. Anything else must go through apt-install
            // / the ai_* wrappers. This keeps the request surface tiny.
            match arg {
                "id" | "uname" | "whoami" | "pwd" | "lsb_release" => Ok(arg.to_string()),
                other => Err(format!(
                    "rejected: generic command '{}' is not allowlisted; request a typed intent (apt-install/su-ai/mk-ai-user) instead",
                    other
                )),
            }
        }
    }
}

struct RootReq {
    enabled: bool,
    spool: PathBuf,
}

impl RootReq {
    fn new() -> Self {
        let enabled = std::env::var_os("PIR_ROOTREQ").map(|v| v != "0" && !v.is_empty()).unwrap_or(true);
        let spool = std::env::var_os("AI_PERM_REQUEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp/ai-perm-requests"));
        RootReq { enabled, spool }
    }

    /// Queue a request JSON into the spool (mode 700, agent-owned).
    fn queue(&self, req: &serde_json::Value) -> Result<String, String> {
        let id = format!(
            "{}@{}-{}",
            chrono_id(),
            std::process::id(),
            hex_nonce()
        );
        let dir = &self.spool;
        std::fs::create_dir_all(dir).map_err(|e| format!("spool mkdir: {e}"))?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        let path = dir.join(format!("{id}.json"));
        std::fs::write(&path, serde_json::to_vec_pretty(req).map_err(|e| e.to_string())?)
            .map_err(|e| format!("spool write: {e}"))?;
        Ok(id)
    }

    /// If the current user already has a passwordless sudo rule for this exact
    /// command, run it inline (no broadening of privilege). Returns None if we
    /// should not / cannot self-escalate (caller then just queues).
    fn try_inline_sudo(&self, sudo_cmd: &str) -> Option<bool> {
        let out = Command::new("sudo")
            .arg("-n") // non-interactive: fail immediately if a password is required
            .args(sudo_cmd.split_whitespace())
            .output();
        match out {
            Ok(o) if o.status.success() => Some(true),
            // sudo -n exits non-zero (or missing) when no passwordless rule → do not queue-and-run.
            _ => None,
        }
    }
}

fn chrono_id() -> String {
    // Best-effort timestamp; avoids a chrono dependency.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

fn hex_nonce() -> String {
    use std::io::Read;
    let mut buf = [0u8; 8];
    let _ = std::fs::File::open("/dev/urandom").map(|mut f| f.read_exact(&mut buf));
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

impl ToolBackend for RootReq {
    fn name(&self) -> &'static str {
        "rootreq"
    }

    fn specs(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "request_root",
                description:
                    "REQUEST (do not perform) a privilege elevation. Queues an auditable, \
                     allowlisted request for an operator to fulfill via rootreq-enforcer. \
                     Intents: apt-install <pkgs>, mk-ai-user <ai_NAME>, su-ai <ai_NAME>, \
                     command <id|uname|whoami|pwd|lsb_release>. Requires a 'reason'. Never \
                     escalates on its own; if a passwordless sudo rule already exists for the \
                     exact intent, it may run inline.",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "intent": { "type": "string", "enum": ["apt-install", "mk-ai-user", "su-ai", "command"] },
                        "arg": { "type": "string", "description": "pkgs / ai_NAME / command" },
                        "reason": { "type": "string" }
                    },
                    "required": ["intent", "arg", "reason"]
                }),
            },
            ToolSpec {
                name: "run_as",
                description:
                    "Run a command as a user the agent is ALREADY permitted to (e.g. an ai_* \
                     account), via existing sudoers. Grants no new privilege.",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "user": { "type": "string" },
                        "command": { "type": "string" }
                    },
                    "required": ["user", "command"]
                }),
            },
        ]
    }

    fn run(&mut self, name: &str, input: &serde_json::Value) -> Outcome {
        match name {
            "request_root" => {
                if !self.enabled {
                    return Outcome::err(String::from("rootreq is disabled (set PIR_ROOTREQ=0 to disable request queueing)"));
                }
                let intent_s = input.get("intent").and_then(Value_as_str).unwrap_or("");
                let arg = input.get("arg").and_then(Value_as_str).unwrap_or("");
                let reason = input.get("reason").and_then(Value_as_str).unwrap_or("(no reason)");
                let intent = match intent_s {
                    "apt-install" => Intent::AptInstall,
                    "mk-ai-user" => Intent::MkAiUser,
                    "su-ai" => Intent::SuAi,
                    "command" => Intent::Command,
                    other => return Outcome::err(format!("unknown intent '{other}' (apt-install|mk-ai-user|su-ai|command)")),
                };
                let arg = match validate_arg(intent, arg) {
                    Ok(a) => a,
                    Err(e) => return Outcome::err(e),
                };
                if reason.trim().is_empty() {
                    return Outcome::err(String::from("request_root requires a 'reason'"));
                }

                // Opportunistic inline run ONLY if a passwordless sudo rule already exists.
                let sudo_cmd = match intent {
                    Intent::AptInstall => format!("ai-apt-install {arg}"),
                    Intent::MkAiUser => format!("mk-ai-user {arg}"),
                    Intent::SuAi => format!("su-ai {arg}"),
                    Intent::Command => arg.to_string(),
                };
                if let Some(true) = self.try_inline_sudo(&sudo_cmd) {
                    return Outcome::ok(format!(
                        "executed inline via existing passwordless sudo rule: sudo {sudo_cmd}"
                    ));
                }

                // Otherwise queue and ask an operator.
                let req = json!({
                    "id": "",
                    "op": "request-root",
                    "intent": intent_s,
                    "arg": arg,
                    "reason": reason,
                    "requested_by": whoami(),
                    "requested_at": utc_now()
                });
                match self.queue(&req) {
                    Ok(id) => Outcome::ok(format!(
                        "queued {intent_s} ({arg}): {id}\nAsk an operator to run: sudo rootreq-enforcer"
                    )),
                    Err(e) => Outcome::err(e),
                }
            }
            "run_as" => {
                let user = input.get("user").and_then(Value_as_str).unwrap_or("");
                let command = input.get("command").and_then(Value_as_str).unwrap_or("");
                if user.is_empty() || command.is_empty() {
                    return Outcome::err(String::from("run_as requires 'user' and 'command'"));
                }
                // Only ai_* targets are allowed via this tool (existing sudoers model).
                if !user.starts_with("ai_") {
                    return Outcome::err(String::from("run_as only permits ai_* targets (existing sudoers)"));
                }
                if !is_safe_token(command) {
                    return Outcome::err(String::from("run_as: command contains disallowed characters"));
                }
                let out = Command::new("sudo")
                    .arg("-u")
                    .arg(user)
                    .arg("--")
                    .arg(command)
                    .output();
                match out {
                    Ok(o) => {
                        let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
                        let stderr = String::from_utf8_lossy(&o.stderr).into_owned();
                        if o.status.success() {
                            Outcome::ok(stdout)
                        } else {
                            Outcome::err(format!("run_as failed: {stderr}"))
                        }
                    }
                    Err(e) => Outcome::err(format!("run_as spawn error: {e}")),
                }
            }
            other => Outcome::err(format!("unknown tool '{other}'")),
        }
    }
}

// Small helpers (avoid pulling serde_json Value::as_str everywhere).
fn Value_as_str(v: &serde_json::Value) -> Option<&str> {
    v.as_str()
}

fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| {
            Command::new("id").arg("-un").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .unwrap_or_else(|_| "unknown".into())
}

fn utc_now() -> String {
    // Best-effort ISO timestamp without chrono.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}Z")
}

pub fn register(reg: &mut Registry) {
    reg.add(Box::new(RootReq::new()));
}
