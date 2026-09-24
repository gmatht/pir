//! MCP extension: exposes tools from stdio MCP servers to the model.
//!
//! Reads `~/.pi/agent/mcp.json`, starts each server declared there, discovers
//! its tools, and re-publishes them as ordinary `ToolBackend` tools named
//! `mcp__<server>__<tool>`. The model sees them alongside `bash`/`read_file`
//! and can call them; the security guardrail treats them like any other tool.
//!
//! ```json
//! {
//!   "servers": {
//!     "filesystem": {
//!       "command": "npx",
//!       "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
//!     }
//!   }
//! }
//! ```
//!
//! # Failure policy
//!
//! Starting an MCP server is *untrusted work*: it may not be installed, it may
//! crash on startup, it may hang on its handshake. None of that may prevent pir
//! from running, so:
//!
//! * a server that fails to start is reported once, in the startup banner, and
//!   then ignored for the session — its tools simply don't exist;
//! * a server that dies later is not respawned mid-session (a crash loop would
//!   burn a turn each time); calls against it fail with a clear message;
//! * nothing here ever blocks on a pipe without a timeout.
//!
//! Set `PIR_MCP=0` to disable the whole thing — no process is spawned at all.

use crate::mcp::{self, Client};
use crate::plugin::{Outcome, Registry, ToolBackend, ToolSpec};
use serde_json::Value;

/// One live server and the tools it offers.
struct Server {
    client: Client,
    tools: Vec<mcp::McpTool>,
}

/// What the background thread produces.
struct Connected {
    servers: Vec<Server>,
    problems: Vec<String>,
}

/// MCP backend that **connects in the background**.
///
/// Starting a server means spawning a process and waiting for a handshake — and
/// for an `npx`-launched server, fetching it first (measured: Context7 is ~24s
/// cold). Doing that during startup would put all of it on the critical path
/// before the first prompt, for every session, including ones that never call an
/// MCP tool.
///
/// So the extension registers immediately with **no tools**, spawns a thread that
/// connects, and adopts the tools when they arrive. `specs()` is called afresh
/// each turn (see `Agent::turn`), so the tools appear as soon as they are ready
/// — typically before the first turn finishes, and certainly before a model
/// could miss them. A session that never uses MCP pays nothing.
struct Mcp {
    /// Populated by the background thread. `None` until it finishes.
    connected: std::sync::Arc<std::sync::Mutex<Option<Connected>>>,
    /// Built once the tools arrive, then reused (they cannot change mid-session:
    /// a server that dies is not respawned).
    cache: std::sync::Arc<std::sync::Mutex<Option<Vec<ToolSpec>>>>,
    /// Set once the operator has been told what happened.
    reported: bool,
}

pub fn register(reg: &mut Registry) {
    if !mcp::enabled() {
        return;
    }
    let path = mcp::config_path();
    let configs = mcp::load_config(&path);
    if configs.is_empty() {
        // Nothing configured is the normal case: stay invisible.
        return;
    }
    reg.add(Box::new(Mcp::start_background(configs, path)));
}

impl Mcp {
    /// Register immediately; connect on a thread.
    fn start_background(configs: Vec<mcp::ServerConfig>, path: std::path::PathBuf) -> Mcp {
        let connected = std::sync::Arc::new(std::sync::Mutex::new(None));
        let slot = connected.clone();
        std::thread::spawn(move || {
            let result = Self::connect(configs, &path);
            if let Ok(mut g) = slot.lock() {
                *g = Some(result);
            }
        });
        Mcp {
            connected,
            cache: std::sync::Arc::new(std::sync::Mutex::new(None)),
            reported: false,
        }
    }

    /// The actual connect, run on the background thread.
    fn connect(configs: Vec<mcp::ServerConfig>, path: &std::path::Path) -> Connected {
        let mut servers = Vec::new();
        let mut problems = Vec::new();
        for cfg in configs {
            match Client::start(&cfg) {
                Ok(mut client) => match client.list_tools() {
                    Ok(tools) if !tools.is_empty() => {
                        servers.push(Server { client, tools });
                    }
                    Ok(_) => problems.push(format!("{}: server offers no tools", cfg.name)),
                    Err(e) => problems.push(format!("{}: {e}", cfg.name)),
                },
                Err(e) => problems.push(format!("{}: {e}", cfg.name)),
            }
        }
        if !problems.is_empty() {
            problems.push(format!("(configured in {})", path.display()));
        }
        Connected { servers, problems }
    }

