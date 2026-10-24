//! Native Rust equivalent of the operator's opencode key-rotation flow.
//!
//! The operator keeps several opencode keys in `~/.pi/agent/opencode-keys.json`
//! (see [`crate::config::KeyPool`] for the shape: `keys`, `activeKeyIndex`,
//! `cooldownMinutes`, `cooldowns`, `quotaBlockedUntil`). This backend is the
//! operator-facing surface for that pool:
//!
//!   * the `opencode-go` provider itself is pointed at the pool's current
//!     usable key at catalog load ([`crate::config::merge_opencode_keys`]) and
//!     every request sends with the pool's current key, with automatic
//!     failover to the next usable key on quota/auth/rate-limit failures
//!     (see `Client::key_pool_key` / `rotate_pool` in `crate::provider`);
//!   * this extension contributes the `opencode_key_status` /
//!     `opencode_key_rotate` tools the model can call when it notices a key
//!     going bad, and the `/opencode-keys` slash command for the operator
//!     (`status` default, `rotate`, `status` explicit);
//!   * the startup report names the active key and the usable count, so a
//!     rotation (or an exhausted pool) is visible before the first prompt.

use crate::config::{load_opencode_key_pool, opencode_keys_path};
use crate::plugin::{CommandSpec, Outcome, Registry, ToolBackend, ToolSpec};
use serde_json::{json, Value};

pub fn register(reg: &mut Registry) {
    reg.add(Box::new(OpencodeKeys));
}

struct OpencodeKeys;

fn no_pool() -> String {
    format!(
        "no opencode key pool: {} is absent or holds no keys — the opencode-go provider uses its static catalog key",
        opencode_keys_path().display()
    )
}

