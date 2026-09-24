//! MCP (Model Context Protocol) client over **stdio** — JSON-RPC 2.0 on a
//! child process's stdin/stdout.
//!
//! An MCP server is a program that speaks JSON-RPC over its own standard
//! streams. pir starts it, sends requests, and reads responses — no sockets, no
//! HTTP. Messages are one JSON object per line (newline-delimited JSON):
//!
//! ```text
//! pir  ──spawn──►  server (child process)
//!  ├── stdin  ────►  {"jsonrpc":"2.0","id":1,"method":"tools/list"}
//!  └── stdout ◄────  {"jsonrpc":"2.0","id":1,"result":{"tools":[...]}}
//! ```
//!
//! This mirrors exactly what the `bash` tool already does (spawn a child, drain
//! pipes, kill its process group on exit), which is why stdio is the tractable
//! transport: all of the difficulty is JSON-RPC framing and lifecycle, not
//! networking.
//!
//! # Lifecycle
//!
//! ```text
//! → initialize            {"protocolVersion": "2024-11-05", "capabilities": {}}
//! ← {"capabilities": {...}}
//! → notifications/initialized
//! → tools/list            ← the tools we expose to the model
//! → tools/call            per invocation
//! ```
//!
//! # Configuration
//!
//! Servers are declared in `~/.pi/agent/mcp.json` (or `PI_DIR/agent/mcp.json`):
//!
//! ```json
//! {
//!   "servers": {
//!     "filesystem": {
//!       "command": "npx",
//!       "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
//!       "env": { "FOO": "bar" },
//!       "cwd": "/tmp"
//!     }
//!   }
//! }
//! ```
//!
//! The `{"mcpServers": {...}}` spelling that other clients use is accepted too,
//! so a config can be copied across. Set `PIR_MCP=0` to disable the feature
//! entirely (no process is ever spawned).

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Protocol revision we advertise. Servers that only understand an older one
/// still answer `initialize`; we do not insist on an echo.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// How long to wait for a response before giving up. Generous: a cold `npx`
/// download can take many seconds, and a slow tool call is still a tool call.
fn default_timeout() -> Duration {
    let secs = std::env::var("PIR_MCP_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(120);
    Duration::from_secs(secs)
}

/// Whether MCP is enabled (`PIR_MCP=0` turns it off before anything spawns).
pub fn enabled() -> bool {
    match std::env::var("PIR_MCP") {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no"),
        Err(_) => true,
    }
}

/// One declared server.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
}

/// Where the server declarations live.
pub fn config_path() -> PathBuf {
    crate::config::pi_dir().join("agent").join("mcp.json")
}

/// Ready-made server declarations an operator can opt into by name.
///
/// These are *suggestions*, not defaults. A default-on server would spawn a
/// Node process and need the network on every launch — a Context7 cold start
/// measured 24s here, against a ~1 MB idle footprint that is the whole point of
/// pir. So nothing here is activated until the operator asks for it.
///
/// `pub` so `/mcp init <name>` can write one out and the docs can list them.
pub fn starters() -> &'static [(&'static str, &'static str, &'static str, &'static [&'static str])] {
    // (name, description, command, args)
    &[
        (
            "context7",
            "Up-to-date library documentation (React, Next.js, etc). No API key needed; \
             first run fetches the server via npx and can take ~20s.",
            "npx",
            &["-y", "@upstash/context7-mcp"],
        ),
        (
            "filesystem",
            "Read/write files under a directory tree (narrower than pir's own tools, \
             but useful for a scoped sandbox).",
            "npx",
            &["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
        ),
    ]
}

/// Render the starter servers as a ready-to-paste `mcp.json`, for
/// `/mcp init [name...]`. With no names, every starter is included.
pub fn starter_config(names: &[String]) -> Result<String, String> {
    let all = starters();
    let chosen: Vec<&(&str, &str, &str, &[&str])> = if names.is_empty() {
        all.iter().collect()
    } else {
        let mut v = Vec::new();
        for n in names {
            match all.iter().find(|(name, ..)| name == n) {
                Some(s) => v.push(s),
                None => {
                    let known: Vec<&str> = all.iter().map(|(n, ..)| *n).collect();
                    return Err(format!(
                        "unknown MCP starter '{n}' (available: {})",
                        known.join(", ")
                    ));
                }
            }
        }
        v
    };
    let mut servers = serde_json::Map::new();
    for (name, _desc, command, args) in chosen {
        servers.insert(
            name.to_string(),
            json!({ "command": command, "args": args }),
        );
    }
    serde_json::to_string_pretty(&json!({ "servers": servers }))
        .map_err(|e| format!("could not render the config: {e}"))
}

/// Write a starter config, creating the parent directory. Refuses to overwrite
/// an existing file — silently replacing a hand-written server list would be a
/// hostile thing to do.
pub fn write_starter_config(path: &Path, names: &[String]) -> Result<String, String> {
    if path.exists() {
        return Err(format!(
            "{} already exists — edit it by hand (or move it aside) rather than having pir \
             overwrite your server list",
            path.display()
        ));
    }
    let text = starter_config(names)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, format!("{text}\n"))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(text)
}

