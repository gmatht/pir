//! pi-extensions — PIR extension that drives the Node.js shim.
//!
//! This is the PIR side of the bridge described in `docs/protocol.md`. It spawns
//! the `extensions/pi-extensions/shim/index.js` Node.js process (once), speaks
//! line-delimited JSON over its stdio, and exposes every loaded pi extension's
//! tools to the model as `piext_<extId>__<tool>`. It also implements the
//! feature-manifest / compatibility-warning system: when an extension is loaded
//! we read its declared `requires` (from package.json or `module.exports`),
//! compare against the shim's ABI, and warn about any unsupported feature —
//! offering to spin up an agent to fix the gap.
//!
//! Enable with `PIR_PI_EXTENSIONS=1`. Off by default. When off, this extension
//! registers no tools and spawns no process.

use crate::plugin::{CommandSpec, EventKind, Outcome, Registry, ToolBackend, ToolSpec};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const SHIM_REL: &str = "extensions/pi-extensions/shim/index.js";

/// Tool/command identity inside a loaded extension.
struct ExtTool {
    ext_id: String,
    tool: String,
    description: String,
    schema: Value,
}

struct ExtCommand {
    ext_id: String,
    name: String,
    description: String,
}

/// A compatibility problem surfaced at install time: a feature the extension
/// declares it needs that the host ABI does not fully support.
#[derive(Clone)]
struct Gap {
    feature: String,
    /// True when the host can auto-stub the feature (e.g. `ctx.ui.confirm` is
    /// auto-approved) so the gap is non-fatal; false when it is hard-unsupported
    /// and the extension may misbehave.
    auto_stubbed: bool,
}

/// Single managed Node.js shim process + its JSON line protocol pump.
struct Shim {
    child: Child,
    stdin: ChildStdin,
    /// Next request id.
    next_id: Arc<AtomicU64>,
    /// Pending requests keyed by id: (oneshot tx for the JSON result, plus a
    /// channel of async `log` lines received before the response).
    pending: Arc<Mutex<HashMap<u64, Pending>>>,
    /// Machine-readable ABI the shim reported at startup.
    abi: Value,
    logs: Arc<Mutex<Vec<String>>>,
}

struct Pending {
    tx: std::sync::mpsc::Sender<Value>,
}

fn shim_path() -> PathBuf {
    // Allow an explicit override (tests, or a shim installed elsewhere).
    if let Some(p) = std::env::var_os("PIR_SHIM_PATH") {
        let p = PathBuf::from(p);
        if p.exists() {
            return p;
        }
    }
    // Resolve relative to the pir binary's manifest dir when possible, else cwd.
    let here = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    // `target/<profile>/pir` -> repo root is two levels up.
    let candidates = [
        here.join(SHIM_REL),
        here.join("../..").join(SHIM_REL),
        PathBuf::from(SHIM_REL),
    ];
    candidates.into_iter().find(|p| p.exists()).unwrap_or_else(|| here.join(SHIM_REL))
}

fn node_bin() -> String {
    std::env::var("PIR_NODE_BIN").unwrap_or_else(|_| "node".to_string())
}