    /// Run `f` against the connected servers, if the background thread has
    /// finished. Returns `None` while still connecting.
    fn with_servers<T>(&self, f: impl FnOnce(&mut Vec<Server>) -> T) -> Option<T> {
        let mut g = self.connected.lock().ok()?;
        let c = g.as_mut()?;
        Some(f(&mut c.servers))
    }

    /// All tools, flattened. `ToolSpec` holds `&'static str`, so the name and
    /// description must be interned — we leak them once per server, when the
    /// connection arrives. That is deliberate and bounded: a handful of strings
    /// per session for a fixed set of names, and it is what lets MCP tools
    /// appear in the same list as the built-ins without changing the ABI.
    fn build_specs(servers: &[Server]) -> Vec<ToolSpec> {
        let mut out = Vec::new();
        for s in servers {
            let server = s.client.server_name();
            for t in &s.tools {
                let name: &'static str = Box::leak(mcp::qualified_name(server, &t.name).into_boxed_str());
                let desc: &'static str = Box::leak(
                    format!(
                        "[MCP: {server}] {}",
                        if t.description.is_empty() {
                            "no description provided by the server".to_string()
                        } else {
                            t.description.clone()
                        }
                    )
                    .into_boxed_str(),
                );
                out.push(ToolSpec { name, description: desc, schema: t.input_schema.clone() });
            }
        }
        out
    }
}