/// Load server declarations. A missing file is an empty list (the feature is
/// simply not configured), never an error.
pub fn load_config(path: &Path) -> Vec<ServerConfig> {
    let Ok(raw) = std::fs::read_to_string(path) else { return Vec::new() };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else { return Vec::new() };
    // Accept both our `servers` key and the `mcpServers` spelling other clients
    // use, so a config can be copied here unchanged.
    let map = v
        .get("servers")
        .or_else(|| v.get("mcpServers"))
        .and_then(|s| s.as_object());
    let Some(map) = map else { return Vec::new() };
    let mut out = Vec::new();
    for (name, spec) in map {
        let Some(command) = spec.get("command").and_then(|c| c.as_str()) else { continue };
        let args = spec
            .get("args")
            .and_then(|a| a.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let env = spec
            .get("env")
            .and_then(|e| e.as_object())
            .map(|e| {
                e.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let cwd = spec.get("cwd").and_then(|c| c.as_str()).map(PathBuf::from);
        out.push(ServerConfig { name: name.clone(), command: command.to_string(), args, env, cwd });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// A tool as the server describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// What went wrong, phrased for the operator rather than the wire.
#[derive(Debug, Clone, PartialEq)]
pub enum McpError {
    /// We could not start the process at all (missing binary, permissions).
    Spawn(String),
    /// The server exited or closed its stream — it is gone for this session.
    Disconnected(String),
    /// No response within the timeout (the request may still be running).
    Timeout(Duration),
    /// The server answered with a JSON-RPC error.
    Remote { code: i64, message: String },
    /// The response was not the shape the protocol promises.
    Protocol(String),
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpError::Spawn(e) => write!(f, "could not start the MCP server: {e}"),
            McpError::Disconnected(e) => write!(f, "MCP server disconnected: {e}"),
            McpError::Timeout(d) => {
                write!(f, "MCP server did not respond within {}s", d.as_secs())
            }
            McpError::Remote { code, message } => write!(f, "MCP error {code}: {message}"),
            McpError::Protocol(e) => write!(f, "malformed MCP response: {e}"),
        }
    }
}

/// A live connection to one server.
pub struct Client {
    name: String,
    child: Child,
    stdin: ChildStdin,
    /// Responses, fed by the reader thread. Anything not carrying our id (a
    /// server-initiated notification, a stray log line) is dropped rather than
    /// mis-read as a reply.
    rx: Receiver<Value>,
    next_id: u64,
    timeout: Duration,
    /// Set once the reader thread sees EOF; further calls fail fast instead of
    /// each waiting out the timeout.
    dead: Option<String>,
}

impl Client {
    /// Spawn the server and perform the MCP handshake.
    pub fn start(cfg: &ServerConfig) -> Result<Client, McpError> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args);
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
        if let Some(cwd) = &cfg.cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        // Own process group, like the bash tool: killing the client kills the
        // whole tree (an `npx` wrapper plus the node process it launches), so a
        // dead pir never leaves an orphaned server behind.
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().map_err(|e| {
            McpError::Spawn(format!("{} {}: {e}", cfg.command, cfg.args.join(" ")))
        })?;
        let stdin = child.stdin.take().ok_or_else(|| McpError::Spawn("no stdin pipe".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| McpError::Spawn("no stdout pipe".into()))?;
        let rx = spawn_reader(cfg.name.clone(), stdout);
        let mut client = Client {
            name: cfg.name.clone(),
            child,
            stdin,
            rx,
            next_id: 1,
            timeout: default_timeout(),
            dead: None,
        };
        client.handshake()?;
        Ok(client)
    }

    /// `initialize` + `notifications/initialized`.
    fn handshake(&mut self) -> Result<(), McpError> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "pir", "version": env!("CARGO_PKG_VERSION") },
        });
        // The result is informational (we don't branch on the server's
        // capabilities), but a failure here means the server is not usable.
        let _info = self.request("initialize", Some(params))?;
        self.notify("notifications/initialized", None)
    }

    /// Send a request and wait for its response.
    pub fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value, McpError> {
        if let Some(why) = &self.dead {
            return Err(McpError::Disconnected(why.clone()));
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut msg = json!({ "jsonrpc": "2.0", "id": id, "method": method });
        if let Some(p) = params {
            msg["params"] = p;
        }
        self.send(&msg)?;

        let deadline = Instant::now() + self.timeout;
        loop {
            let left = match deadline.checked_duration_since(Instant::now()) {
                Some(d) if !d.is_zero() => d,
                _ => return Err(McpError::Timeout(self.timeout)),
            };
            match self.rx.recv_timeout(left) {
                Ok(v) => {
                    // Only a message carrying *our* id answers this request.
                    if v.get("id").and_then(|i| i.as_u64()) != Some(id) {
                        continue;
                    }
                    if let Some(err) = v.get("error") {
                        return Err(McpError::Remote {
                            code: err.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
                            message: err
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("(no message)")
                                .to_string(),
                        });
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
                Err(RecvTimeoutError::Timeout) => return Err(McpError::Timeout(self.timeout)),
                Err(RecvTimeoutError::Disconnected) => {
                    let why = self.child_gone();
                    self.dead = Some(why.clone());
                    return Err(McpError::Disconnected(why));
                }
            }
        }
    }

    /// Send a notification (no response expected).
    pub fn notify(&mut self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        let mut msg = json!({ "jsonrpc": "2.0", "method": method });
        if let Some(p) = params {
            msg["params"] = p;
        }
        self.send(&msg)
    }

    fn send(&mut self, msg: &Value) -> Result<(), McpError> {
        let mut line = msg.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|e| {
                let why = format!("cannot write to the server: {e}");
                self.dead = Some(why.clone());
                McpError::Disconnected(why)
            })
    }

    /// Discover the server's tools.
    pub fn list_tools(&mut self) -> Result<Vec<McpTool>, McpError> {
        let result = self.request("tools/list", Some(json!({})))?;
        let arr = result
            .get("tools")
            .and_then(|t| t.as_array())
            .ok_or_else(|| McpError::Protocol("tools/list had no 'tools' array".into()))?;
        Ok(arr
            .iter()
            .filter_map(|t| {
                let name = t.get("name").and_then(|n| n.as_str())?.to_string();
                Some(McpTool {
                    name,
                    description: t
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string(),
                    input_schema: t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                })
            })
            .collect())
    }

    /// Invoke a tool. Returns its text content.
    pub fn call_tool(&mut self, name: &str, args: &Value) -> Result<String, McpError> {
        let result = self.request("tools/call", Some(json!({ "name": name, "arguments": args })))?;
        Ok(render_tool_result(&result))
    }

    pub fn server_name(&self) -> &str {
        &self.name
    }

    /// Whether the child is still running (used to report a dead server rather
    /// than retrying it every turn).
    pub fn alive(&mut self) -> bool {
        if self.dead.is_some() {
            return false;
        }
        matches!(self.child.try_wait(), Ok(None))
    }

    fn child_gone(&mut self) -> String {
        match self.child.try_wait() {
            Ok(Some(status)) => format!("exited with {status}"),
            _ => "closed its output stream".to_string(),
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Best-effort teardown. Kill the whole process group so an `npx` wrapper
        // doesn't outlive us, then reap.
        #[cfg(unix)]
        {
            let pgid = self.child.id() as i32;
            unsafe {
                let _ = libc::kill(-pgid, libc::SIGTERM);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = self.child.kill();
        }
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => return,
            }
        }
        // Still alive after the grace period: escalate, but never block exit.
        #[cfg(unix)]
        {
            let pgid = self.child.id() as i32;
            unsafe {
                let _ = libc::kill(-pgid, libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
    }
}

/// Read newline-delimited JSON from the server's stdout on a thread.
///
/// A dedicated thread is required, not a convenience: `stdout` blocks, and the
/// main thread must stay free to enforce the per-request timeout. Lines that
/// are not JSON objects (a server that logs to stdout — common, and against the
/// spec) are skipped rather than treated as a fatal protocol error.
fn spawn_reader(_name: String, stdout: ChildStdout) -> Receiver<Value> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(line)
                && v.is_object()
                && tx.send(v).is_err()
            {
                break; // the client is gone
            }
        }
        // Dropping `tx` closes the channel, which the client reads as EOF.
    });
    rx
}