impl Shim {
    fn start() -> std::io::Result<Shim> {
        let bin = node_bin();
        let shim = shim_path();
        let mut child = Command::new(&bin)
            .arg(&shim)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("PIR_SESSION_FILE", std::env::var("PIR_SESSION_FILE").unwrap_or_default())
            .spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let pending: Arc<Mutex<HashMap<u64, Pending>>> = Arc::new(Mutex::new(HashMap::new()));
        let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let abi_slot: Arc<Mutex<Value>> = Arc::new(Mutex::new(json!({})));

        // Reader thread: parse `<id> <json>` lines from the shim. `id` 0 means
        // an async log/notification (no response expected).
        let pending_r = pending.clone();
        let logs_r = logs.clone();
        let abi_r = abi_slot.clone();
        let reader = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break, // EOF: shim exited
                    Ok(_) => {}
                    Err(_) => break,
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let msg: Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(e) => {
                        let mut g = logs_r.lock().unwrap();
                        g.push(format!("[shim] dropped malformed line: {e}"));
                        continue;
                    }
                };
                // Async / notification: id is null/0.
                let id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                if id == 0 {
                    if let Some(method) = msg.get("method").and_then(|m| m.as_str()) {
                        match method {
                            "ready" => {
                                if let Some(params) = msg.get("params") {
                                    *abi_r.lock().unwrap() = params.clone();
                                }
                            }
                            "log" => {
                                let level = msg
                                    .get("params")
                                    .and_then(|p| p.get("level"))
                                    .and_then(|l| l.as_str())
                                    .unwrap_or("info");
                                let text = msg
                                    .get("params")
                                    .and_then(|p| p.get("message"))
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("");
                                let mut g = logs_r.lock().unwrap();
                                g.push(format!("[shim:{level}] {text}"));
                            }
                            _ => {}
                        }
                    }
                    continue;
                }
                // Response to a request: route to the matching pending entry.
                if let Some(p) = pending_r.lock().unwrap().remove(&id) {
                    let _ = p.tx.send(msg);
                }
            }
        });

        // Drain shim stderr to our own logs so nothing is silently lost.
        let logs_e = logs.clone();
        thread::spawn(move || {
            let mut r = BufReader::new(stderr);
            let mut line = String::new();
            while r.read_line(&mut line).unwrap_or(0) > 0 {
                let t = line.trim().to_string();
                if !t.is_empty() {
                    logs_e.lock().unwrap().push(format!("[shim:stderr] {t}"));
                }
                line.clear();
            }
        });

        let mut shim = Shim {
            child,
            stdin,
            next_id: Arc::new(AtomicU64::new(1)),
            pending: pending.clone(),
            abi: json!({}),
            logs: logs.clone(),
        };
        // Wait for the `ready` notification (with a bounded timeout).
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            {
                let abi = abi_slot.lock().unwrap();
                if !abi.is_null() && abi.get("abi").is_some() {
                    shim.abi = abi.clone();
                    break;
                }
            }
            if std::time::Instant::now() >= deadline {
                break; // proceed anyway; abi just stays empty
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = reader; // reader thread is detached; lives until shim exits
        Ok(shim)
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let mut g = self.pending.lock().unwrap();
            g.insert(id, Pending { tx });
        }
        let msg = json!({ "id": id, "method": method, "params": params });
        let line = serde_json::to_string(&msg).map_err(|e| e.to_string())?;
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .map_err(|e| format!("shim write error: {e}"))?;

        // Wait for the response (bounded).
        let resp = rx.recv_timeout(Duration::from_secs(60)).map_err(|e| format!("shim timeout: {e}"))?;
        if let Some(err) = resp.get("error") {
            let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown shim error");
            return Err(msg.to_string());
        }
        let result = resp.get("result").cloned().unwrap_or(Value::Null);
        Ok(result)
    }

    fn abi(&self) -> &Value {
        &self.abi
    }

    fn drain_logs(&self) -> Vec<String> {
        std::mem::take(&mut *self.logs.lock().unwrap())
    }
}

impl Drop for Shim {
    fn drop(&mut self) {
        // Best-effort: flush stdin (signals EOF to the shim) and wait briefly
        // using poll-based try_wait (std `Child` has no `wait_timeout`).
        let _ = self.stdin.flush();
        if let Ok(Some(_)) = self.child.try_wait() {
            return;
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if std::time::Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    return;
                }
                Ok(None) => thread::sleep(Duration::from_millis(100)),
                Err(_) => return,
            }
        }
    }
}

/// The extension state: one shim process, the set of loaded extensions, and the
/// identity map from namespaced tool/command names to (ext, tool/cmd).
struct PiExtensions {
    enabled: bool,
    shim: Option<Shim>,
    tools: Vec<ExtTool>,
    commands: Vec<ExtCommand>,
    /// extension id -> last compatibility gaps (for /piext status).
    gaps: HashMap<String, Vec<Gap>>,
    /// Whether the agent may be spawned to auto-fix compatibility gaps.
    agent_fixes: bool,
}