impl ToolBackend for OpencodeKeys {
    fn name(&self) -> &'static str {
        "opencode-keys"
    }

    fn specs(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "opencode_key_status",
                description: "Show the opencode key pool status (active key, usable/cooldown/quota-blocked per key). Reads ~/.pi/agent/opencode-keys.json.",
                schema: json!({ "type": "object", "properties": {}, "required": [] }),
            },
            ToolSpec {
                name: "opencode_key_rotate",
                description: "Rotate the opencode key pool to the next usable key (updates activeKeyIndex in ~/.pi/agent/opencode-keys.json). Takes no arguments. Automatic failover already handles quota/auth/rate-limit failures; call this to move off a key preemptively.",
                schema: json!({ "type": "object", "properties": {}, "required": [] }),
            },
        ]
    }

    fn run(&mut self, name: &str, _input: &Value) -> Outcome {
        match name {
            "opencode_key_status" => match load_opencode_key_pool() {
                Some(pool) => Outcome::ok(pool.status()),
                None => Outcome::err(no_pool()),
            },
            "opencode_key_rotate" => match load_opencode_key_pool() {
                Some(mut pool) => match pool.rotate_manual("tool-requested") {
                    Some((idx, name)) => Outcome::ok(format!(
                        "rotated opencode key pool: active is now {name} (index {idx})"
                    )),
                    None => Outcome::err(
                        "no other usable key to rotate to (pool exhausted or single usable key)"
                            .into(),
                    ),
                },
                None => Outcome::err(no_pool()),
            },
            other => Outcome::err(format!("unknown tool '{other}'")),
        }
    }

    fn commands(&self) -> Vec<CommandSpec> {
        vec![CommandSpec {
            name: "opencode-keys".into(),
            description: "Show or rotate the opencode key pool (~/.pi/agent/opencode-keys.json). Usage: /opencode-keys [status|rotate].".into(),
        }]
    }

    fn run_command(&mut self, name: &str, args: &str) -> Outcome {
        if name != "opencode-keys" {
            return Outcome::err(format!("unknown command '{name}'"));
        }
        match args.trim().to_lowercase().as_str() {
            "" | "status" => match load_opencode_key_pool() {
                Some(pool) => Outcome::ok(pool.status()),
                None => Outcome::err(no_pool()),
            },
            "rotate" => match load_opencode_key_pool() {
                Some(mut pool) => match pool.rotate_manual("operator-requested") {
                    Some((idx, key_name)) => Outcome::ok(format!(
                        "rotated opencode key pool: active is now {key_name} (index {idx})"
                    )),
                    None => Outcome::err(
                        "no other usable key to rotate to (pool exhausted or single usable key)"
                            .into(),
                    ),
                },
                None => Outcome::err(no_pool()),
            },
            other => Outcome::err(format!(
                "unknown /opencode-keys subcommand '{other}' — try /opencode-keys [status|rotate]"
            )),
        }
    }

    fn startup_report(&mut self) -> Option<String> {
        let pool = load_opencode_key_pool()?;
        let usable = pool.usable_indices_count();
        Some(format!(
            "[opencode-keys] pool: active {} — {usable} of {} usable (/opencode-keys status|rotate)",
            pool.key_name(pool.active_index()),
            pool.len(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Isolated pool file under a temp `PI_DIR` (the shared env lock guards the
    /// process-global swap; every assertion restores).
    struct PoolDir {
        dir: std::path::PathBuf,
        old: Option<std::ffi::OsString>,
        // Held for the whole test: dropping it at the end of `new()` would
        // release the process-global env lock while the test body still
        // depends on `PI_DIR`, letting a parallel env-mutating test interleave.
        _env: std::sync::MutexGuard<'static, ()>,
    }

    impl PoolDir {
        fn new(doc: &Value) -> Self {
            let env = crate::test_support::env_lock();
            let dir = std::env::temp_dir().join(format!(
                "pir-opencode-keys-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
            ));
            std::fs::create_dir_all(dir.join("agent")).unwrap();
            std::fs::write(dir.join("agent").join("opencode-keys.json"), doc.to_string()).unwrap();
            let old = std::env::var_os("PI_DIR");
            // SAFETY: edition 2024 marks env mutation unsafe; tests hold the
            // shared env lock and restore on drop (see `Drop` below).
            unsafe { std::env::set_var("PI_DIR", &dir) };
            PoolDir { dir, old, _env: env }
        }
    }

    impl Drop for PoolDir {
        fn drop(&mut self) {
            match self.old.take() {
                Some(v) => unsafe { std::env::set_var("PI_DIR", v) },
                None => unsafe { std::env::remove_var("PI_DIR") },
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn two_key_doc() -> Value {
        json!({
            "keys": [
                { "name": "a", "key": "sk-a" },
                { "name": "b", "key": "sk-b" },
            ],
            "activeKeyIndex": 0,
            "cooldownMinutes": 60,
        })
    }

    #[test]
    fn status_reports_active_and_usable() {
        let _pool = PoolDir::new(&two_key_doc());
        let mut ext = OpencodeKeys;
        let out = ext.run("opencode_key_status", &json!({}));
        assert!(!out.is_error, "status must succeed: {}", out.content);
        assert!(out.content.contains("#0 (a)"), "active key named: {}", out.content);
        assert!(out.content.contains("2 usable"), "both usable: {}", out.content);
    }

    #[test]
    fn rotate_advances_active_and_persists() {
        let _pool = PoolDir::new(&two_key_doc());
        let mut ext = OpencodeKeys;
        let out = ext.run("opencode_key_rotate", &json!({}));
        assert!(!out.is_error, "rotate must succeed: {}", out.content);
        assert!(out.content.contains("#1 (b)"), "moved to b: {}", out.content);
        // Reloaded from disk (the rotation persisted).
        let pool = load_opencode_key_pool().expect("pool reloads");
        assert_eq!(pool.active_index(), 1);
    }

    #[test]
    fn rotate_without_pool_is_a_clear_error() {
        let _pool = PoolDir::new(&json!({ "keys": [] }));
        let mut ext = OpencodeKeys;
        let out = ext.run("opencode_key_status", &json!({}));
        assert!(out.is_error);
        assert!(out.content.contains("no opencode key pool"), "got: {}", out.content);
    }

    #[test]
    fn slash_command_status_and_rotate() {
        let _pool = PoolDir::new(&two_key_doc());
        let mut ext = OpencodeKeys;
        let s = ext.run_command("opencode-keys", "");
        assert!(!s.is_error, "bare status: {}", s.content);
        let r = ext.run_command("opencode-keys", "rotate");
        assert!(!r.is_error, "rotate: {}", r.content);
        assert!(r.content.contains("#1 (b)"), "got: {}", r.content);
        let bad = ext.run_command("opencode-keys", "bogus");
        assert!(bad.is_error);
    }
}