/// Render a `tools/call` result as text.
///
/// The protocol result is `{"content": [{"type":"text","text":...}, ...],
/// "isError": bool}`. Non-text parts (images, embedded resources) are named
/// rather than dumped — same reasoning as `read_file`: a base64 blob in a tool
/// result costs context and helps nobody.
fn render_tool_result(result: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(items) = result.get("content").and_then(|c| c.as_array()) {
        for item in items {
            match item.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                        parts.push(t.to_string());
                    }
                }
                Some(other) => {
                    let mime = item
                        .get("mimeType")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown type");
                    parts.push(format!("[{other} content: {mime}]"));
                }
                None => {}
            }
        }
    }
    // A server may answer with structured content only.
    if parts.is_empty()
        && let Some(sc) = result.get("structuredContent")
    {
        parts.push(sc.to_string());
    }
    if parts.is_empty() {
        return "(the MCP tool returned no content)".to_string();
    }
    let mut out = parts.join("\n");
    if result.get("isError").and_then(|e| e.as_bool()).unwrap_or(false) {
        out = format!("(MCP tool reported an error)\n{out}");
    }
    out
}

/// A tool name from one server, namespaced so two servers cannot collide and so
/// the model can see where a tool came from: `mcp__<server>__<tool>`.
pub fn qualified_name(server: &str, tool: &str) -> String {
    format!("mcp__{}__{}", sanitize(server), sanitize(tool))
}