impl PiExtensions {
    fn new() -> Self {
        let enabled = std::env::var_os("PIR_PI_EXTENSIONS")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false);
        let agent_fixes = std::env::var_os("PIR_PI_EXT_FIX")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(true);
        PiExtensions {
            enabled,
            shim: None,
            tools: Vec::new(),
            commands: Vec::new(),
            gaps: HashMap::new(),
            agent_fixes,
        }
    }

    fn ensure_shim(&mut self) -> Result<&mut Shim, String> {
        if self.shim.is_none() {
            match Shim::start() {
                Ok(s) => self.shim = Some(s),
                Err(e) => return Err(format!("pi-extensions: could not start shim: {e}")),
            }
        }
        Ok(self.shim.as_mut().unwrap())
    }

    /// Compute compatibility gaps between a declared `requires` list and the
    /// shim's ABI. `notSupported` features are reported (auto_stubbed for the
    /// ones the host can emulate, hard for the rest). `features` are always OK.
    fn analyze_requires(&mut self, ext_id: &str, requires: &[String]) -> Result<Vec<Gap>, String> {
        let shim = self.ensure_shim()?;
        let abi = shim.abi().clone();
        let supported: Vec<String> = abi
            .get("features")
            .and_then(|f| f.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        let not_supported: Vec<String> = abi
            .get("notSupported")
            .and_then(|f| f.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();

        let mut gaps = Vec::new();
        for feat in requires {
            if supported.contains(feat) {
                continue;
            }
            // Known unsupported feature. Some are auto-stubbed by the host.
            let auto_stubbed = matches!(
                feat.as_str(),
                "ctx.ui.confirm" | "ctx.ui.custom" | "ctx.ui.setStatus" | "ctx.ui.setWidget"
            );
            let _ = not_supported; // not_supported is informational; the
                                   // auto_stubbed set above encodes what we can
                                   // emulate. Anything else unsupported is a hard
                                   // gap.
            gaps.push(Gap {
                feature: feat.clone(),
                auto_stubbed,
            });
        }
        self.gaps.insert(ext_id.to_string(), gaps.clone());
        Ok(gaps)
    }

    /// Load an extension from disk, register its tools/commands, and analyze its
    /// compatibility. Returns a human-readable summary including any warnings.
    fn install_extension(&mut self, path: &str) -> Result<String, String> {
        let shim = self.ensure_shim()?;
        let info = shim.request(
            "load_extension",
            json!({ "path": path }),
        )?;
        let ext_id = info
            .get("extensionId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "shim did not return an extensionId".to_string())?
            .to_string();
        let reqs: Vec<String> = info
            .get("requires")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();

        // Register tools/commands as namespaced PIR tools.
        if let Some(tools) = info.get("tools").and_then(|t| t.as_array()) {
            for t in tools {
                let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                if name.is_empty() {
                    continue;
                }
                let description = t
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();
                let schema = t.get("schema").cloned().unwrap_or_else(|| json!({}));
                self.tools.push(ExtTool {
                    ext_id: ext_id.clone(),
                    tool: name,
                    description,
                    schema,
                });
            }
        }
        if let Some(cmds) = info.get("commands").and_then(|c| c.as_array()) {
            for c in cmds {
                let name = c.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                if name.is_empty() {
                    continue;
                }
                let description = c
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();
                self.commands.push(ExtCommand {
                    ext_id: ext_id.clone(),
                    name,
                    description,
                });
            }
        }

        // Compatibility analysis.
        let gaps = self.analyze_requires(&ext_id, &reqs)?;
        let mut summary = format!("loaded pi extension '{ext_id}' from {path}");
        if !gaps.is_empty() {
            let hard: Vec<&Gap> = gaps.iter().filter(|g| !g.auto_stubbed).collect();
            let soft: Vec<&Gap> = gaps.iter().filter(|g| g.auto_stubbed).collect();
            summary.push_str("\n⚠ compatibility warning: this extension needs features the pir ABI does not support:");
            for g in &hard {
                summary.push_str(&format!("\n  - HARD: {} (not supported by the shim)", g.feature));
            }
            for g in &soft {
                summary.push_str(&format!(
                    "\n  - SOFT: {} (auto-stubbed by the shim, may differ from real pi)",
                    g.feature
                ));
            }
            if !hard.is_empty() && self.agent_fixes {
                summary.push_str(
                    "\n  you can run /piext fix <ext_id> to spin up an agent that adapts the extension to the pir ABI.",
                );
            }
        } else if !reqs.is_empty() {
            summary.push_str("\n✓ all required features are supported by the pir ABI");
        }
        Ok(summary)
    }

    /// Forward a tool call to the shim and return the value.
    fn call_tool(&mut self, ext_id: &str, tool: &str, input: &Value) -> Result<String, String> {
        let shim = self.ensure_shim()?;
        // Map the PIR tool input object back into the positional `args` the pi
        // extension API expects (tools receive an `args` array / object). We
        // pass the whole object as `args`.
        let result = shim.request(
            "call_extension",
            json!({
                "extensionId": ext_id,
                "method": tool,
                "args": input
            }),
        )?;
        let value = result.get("value").cloned().unwrap_or(Value::Null);
        Ok(serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()))
    }

    fn run_command(&mut self, ext_id: &str, name: &str, args: &str) -> Result<String, String> {
        let shim = self.ensure_shim()?;
        let result = shim.request(
            "run_command",
            json!({
                "extensionId": ext_id,
                "name": name,
                "args": args
            }),
        )?;
        let value = result.get("value").cloned().unwrap_or(Value::Null);
        Ok(match value {
            Value::String(s) => s,
            other => serde_json::to_string_pretty(&other).unwrap_or_else(|_| other.to_string()),
        })
    }

    fn namespaced_tool_name(&self, t: &ExtTool) -> String {
        format!("piext_{}__{}", t.ext_id, t.tool)
    }
}

impl ToolBackend for PiExtensions {
    fn name(&self) -> &'static str {
        "pi-extensions"
    }

    fn specs(&self) -> Vec<ToolSpec> {
        if !self.enabled {
            return Vec::new();
        }
        let mut specs = vec![ToolSpec {
            name: "piext_install",
            description:
                "Load a legacy pi extension via the Node.js shim. Pass 'path' to the extension \
                 directory or .js file. Resolves the extension's feature manifest, warns if it \
                 needs features the pir ABI does not support, and registers its tools/commands.",
            schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "extension dir or file" } },
                "required": ["path"]
            }),
        }];
        // One tool per loaded extension tool, namespaced.
        for t in &self.tools {
            specs.push(ToolSpec {
                name: self.namespaced_tool_name(t).leak(),
                description: Box::leak(t.description.clone().into_boxed_str()),
                schema: t.schema.clone(),
            });
        }
        specs
    }

    fn run(&mut self, name: &str, input: &Value) -> Outcome {
        if name == "piext_install" {
            let path = match input.get("path").and_then(|v| v.as_str()) {
                Some(p) if !p.is_empty() => p.to_string(),
                _ => return Outcome::err("piext_install: missing 'path'".into()),
            };
            return match self.install_extension(&path) {
                Ok(s) => {
                    // Surface any shim logs (e.g. session_start notifications).
                    if let Some(shim) = self.shim.as_ref() {
                        for l in shim.drain_logs() {
                            eprintln!("{}", crate::term::dim(&l));
                        }
                    }
                    Outcome::ok(s)
                }
                Err(e) => Outcome::err(e),
            };
        }
        // Namespaced tool call: parse `piext_<extId>__<tool>`.
        if let Some(rest) = name.strip_prefix("piext_") {
            if let Some((ext_id, tool)) = rest.split_once("__") {
                return match self.call_tool(ext_id, tool, input) {
                    Ok(s) => Outcome::ok(s),
                    Err(e) => Outcome::err(e),
                };
            }
        }
        Outcome::err(format!("unknown pi-extensions tool '{name}'"))
    }

    fn commands(&self) -> Vec<CommandSpec> {
        if !self.enabled {
            return Vec::new();
        }
        let mut cmds = vec![CommandSpec {
            name: "piext".to_string(),
            description: "pi-extensions: list loaded extensions and their tools/commands, or run /piext status".to_string(),
        }];
        for c in &self.commands {
            cmds.push(CommandSpec {
                name: format!("piext_{}__{}", c.ext_id, c.name),
                description: c.description.clone(),
            });
        }
        cmds
    }

    fn run_command(&mut self, name: &str, args: &str) -> Outcome {
        if name == "piext" {
            return self.piext_control(args);
        }
        if let Some(rest) = name.strip_prefix("piext_") {
            if let Some((ext_id, cmd)) = rest.split_once("__") {
                return match self.run_command(ext_id, cmd, args) {
                    Ok(s) => Outcome::ok(s),
                    Err(e) => Outcome::err(e),
                };
            }
        }
        Outcome::err(format!("unknown pi-extensions command '/{name}'"))
    }

    fn on_event(&mut self, kind: EventKind, _payload: &Value) {
        if !self.enabled {
            return;
        }
        // Map PIR lifecycle events to pi event names and forward to the shim.
        let ev = match kind {
            EventKind::SessionStart => "session_start",
            EventKind::TurnStart => "turn_start",
            EventKind::TurnEnd => "turn_end",
            EventKind::AgentStart => "agent_start",
            EventKind::AgentEnd => "agent_end",
            _ => return,
        };
        if let Ok(shim) = self.ensure_shim() {
            let _ = shim.request("event", json!({ "event": ev, "payload": {} }));
        }
    }

    fn on_exit(&mut self) {
        // Reap the shim process on exit.
        if let Some(mut shim) = self.shim.take() {
            let _ = shim.child.kill();
            let _ = shim.child.wait();
        }
    }
}