impl ToolBackend for Mcp {
    fn name(&self) -> &'static str {
        "mcp"
    }

    fn specs(&self) -> Vec<ToolSpec> {
        // Reuse the built list once the servers are in.
        if let Ok(g) = self.cache.lock()
            && let Some(c) = g.as_ref()
        {
            return c.clone();
        }
        // Empty until the background connect finishes: that is the point — it
        // must never block the turn.
        let Ok(mut g) = self.connected.lock() else { return Vec::new() };
        let Some(c) = g.as_mut() else { return Vec::new() };
        let built = Self::build_specs(&c.servers);
        drop(g);
        if let Ok(mut cg) = self.cache.lock() {
            *cg = Some(built.clone());
        }
        built
    }

    fn run(&mut self, name: &str, input: &Value) -> Outcome {
        let Some((server, tool)) = mcp::split_qualified(name) else {
            return Outcome::err(format!("mcp: '{name}' is not an MCP tool name"));
        };
        // A tool can only be called once it has been advertised, so the servers
        // are normally connected by now. This only fires if the model called an
        // MCP tool from a stale list while the connect was still in flight.
        let still_connecting = self.connected.lock().map(|g| g.is_none()).unwrap_or(false);
        let out = self.with_servers(|servers| {
            let idx = servers.iter().position(|s| {
                mcp::qualified_name(s.client.server_name(), "x") == format!("mcp__{server}__x")
            });
            let Some(idx) = idx else {
                return Outcome::err(format!("mcp: no connected server named '{server}'"));
            };
            let s = &mut servers[idx];
            let Some(t) = s
                .tools
                .iter()
                .find(|t| mcp::qualified_name("x", &t.name) == format!("mcp__x__{tool}"))
            else {
                return Outcome::err(format!("mcp: server '{server}' has no tool '{tool}'"));
            };
            let real_tool = t.name.clone();
            match s.client.call_tool(&real_tool, input) {
                Ok(text) => Outcome::ok(text),
                Err(e) => Outcome::err(format!("mcp {server}/{real_tool}: {e}")),
            }
        });
        match out {
            Some(o) => o,
            None if still_connecting => Outcome::err(
                "mcp: the MCP servers are still starting (this is a one-off cost — an npx \
                 server may be downloading). Try again in a moment."
                    .to_string(),
            ),
            None => Outcome::err(format!("mcp: no connected server named '{server}'")),
        }
    }

    /// Report what happened, once, **after** the background connect finishes.
    ///
    /// Returns `None` while still connecting: `startup_report` runs before the
    /// first prompt on the critical path, so it must not wait. The tools appear
    /// on their own when they arrive, and this reports the outcome whenever it
    /// is next asked.
    fn startup_report(&mut self) -> Option<String> {
        if self.reported {
            return None;
        }
        let mut g = self.connected.lock().ok()?;
        let c = g.as_mut()?; // `None` = still connecting
        self.reported = true;
        if c.problems.is_empty() {
            return None; // a clean start stays silent
        }
        let tool_count: usize = c.servers.iter().map(|s| s.tools.len()).sum();
        let mut s = format!(
            "[mcp] {tool_count} tool(s) from {} server(s); {} problem(s):",
            c.servers.len(),
            c.problems.len()
        );
        for p in &c.problems {
            s.push_str(&format!("\n       {p}"));
        }
        Some(s)
    }

    /// Shut every server down. `Client::drop` kills the process group, so an
    /// `npx` wrapper and the node process under it both go.
    fn on_exit(&mut self) {
        if let Ok(mut g) = self.connected.lock()
            && let Some(c) = g.as_mut()
        {
            c.servers.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    /// A throwaway `mcp.json` plus the test server, so the extension is tested
    /// against a real MCP server end to end.
    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Option<Self> {
            let server = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/mcp_test_server.py");
            let ok = std::process::Command::new("python3")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok || !server.is_file() {
                return None;
            }
            let dir = std::env::temp_dir().join(format!("pir_mcp_ext_{}_{}", tag, std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let cfg = serde_json::json!({
                "servers": {
                    "test": { "command": "python3", "args": [server.to_string_lossy()] }
                }
            });
            std::fs::write(dir.join("mcp.json"), cfg.to_string()).unwrap();
            Some(Fixture { dir })
        }

        fn configs(&self) -> Vec<mcp::ServerConfig> {
            mcp::load_config(&self.dir.join("mcp.json"))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Connect eagerly for tests: `start_background` is the production path, but
    /// assertions about published tools need the connect to have finished. This
    /// is the same `connect` the background thread runs, just awaited.
    fn connected_mcp(configs: Vec<mcp::ServerConfig>, path: &std::path::Path) -> Mcp {
        let connected = std::sync::Arc::new(std::sync::Mutex::new(Some(Mcp::connect(configs, path))));
        Mcp { connected, cache: std::sync::Arc::new(std::sync::Mutex::new(None)), reported: false }
    }

    /// Wait for a backgrounded extension to finish connecting (bounded).
    fn wait_connected(ext: &mut Mcp) {
        for _ in 0..1000 {
            if ext.connected.lock().map(|g| g.is_some()).unwrap_or(false) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("MCP connect did not finish in time");
    }

    #[test]
    fn tools_from_a_real_server_are_published_and_callable() {
        let Some(f) = Fixture::new("pub") else { return };
        let mut ext = connected_mcp(f.configs(), &f.dir.join("mcp.json"));

        let specs = ext.specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name).collect();
        assert!(names.contains(&"mcp__test__echo"), "got: {names:?}");
        assert!(names.contains(&"mcp__test__add"), "got: {names:?}");
        // The description says which server it came from.
        let echo = specs.iter().find(|s| s.name == "mcp__test__echo").unwrap();
        assert!(echo.description.starts_with("[MCP: test]"), "got: {}", echo.description);
        assert!(echo.description.contains("Echo the given text"), "got: {}", echo.description);
        // And the schema is the server's own.
        assert_eq!(echo.schema["required"][0], "text");

        // Calling it through the backend reaches the server.
        let out = ext.run("mcp__test__echo", &serde_json::json!({"text": "hello mcp"}));
        assert!(!out.is_error, "got: {}", out.content);
        assert_eq!(out.content, "hello mcp");
        let out = ext.run("mcp__test__add", &serde_json::json!({"a": 20, "b": 22}));
        assert_eq!(out.content, "42");
    }

    #[test]
    fn unknown_names_and_servers_fail_with_a_useful_message() {
        let Some(f) = Fixture::new("unknown") else { return };
        let mut ext = connected_mcp(f.configs(), &f.dir.join("mcp.json"));
        let out = ext.run("bash", &serde_json::json!({}));
        assert!(out.is_error);
        assert!(out.content.contains("not an MCP tool name"), "got: {}", out.content);
        let out = ext.run("mcp__nosuch__tool", &serde_json::json!({}));
        assert!(out.is_error);
        assert!(out.content.contains("no connected server"), "got: {}", out.content);
        let out = ext.run("mcp__test__nosuchtool", &serde_json::json!({}));
        assert!(out.is_error);
        assert!(out.content.contains("has no tool"), "got: {}", out.content);
    }

    #[test]
    fn a_remote_error_is_surfaced_as_a_tool_error() {
        let Some(f) = Fixture::new("err") else { return };
        let mut ext = connected_mcp(f.configs(), &f.dir.join("mcp.json"));
        let out = ext.run("mcp__test__fail", &serde_json::json!({}));
        assert!(out.is_error, "got: {}", out.content);
        assert!(out.content.contains("always fails"), "got: {}", out.content);
    }

    /// A server that cannot start must not prevent the extension from loading,
    /// and must say so in the banner.
    #[test]
    fn a_failing_server_is_reported_and_non_fatal() {
        let dir = std::env::temp_dir().join(format!("pir_mcp_bad_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut f = std::fs::File::create(dir.join("mcp.json")).unwrap();
        write!(
            f,
            r#"{{"servers":{{"broken":{{"command":"this-does-not-exist-xyz"}}}}}}"#
        )
        .unwrap();
        drop(f);
        let mut ext = connected_mcp(mcp::load_config(&dir.join("mcp.json")), &dir.join("mcp.json"));
        // No tools, but the object is usable.
        assert!(ext.specs().is_empty());
        let report = ext.startup_report().expect("a problem must be reported");
        assert!(report.contains("broken"), "got: {report}");
        assert!(report.contains("could not start"), "got: {report}");
        // A call against it fails cleanly rather than panicking.
        let out = ext.run("mcp__broken__x", &serde_json::json!({}));
        assert!(out.is_error);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_problems_means_no_banner() {
        let Some(f) = Fixture::new("quiet") else { return };
        let mut ext = connected_mcp(f.configs(), &f.dir.join("mcp.json"));
        assert!(ext.startup_report().is_none(), "a clean start must be silent");
    }

    #[test]
    fn on_exit_shuts_the_servers_down() {
        let Some(f) = Fixture::new("exit") else { return };
        let mut ext = connected_mcp(f.configs(), &f.dir.join("mcp.json"));
        assert!(!ext.specs().is_empty());
        ext.on_exit();
        let empty = ext.with_servers(|s| s.is_empty()).unwrap_or(false);
        assert!(empty, "servers must be dropped");
    }

    /// The production path: `register` must return immediately and the tools
    /// must appear once the background connect finishes. This is what keeps a
    /// slow server (an npx download can take ~24s) off the startup path.
    #[test]
    fn connecting_happens_in_the_background_not_at_startup() {
        let Some(f) = Fixture::new("bg") else { return };
        let ext = Mcp::start_background(f.configs(), f.dir.join("mcp.json"));
        // Not connected yet (or, on a fast machine, already done) — either way
        // this must not block.
        let t0 = std::time::Instant::now();
        let _ = ext.specs();
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(500),
            "specs() must never wait for a connection, took {:?}",
            t0.elapsed()
        );
        let mut ext = ext;
        wait_connected(&mut ext);
        let names: Vec<String> = ext.specs().iter().map(|s| s.name.to_string()).collect();
        assert!(names.iter().any(|n| n == "mcp__test__echo"), "got: {names:?}");
        // And it is actually callable once connected.
        let out = ext.run("mcp__test__echo", &serde_json::json!({"text": "bg ok"}));
        assert!(!out.is_error, "got: {}", out.content);
        assert_eq!(out.content, "bg ok");
    }

    /// A call made while the connect is still in flight gets a clear, actionable
    /// error rather than a hang or a confusing "no such server".
    #[test]
    fn a_call_before_connecting_says_so() {
        // No fixture needed: this one is never connected at all.
        let ext = Mcp {
            connected: std::sync::Arc::new(std::sync::Mutex::new(None)),
            cache: std::sync::Arc::new(std::sync::Mutex::new(None)),
            reported: false,
        };
        let mut ext = ext;
        let out = ext.run("mcp__test__echo", &serde_json::json!({"text": "x"}));
        assert!(out.is_error);
        assert!(out.content.contains("still starting"), "got: {}", out.content);
        // And the banner stays silent while connecting (it must not block).
        assert!(ext.startup_report().is_none());
    }
}