/// Make a name safe for a tool identifier (lowercase alphanumerics, `_`, `-`).
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

/// Split a qualified name back into `(server, tool)`. `None` when the name is
/// not one of ours.
pub fn split_qualified(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parses_our_shape_and_the_common_alias() {
        let d = std::env::temp_dir().join(format!("pir_mcp_cfg_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let p = d.join("mcp.json");
        // Our key, plus args/env/cwd.
        std::fs::write(
            &p,
            r#"{"servers":{"fs":{"command":"npx","args":["-y","server-fs","/tmp"],
                 "env":{"FOO":"bar"},"cwd":"/tmp"}}}"#,
        )
        .unwrap();
        let cfgs = load_config(&p);
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].name, "fs");
        assert_eq!(cfgs[0].command, "npx");
        assert_eq!(cfgs[0].args, vec!["-y", "server-fs", "/tmp"]);
        assert_eq!(cfgs[0].env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(cfgs[0].cwd.as_deref(), Some(Path::new("/tmp")));

        // The `mcpServers` spelling other clients use.
        std::fs::write(&p, r#"{"mcpServers":{"git":{"command":"uvx","args":["mcp-git"]}}}"#).unwrap();
        let cfgs = load_config(&p);
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].command, "uvx");
        assert!(cfgs[0].args == vec!["mcp-git"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn config_tolerates_garbage_without_failing() {
        let d = std::env::temp_dir().join(format!("pir_mcp_garbage_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let p = d.join("mcp.json");
        // Missing file.
        assert!(load_config(&d.join("nope.json")).is_empty());
        // Invalid JSON.
        std::fs::write(&p, "{not json").unwrap();
        assert!(load_config(&p).is_empty());
        // No servers key.
        std::fs::write(&p, r#"{"other":1}"#).unwrap();
        assert!(load_config(&p).is_empty());
        // A server with no command is skipped, not half-registered.
        std::fs::write(&p, r#"{"servers":{"bad":{},"good":{"command":"x"}}}"#).unwrap();
        let cfgs = load_config(&p);
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].name, "good");
        let _ = std::fs::remove_dir_all(&d);
    }


    /// Starters are suggestions, never defaults: nothing is written until the
    /// operator asks, and an existing config is never clobbered.
    #[test]
    fn starter_config_is_opt_in_and_never_overwrites() {
        let d = std::env::temp_dir().join(format!("pir_mcp_starter_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let path = d.join("agent").join("mcp.json");

        // Nothing exists yet: writing one starter produces valid JSON naming it.
        let text = write_starter_config(&path, &["context7".to_string()]).expect("write starter");
        assert!(path.is_file(), "the config must be written");
        let cfgs = load_config(&path);
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].name, "context7");
        assert_eq!(cfgs[0].command, "npx");
        assert!(cfgs[0].args.iter().any(|a| a.contains("context7-mcp")), "{:?}", cfgs[0].args);
        assert!(text.contains("context7"), "the rendered text should be shown: {text}");

        // Writing again must refuse rather than replace a hand-edited list.
        let err = write_starter_config(&path, &[]).unwrap_err();
        assert!(err.contains("already exists"), "got: {err}");
        assert!(err.contains("overwrite"), "must explain why: {err}");
        // ...and the original is untouched.
        assert_eq!(load_config(&path).len(), 1);

        // No names = every starter.
        let p2 = d.join("agent2").join("mcp.json");
        write_starter_config(&p2, &[]).expect("all starters");
        assert_eq!(load_config(&p2).len(), starters().len());

        // An unknown name is rejected with the valid list.
        let err = write_starter_config(&d.join("x.json"), &["nope".to_string()]).unwrap_err();
        assert!(err.contains("unknown MCP starter"), "got: {err}");
        assert!(err.contains("context7"), "must list what is available: {err}");
        // And nothing was written by the failed attempt.
        assert!(!d.join("x.json").exists(), "a rejected name must not create a file");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn every_starter_renders_a_valid_config() {
        // Each declared starter must produce a config pir can actually load —
        // a starter that doesn't parse would be a trap.
        let text = starter_config(&[]).expect("render");
        let v: Value = serde_json::from_str(&text).expect("valid JSON");
        let servers = v.get("servers").and_then(|s| s.as_object()).expect("servers object");
        assert!(!servers.is_empty());
        for (name, spec) in servers {
            assert!(spec.get("command").and_then(|c| c.as_str()).is_some(), "{name} has no command");
            assert!(spec.get("args").and_then(|a| a.as_array()).is_some(), "{name} has no args");
        }
        // And the loader agrees with the renderer.
        let d = std::env::temp_dir().join(format!("pir_mcp_render_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let p = d.join("mcp.json");
        std::fs::write(&p, &text).unwrap();
        assert_eq!(load_config(&p).len(), servers.len());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn enabled_honours_the_kill_switch() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("PIR_MCP");
        // SAFETY: test-local env mutation under the shared env lock.
        unsafe { std::env::set_var("PIR_MCP", "0"); }
        assert!(!enabled());
        unsafe { std::env::set_var("PIR_MCP", "off"); }
        assert!(!enabled());
        unsafe { std::env::set_var("PIR_MCP", "1"); }
        assert!(enabled());
        match old {
            Some(v) => unsafe { std::env::set_var("PIR_MCP", v) },
            None => unsafe { std::env::remove_var("PIR_MCP") },
        }
    }

    #[test]
    fn tool_names_are_namespaced_and_round_trip() {
        let n = qualified_name("my-server", "read file");
        assert_eq!(n, "mcp__my-server__read_file");
        let (server, tool) = split_qualified(&n).expect("round trips");
        assert_eq!(server, "my-server");
        assert_eq!(tool, "read_file");
        // Not ours.
        assert!(split_qualified("bash").is_none());
        assert!(split_qualified("mcp__onlyserver").is_none());
        assert!(split_qualified("mcp____tool").is_none());
        assert!(split_qualified("mcp__server__").is_none());
    }

    #[test]
    fn tool_results_render_text_and_name_binary_parts() {
        let r = json!({"content":[
            {"type":"text","text":"hello"},
            {"type":"image","mimeType":"image/png","data":"AAA"}
        ]});
        let out = render_tool_result(&r);
        assert!(out.contains("hello"), "got: {out}");
        assert!(out.contains("[image content: image/png]"), "got: {out}");
        assert!(!out.contains("AAA"), "base64 must not be dumped: {out}");

        // An error result is labelled.
        let r = json!({"isError": true, "content":[{"type":"text","text":"boom"}]});
        let out = render_tool_result(&r);
        assert!(out.contains("reported an error"), "got: {out}");
        assert!(out.contains("boom"));

        // Structured-only results still produce something.
        let r = json!({"structuredContent":{"a":1}});
        assert!(render_tool_result(&r).contains("\"a\""));

        // Nothing at all is stated, not silently empty.
        assert!(render_tool_result(&json!({})).contains("no content"));
    }


    /// Path to the bundled test server (a real MCP server, so these tests
    /// exercise the wire format rather than a mock).
    fn test_server() -> Option<ServerConfig> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/mcp_test_server.py");
        if !path.is_file() {
            return None;
        }
        // Prefer the interpreter that exists.
        if python_available() {
            Some(ServerConfig {
                name: "t".into(),
                command: "python3".into(),
                args: vec![path.to_string_lossy().to_string()],
                env: BTreeMap::new(),
                cwd: None,
            })
        } else {
            None
        }
    }

    fn python_available() -> bool {
        Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn handshake_and_tool_discovery_work_against_a_real_server() {
        let Some(cfg) = test_server() else { return };
        let mut c = Client::start(&cfg).expect("handshake");
        let tools = c.list_tools().expect("tools/list");
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        for want in ["echo", "add", "fail", "emit_image", "slow"] {
            assert!(names.contains(&want), "tool {want} missing from {names:?}");
        }
        // Descriptions and schemas survive the trip.
        let echo = tools.iter().find(|t| t.name == "echo").unwrap();
        assert_eq!(echo.description, "Echo the given text back");
        assert_eq!(echo.input_schema["required"][0], "text");
    }

    #[test]
    fn calling_a_tool_returns_its_text() {
        let Some(cfg) = test_server() else { return };
        let mut c = Client::start(&cfg).expect("handshake");
        let out = c.call_tool("echo", &json!({"text": "round-trip"})).expect("call");
        assert_eq!(out, "round-trip");
        let out = c.call_tool("add", &json!({"a": 2, "b": 40})).expect("call");
        assert_eq!(out, "42");
    }

    #[test]
    fn a_jsonrpc_error_from_the_server_is_reported_as_such() {
        let Some(cfg) = test_server() else { return };
        let mut c = Client::start(&cfg).expect("handshake");
        let err = c.call_tool("fail", &json!({})).unwrap_err();
        match err {
            McpError::Remote { code, message } => {
                assert_eq!(code, -32000);
                assert!(message.contains("always fails"), "got: {message}");
            }
            other => panic!("expected a remote error, got {other:?}"),
        }
    }

    #[test]
    fn non_text_content_is_named_and_its_payload_dropped() {
        let Some(cfg) = test_server() else { return };
        let mut c = Client::start(&cfg).expect("handshake");
        let out = c.call_tool("emit_image", &json!({})).expect("call");
        assert!(out.contains("here is an image"), "got: {out}");
        assert!(out.contains("[image content: image/png]"), "got: {out}");
        assert!(!out.contains("iVBORw0KGgo"), "base64 leaked: {out}");
    }

    #[test]
    fn a_slow_call_times_out_without_hanging() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(cfg) = test_server() else { return };
        let old = std::env::var_os("PIR_MCP_TIMEOUT_SECS");
        // SAFETY: test-local env mutation under the shared env lock.
        unsafe { std::env::set_var("PIR_MCP_TIMEOUT_SECS", "1"); }
        let mut c = Client::start(&cfg).expect("handshake");
        let t0 = Instant::now();
        let err = c.call_tool("slow", &json!({"seconds": 30})).unwrap_err();
        let took = t0.elapsed();
        match old {
            Some(v) => unsafe { std::env::set_var("PIR_MCP_TIMEOUT_SECS", v) },
            None => unsafe { std::env::remove_var("PIR_MCP_TIMEOUT_SECS") },
        }
        assert!(matches!(err, McpError::Timeout(_)), "got: {err:?}");
        assert!(took < Duration::from_secs(10), "must not wait for the server: {took:?}");
    }

    #[test]
    fn a_server_that_cannot_start_is_a_clear_error() {
        let cfg = ServerConfig {
            name: "nope".into(),
            command: "this-binary-does-not-exist-anywhere".into(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
        };
        let err = match Client::start(&cfg) {
            Err(e) => e,
            Ok(_) => panic!("a nonexistent binary must not start"),
        };
        assert!(matches!(err, McpError::Spawn(_)), "got: {err:?}");
        assert!(err.to_string().contains("could not start"), "got: {err}");
    }

    #[test]
    fn killing_the_client_kills_the_server_tree() {
        let Some(cfg) = test_server() else { return };
        let c = Client::start(&cfg).expect("handshake");
        let pid = c.child.id() as i32;
        assert!(unsafe { libc::kill(pid, 0) } == 0, "server should be running");
        drop(c);
        // After Drop the process must be gone (Drop kills the group and reaps).
        let mut gone = false;
        for _ in 0..50 {
            if unsafe { libc::kill(pid, 0) } != 0 {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(gone, "the MCP server outlived the client");
    }

    /// A server that logs to stdout (against the spec, but common) must not
    /// break the client: non-JSON lines are skipped.
    #[test]
    fn noisy_stdout_does_not_break_the_protocol() {
        let d = std::env::temp_dir().join(format!("pir_mcp_noisy_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let script = d.join("noisy.py");
        std::fs::write(
            &script,
            r#"#!/usr/bin/env python3
import sys, json
print("warning: this is a log line, not JSON")
sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    m = json.loads(line)
    if m.get("id") is None: continue
    print("another log line")
    sys.stdout.flush()
    sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":m["id"],"result":{"tools":[{"name":"x","description":"d","inputSchema":{"type":"object"}}]}}) + "\n")
    sys.stdout.flush()
"#,
        )
        .unwrap();
        if !python_available() {
            let _ = std::fs::remove_dir_all(&d);
            return;
        }
        let cfg = ServerConfig {
            name: "noisy".into(),
            command: "python3".into(),
            args: vec![script.to_string_lossy().to_string()],
            env: BTreeMap::new(),
            cwd: None,
        };
        let mut c = Client::start(&cfg).expect("handshake despite noise");
        let tools = c.list_tools().expect("tools/list despite noise");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "x");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn errors_display_usefully() {
        let e = McpError::Spawn("npx: not found".into());
        assert!(e.to_string().contains("could not start"));
        assert!(e.to_string().contains("not found"));
        assert!(McpError::Timeout(Duration::from_secs(30)).to_string().contains("30s"));
        assert!(
            McpError::Remote { code: -32601, message: "no such method".into() }
                .to_string()
                .contains("-32601")
        );
    }
}