impl PiExtensions {
    /// `/piext` control command: `status`, `fix <extId>`, `list`.
    fn piext_control(&mut self, args: &str) -> Outcome {
        let args = args.trim();
        match args {
            "" | "status" | "list" => {
                if self.tools.is_empty() && self.commands.is_empty() {
                    return Outcome::ok("(no pi extensions loaded — use piext_install <path>)".into());
                }
                let mut out = String::from("loaded pi extensions:\n");
                let mut seen = std::collections::HashSet::new();
                for t in &self.tools {
                    if seen.insert(t.ext_id.clone()) {
                        out.push_str(&format!("  - {} ({} tools, {} commands)\n", t.ext_id, self.tools.len(), self.commands.len()));
                    }
                }
                out.push_str("\ntools:\n");
                for t in &self.tools {
                    out.push_str(&format!("  - {}  {}\n", self.namespaced_tool_name(t), crate::term::dim(&t.description)));
                }
                if !self.commands.is_empty() {
                    out.push_str("\ncommands:\n");
                    for c in &self.commands {
                        out.push_str(&format!("  - /piext_{}__{}\n", c.ext_id, c.name));
                    }
                }
                Outcome::ok(out)
            }
            other => {
                if let Some(ext_id) = other.strip_prefix("fix ") {
                    return self.offer_fix(ext_id.trim());
                }
                Outcome::err(format!("unknown /piext subcommand '{other}' (try: status, fix <extId>)"))
            }
        }
    }

    /// Offer to spin up an agent to fix a compatibility gap for `ext_id`.
    fn offer_fix(&mut self, ext_id: &str) -> Outcome {
        let gaps = self.gaps.get(ext_id).cloned().unwrap_or_default();
        if gaps.is_empty() {
            return Outcome::ok(format!("extension '{ext_id}' has no recorded compatibility gaps"));
        }
        let hard: Vec<&Gap> = gaps.iter().filter(|g| !g.auto_stubbed).collect();
        if hard.is_empty() {
            return Outcome::ok(format!(
                "extension '{ext_id}' gaps are all auto-stubbed by the shim; no agent fix needed"
            ));
        }
        if !self.agent_fixes {
            return Outcome::err(format!(
                "extension '{ext_id}' needs unsupported features {:?}; agent-assisted fixes are disabled (PIR_PI_EXT_FIX=0)",
                hard.iter().map(|g| g.feature.clone()).collect::<Vec<_>>()
            ));
        }
        // Queue a follow-up prompt that drives an agent to adapt the extension
        // to the pir ABI. The agent will read the extension, edit it to drop or
        // emulate the unsupported features, and re-install it.
        let features = hard.iter().map(|g| g.feature.clone()).collect::<Vec<_>>().join(", ");
        let prompt = format!(
            "The pi extension '{ext_id}' (loaded via the Node.js shim) declares it needs the \
             following features that the pir ABI does not support: {features}. \
             Adapt the extension so it works under pir: remove or emulate those \
             features using the shim's supported surface (see docs/protocol.md and the \
             ABI from `pir --abi`), then re-run piext_install on its path and confirm \
             the compatibility warning is gone. Explain each change you make.",
        );
        // We cannot directly enqueue a turn from here; return the prompt as the
        // outcome so the caller (REPL) can surface it. To actually trigger an
        // agent, we push it onto a shared fix-request queue the REPL drains.
        let mut q = FIX_QUEUE.lock().unwrap();
        q.push(prompt.clone());
        Outcome::ok(format!(
            "queued an agent-assisted fix for '{ext_id}' (needs: {features}). The next idle turn will \
             attempt to adapt it to the pir ABI.\n\nFix prompt:\n{prompt}"
        ))
    }
}

/// Shared queue of agent-assisted fix prompts. The REPL drains this after a turn
/// (like `on_turn_end` follow-ups) and runs each as a new turn.
static FIX_QUEUE: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Drain queued fix prompts (called by the REPL/main after tool execution).
pub fn take_fix_prompts() -> Vec<String> {
    std::mem::take(&mut *FIX_QUEUE.lock().unwrap())
}

pub fn register(reg: &mut Registry) {
    reg.add(Box::new(PiExtensions::new()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolve the bundled sample extension dir for an integration test. Tries
    /// the shim's own directory then the repo-relative path.
    fn sample_ext_dir() -> Option<PathBuf> {
        let cand = [
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("extensions/pi-extensions/sample-ext"),
            PathBuf::from("extensions/pi-extensions/sample-ext"),
        ];
        cand.into_iter().find(|p| p.join("index.js").exists())
    }

    /// True when `node` is on PATH (and the integration test is explicitly
    /// enabled via PIR_PI_EXT_TEST=1), so the live shim bridge is exercised.
    fn live_shim_ok() -> bool {
        if std::env::var_os("PIR_PI_EXT_TEST").is_none() {
            return false;
        }
        // Probe node without spawning the shim.
        std::process::Command::new(node_bin())
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn disabled_backend_registers_nothing() {
        // Without PIR_PI_EXTENSIONS the backend is inert.
        std::env::remove_var("PIR_PI_EXTENSIONS");
        let e = PiExtensions::new();
        assert!(!e.enabled);
        assert!(e.specs().is_empty());
        assert!(e.commands().is_empty());
    }

    #[test]
    fn namespaced_naming_and_gap_queue() {
        let mut e = PiExtensions::new();
        // Force enabled for the pure-logic parts that don't touch the shim.
        e.enabled = true;
        let t = ExtTool {
            ext_id: "demo".into(),
            tool: "greet".into(),
            description: "say hi".into(),
            schema: json!({}),
        };
        assert_eq!(e.namespaced_tool_name(&t), "piext_demo__greet");

        // offer_fix with no recorded gaps is a no-op (does not touch the queue).
        let before = take_fix_prompts();
        let out = e.offer_fix("demo");
        assert!(out.content.contains("no recorded compatibility gaps"));
        assert!(take_fix_prompts().is_empty());
        let _ = before;
    }

    #[test]
    fn shim_roundtrip_load_call_command() {
        if !live_shim_ok() {
            // Skip: node not available or PIR_PI_EXT_TEST not set. This keeps
            // the default `cargo test` run fast and offline; run with
            // PIR_PI_EXT_TEST=1 to exercise the live Node.js bridge.
            eprintln!("skipping live shim test (set PIR_PI_EXT_TEST=1 and have node on PATH)");
            return;
        }
        let dir = match sample_ext_dir() {
            Some(d) => d,
            None => {
                eprintln!("skipping live shim test: sample-ext not found");
                return;
            }
        };
        let mut e = PiExtensions::new();
        e.enabled = true;

        // install
        let summary = e.install_extension(dir.to_str().unwrap()).expect("install");
        // The sample declares ctx.ui.input (unsupported) so a HARD gap is
        // expected and surfaced in the summary.
        assert!(summary.contains("sample-ext"), "summary: {summary}");
        assert!(
            summary.contains("ctx.ui.input") || summary.contains("compatibility"),
            "expected a compatibility warning, got: {summary}"
        );

        // specs/commands now include the namespaced entries
        let specs: Vec<String> = e.specs().iter().map(|s| s.name.to_string()).collect();
        assert!(specs.contains(&"piext_sample-ext__greet".to_string()));
        assert!(specs.contains(&"piext_sample-ext__add".to_string()));
        let cmds: Vec<String> = e.commands().iter().map(|c| c.name.clone()).collect();
        assert!(cmds.contains(&"piext_sample-ext__sample-ping".to_string()));

        // call greet tool
        let r = e.call_tool("sample-ext", "greet", &json!({ "name": "pir" }));
        assert!(r.is_ok(), "call_tool err: {r:?}");
        assert!(r.unwrap().contains("hello, pir!"), "greet output");

        // call add tool
        let r = e.call_tool("sample-ext", "add", &json!({ "a": 2, "b": 3 }));
        assert!(r.is_ok());
        assert!(r.unwrap().contains("5"), "add output");

        // run command
        let r = e.run_command("sample-ext", "sample-ping", "x");
        assert!(r.is_ok());
        assert_eq!(r.unwrap().trim(), "pong x");
    }
}
