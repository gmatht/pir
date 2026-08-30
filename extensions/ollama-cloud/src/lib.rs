//! Native Rust equivalent of the `pi-ollama-cloud` *pi* extension.
//!
//! `pi-ollama-cloud` is a **TypeScript** extension for the real `pi` coding
//! agent — it calls `pi.registerProvider(...)`, `pi.registerTool(...)` and
//! `pi.registerCommand(...)` from `@earendil-works/pi-coding-agent`. pir has
//! no TypeScript / pi-extension runtime; its "extension layer" is
//! compile-time-linked *native Rust* backends (`crate::plugin::ToolBackend`).
//! So the npm package cannot be loaded as-is. This backend is the faithful
//! native port of what the package does:
//!
//!   * the `ollama-cloud` provider itself is synthesized in `crate::config`
//!     (`merge_ollama_cloud`) from the package's baked-in 18-model fallback
//!     catalog, so `/model` lists it on first launch with no network call;
//!   * this backend contributes the two web tools the package registers
//!     (`ollama_web_search`, `ollama_web_fetch`) that hit Ollama Cloud's
//!     `/api/web_search` and `/api/web_fetch` endpoints;
//!   * it contributes the two slash commands the package exposes
//!     (`/ollama-webtools` to toggle the web tools, `/ollama-cloud-usage` to
//!     show session/weekly usage).
//!
//! API-key resolution mirrors `web-tools.ts` / `utils.ts`: prefer
//! `OLLAMA_API_KEY`, then a `ollama-cloud` entry in `~/.pi/agent/auth.json`,
//! then `~/.pi/agent/ollama-cloud.json`.
//!
//! See https://pi.dev/packages/pi-ollama-cloud and
//! https://github.com/fgrehm/pi-ollama-cloud

use crate::config::ollama_cloud_api_key;
use crate::plugin::{CommandSpec, Outcome, Registry, ToolBackend, ToolSpec};
use serde_json::{json, Value};
use std::io::Read as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Base URL for the Ollama Cloud API. Mirrors the package's
/// `OLLAMA_BASE` (`https://ollama.com`, with the cloud host hard-coded — local
/// Ollama daemons are a different product).
const OLLAMA_BASE: &str = "https://ollama.com";

/// Per-request timeout for the web tools (the package uses 15s).
const WEB_TOOLS_TIMEOUT: Duration = Duration::from_secs(15);

/// Like the package's `resolveWebToolsEnv()`: an env kill-switch. When
/// `PI_OLLAMA_WEB_TOOLS=false` the tools are registered (so `pi` knows about
/// them) but forced off, and no re-enable command is offered.
fn web_tools_env_enabled() -> bool {
    match std::env::var("PI_OLLAMA_WEB_TOOLS") {
        Ok(v) => !matches!(v.trim().to_lowercase().as_str(), "false" | "0" | "off" | "no"),
        Err(_) => true,
    }
}

/// Parse the toggle argument for `/ollama-webtools` and `/ollama-usage-status`.
/// Mirrors the package's `resolveUsageStatusToggle` (on/off/enable/disable,
/// empty = toggle). Returns `None` for an unknown argument (so the caller can
/// surface a usage error) — unlike the package we distinguish "toggle" (Some)
/// from "no change due to bad input" (None).
fn resolve_toggle(arg: &str, current: bool) -> Option<bool> {
    match arg.trim().to_lowercase().as_str() {
        "" => Some(!current),
        "on" | "enable" | "true" | "1" => Some(true),
        "off" | "disable" | "0" => Some(false),
        _ => None,
    }
}

pub fn register(reg: &mut Registry) {
    let env_on = web_tools_env_enabled();
    reg.add(Box::new(OllamaCloud {
        web_tools_active: Arc::new(AtomicBool::new(env_on)),
        // The env kill-switch forces the tools off regardless of toggle.
        env_enabled: env_on,
    }));
}

struct OllamaCloud {
    web_tools_active: Arc<AtomicBool>,
    env_enabled: bool,
}

impl OllamaCloud {
    /// Restrict a tool to the active Ollama Cloud provider? No — the tools work
    /// with any provider as long as an Ollama Cloud key is configured (the
    /// package attaches them to the active tool set, not gated on provider).
    /// We only gate on the runtime toggle + env kill-switch.
    fn tools_enabled(&self) -> bool {
        self.env_enabled && self.web_tools_active.load(Ordering::SeqCst)
    }
}

impl ToolBackend for OllamaCloud {
    fn name(&self) -> &'static str {
        "ollama-cloud"
    }

    fn specs(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "ollama_web_search",
                description: "Search the web for real-time information using Ollama Cloud's web \
                              search API. Returns relevant results with titles, URLs, and content \
                              snippets. Requires an Ollama Cloud API key (OLLAMA_API_KEY, or a \
                              'ollama-cloud' auth.json / ollama-cloud.json entry).",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "The search query to execute" },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum number of search results to return (default 5, max 10)",
                            "minimum": 1,
                            "maximum": 10
                        }
                    },
                    "required": ["query"]
                }),
            },
            ToolSpec {
                name: "ollama_web_fetch",
                description: "Fetch and extract text content from a web page URL using Ollama Cloud's \
                              web fetch API. Returns the page title, main content, and links found on \
                              the page. Requires an Ollama Cloud API key (OLLAMA_API_KEY, or a \
                              'ollama-cloud' auth.json / ollama-cloud.json entry).",
                schema: json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "URL to fetch and extract content from" }
                    },
                    "required": ["url"]
                }),
            },
        ]
    }

    fn run(&mut self, name: &str, input: &serde_json::Value) -> Outcome {
        if !self.tools_enabled() {
            return Outcome::err(
                "Ollama Cloud web tools are disabled. Enable them with /ollama-webtools on \
                 (or set OLLAMA_API_KEY and restart pir)."
                    .into(),
            );
        }
        match name {
            "ollama_web_search" => web_search(input),
            "ollama_web_fetch" => web_fetch(input),
            other => Outcome::err(format!("unknown tool '{other}'")),
        }
    }

    fn commands(&self) -> Vec<CommandSpec> {
        let mut cmds = vec![CommandSpec {
            name: "ollama-cloud-usage".into(),
            description: "Show Ollama Cloud session and weekly usage limits (undocumented /api/usage).".into(),
        }];
        // The package only registers the runtime toggle when the env var does
        // not hard-disable the tools.
        if self.env_enabled {
            cmds.push(CommandSpec {
                name: "ollama-webtools".into(),
                description: "Enable or disable Ollama Cloud web tools (ollama_web_search, \
                              ollama_web_fetch). Optional arg: on/off/enable/disable; no arg toggles."
                    .into(),
            });
        }
        cmds
    }

    fn run_command(&mut self, name: &str, args: &str) -> Outcome {
        match name {
            "ollama-webtools" => {
                if !self.env_enabled {
                    return Outcome::err(
                        "Ollama Cloud web tools are hard-disabled via PI_OLLAMA_WEB_TOOLS=false."
                            .into(),
                    );
                }
                match resolve_toggle(args, self.tools_enabled()) {
                    None => Outcome::err(
                        "Unknown argument. Usage: /ollama-webtools [on|off|enable|disable]".into(),
                    ),
                    Some(next) => {
                        self.web_tools_active.store(next, Ordering::SeqCst);
                        Outcome::ok(format!(
                            "Ollama Cloud web tools: {}",
                            if next { "enabled" } else { "disabled" }
                        ))
                    }
                }
            }
            "ollama-cloud-usage" => show_usage(),
            other => Outcome::err(format!("unknown command '{other}'")),
        }
    }

    fn startup_report(&mut self) -> Option<String> {
        let key = ollama_cloud_api_key();
        if key.is_none() {
            return None; // no key -> provider wasn't registered; nothing to say
        }
        let state = if self.tools_enabled() { "enabled" } else { "disabled" };
        Some(format!(
            "[ollama-cloud] provider ready; web tools {state} —ollama-webtools, /ollama-cloud-usage"
        ))
    }
}

// ---------------------------------------------------------------------------
// Web tools
// ---------------------------------------------------------------------------

/// POST `body` to `OLLAMA_BASE/endpoint` with the bearer key, with a hard
/// timeout. Returns the parsed JSON response. Errors are mapped to readable
/// strings (terminal-safe; no HTML dumped).
fn ollama_post(endpoint: &str, key: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
    let url = format!("{}{}", OLLAMA_BASE, endpoint);
    // ureq 2.x: attach native-tls explicitly (mirrors crate::provider).
    let connector = std::sync::Arc::new(
        native_tls::TlsConnector::new().map_err(|e| format!("ollama_cloud: TLS init failed: {e}"))?,
    );
    let agent = ureq::AgentBuilder::new()
        .tls_connector(connector)
        .timeout(WEB_TOOLS_TIMEOUT)
        .build();
    let resp = agent
        .post(&url)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json")
        .send_json(body);
    match resp {
        Ok(r) => r
            .into_json::<serde_json::Value>()
            .map_err(|e| format!("ollama_cloud: bad JSON from {endpoint}: {e}")),
        Err(ureq::Error::Status(code, r)) => {
            let mut b = String::new();
            let _ = r.into_reader().take(1024).read_to_string(&mut b);
            let detail = crate::provider::http_status_detail(code, &b);
            Err(format!("ollama_cloud {endpoint}: {detail}"))
        }
        Err(e) => Err(format!("ollama_cloud {endpoint}: {e}")),
    }
}

fn require_key() -> Result<String, String> {
    match ollama_cloud_api_key() {
        Some(k) if !k.is_empty() => Ok(k),
        _ => Err(
            "No Ollama Cloud API key configured. Set OLLAMA_API_KEY, or add a 'ollama-cloud' \
             entry to ~/.pi/agent/auth.json, or ~/.pi/agent/ollama-cloud.json."
                .into(),
        ),
    }
}

fn web_search(input: &serde_json::Value) -> Outcome {
    let query = match input["query"].as_str() {
        Some(q) if !q.trim().is_empty() => q,
        _ => return Outcome::err("ollama_web_search: missing 'query'".into()),
    };
    let max_results = input["max_results"]
        .as_u64()
        .unwrap_or(5)
        .clamp(1, 10) as u32;
    let key = match require_key() {
        Ok(k) => k,
        Err(e) => return Outcome::err(e),
    };
    let resp = match ollama_post(
        "/api/web_search",
        &key,
        json!({ "query": query, "max_results": max_results }),
    ) {
        Ok(v) => v,
        Err(e) => return Outcome::err(e),
    };
    // Validate the shape (package's isSearchResponse): { results: [{title,url,content}] }.
    let results = resp
        .get("results")
        .and_then(Value::as_array)
        .filter(|a| {
            a.iter().all(|r| {
                r.get("title").and_then(Value::as_str).is_some()
                    && r.get("url").and_then(Value::as_str).is_some()
                    && r.get("content").and_then(Value::as_str).is_some()
            })
        });
    let Some(results) = results else {
        return Outcome::err("ollama_web_search: unexpected response shape from the API.".into());
    };
    if results.is_empty() {
        return Outcome::ok("No results found.".into());
    }
    let mut out = String::new();
    for (i, r) in results.iter().enumerate() {
        let title = r["title"].as_str().unwrap_or_default();
        let url = r["url"].as_str().unwrap_or_default();
        let content = r["content"].as_str().unwrap_or_default();
        out.push_str(&format!("{}. {}\n   URL: {}\n   {}\n\n", i + 1, title, url, content));
    }
    Outcome::ok(out.trim_end().to_string())
}

fn web_fetch(input: &serde_json::Value) -> Outcome {
    let url = match input["url"].as_str() {
        Some(u) if !u.trim().is_empty() => u,
        _ => return Outcome::err("ollama_web_fetch: missing 'url'".into()),
    };
    let key = match require_key() {
        Ok(k) => k,
        Err(e) => return Outcome::err(e),
    };
    let resp = match ollama_post("/api/web_fetch", &key, json!({ "url": url })) {
        Ok(v) => v,
        Err(e) => return Outcome::err(e),
    };
    // Validate (package's isFetchResponse): { title:str, content:str, links:[str]|null }.
    let ok = resp.get("title").and_then(Value::as_str).is_some()
        && resp.get("content").and_then(Value::as_str).is_some()
        && (resp.get("links").map(Value::is_null).unwrap_or(false)
            || resp
                .get("links")
                .and_then(Value::as_array)
                .map(|a| a.iter().all(|l| l.as_str().is_some()))
                .unwrap_or(false));
    if !ok {
        return Outcome::err("ollama_web_fetch: unexpected response shape from the API.".into());
    }
    let title = resp["title"].as_str().unwrap_or_default();
    let content = resp["content"].as_str().unwrap_or_default();
    let links = resp["links"].as_array().map(|a| {
        a.iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>()
    });
    let mut out = String::new();
    out.push_str(&format!("Title: {title}\n\nContent:\n{content}\n"));
    match &links {
        Some(l) => {
            out.push_str(&format!("\nLinks found: {}\n", l.len()));
            for l in l.iter().take(10) {
                out.push_str(&format!("  - {l}\n"));
            }
        }
        None => {
            out.push_str("\nLinks found: 0\n");
        }
    }
    Outcome::ok(out.trim_end().to_string())
}

// ---------------------------------------------------------------------------
// Usage command (/ollama-cloud-usage)
// ---------------------------------------------------------------------------

/// Fetch and format Ollama Cloud usage from the undocumented `/api/usage`
/// endpoint. Mirrors `usage.ts`: distinct statuses map to distinct messages,
/// and a non-conforming body is surfaced rather than dumped.
fn show_usage() -> Outcome {
    let key = match require_key() {
        Ok(k) => k,
        Err(e) => return Outcome::err(e),
    };
    let resp = match ollama_post("/api/usage", &key, json!({})) {
        Ok(v) => v,
        Err(e) => return Outcome::err(e),
    };
    // Expect { session: {usage, limit}, weekly: {usage, limit} } (fractions 0..1).
    let session = resp.get("session");
    let weekly = resp.get("weekly");
    if session.is_none() && weekly.is_none() {
        return Outcome::err("ollama_cloud usage: unexpected response shape from the API.".into());
    }
    let pct = |f: f64| -> u32 {
        if !f.is_finite() {
            0
        } else {
            (f.clamp(0.0, 1.0) * 100.0).round() as u32
        }
    };
    let fmt = |label: &str, node: Option<&Value>| -> String {
        match node {
            None => format!("{label}: n/a"),
            Some(n) => {
                let usage = n.get("usage").and_then(Value::as_f64).unwrap_or(0.0);
                let limit = n.get("limit").and_then(Value::as_f64).unwrap_or(0.0);
                format!("{label}: {}% ({:.2}/{:.2})", pct(usage), usage, limit)
            }
        }
    };
    let mut out = String::from("Ollama Cloud usage (subscription-billed; estimates):\n");
    out.push_str(&fmt("  session", session));
    out.push('\n');
    out.push_str(&fmt("  weekly", weekly));
    Outcome::ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn ext() -> OllamaCloud {
        OllamaCloud {
            web_tools_active: Arc::new(AtomicBool::new(true)),
            env_enabled: true,
        }
    }

    #[test]
    fn registers_two_web_tools() {
        let specs = ext().specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name).collect();
        assert!(names.contains(&"ollama_web_search"));
        assert!(names.contains(&"ollama_web_fetch"));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn registers_webtools_and_usage_commands() {
        let cmds: Vec<String> = ext().commands().into_iter().map(|c| c.name).collect();
        assert!(cmds.contains(&"ollama-webtools".to_string()));
        assert!(cmds.contains(&"ollama-cloud-usage".to_string()));
    }

    #[test]
    fn env_kill_switch_hides_toggle_command() {
        let e = OllamaCloud {
            web_tools_active: Arc::new(AtomicBool::new(true)),
            env_enabled: false,
        };
        let cmds: Vec<String> = e.commands().into_iter().map(|c| c.name).collect();
        assert!(!cmds.contains(&"ollama-webtools".to_string()));
        assert!(cmds.contains(&"ollama-cloud-usage".to_string()));
        // And the tools are reported disabled.
        assert!(!e.tools_enabled());
    }

    #[test]
    fn webtools_toggle_on_off_and_bad_arg() {
        let mut e = ext();
        // Toggle from enabled (default true) -> off.
        let off = e.run_command("ollama-webtools", "");
        assert!(off.content.contains("disabled"));
        assert!(!e.tools_enabled());
        // Explicit on.
        let on = e.run_command("ollama-webtools", "on");
        assert!(on.content.contains("enabled"));
        assert!(e.tools_enabled());
        // Bad arg -> error, state unchanged.
        let bad = e.run_command("ollama-webtools", "maybe");
        assert!(bad.is_error);
        assert!(e.tools_enabled()); // on
    }

    #[test]
    fn disabled_tools_refuse_to_run() {
        let mut e = OllamaCloud {
            web_tools_active: Arc::new(AtomicBool::new(false)),
            env_enabled: true,
        };
        let r = e.run("ollama_web_search", &json!({ "query": "rust" }));
        assert!(r.is_error);
        assert!(r.content.contains("disabled"));
    }

    #[test]
    fn search_requires_query() {
        // With a key present (env), missing query must error before any network.
        std::env::set_var("OLLAMA_API_KEY", "test-key");
        let mut e = ext();
        let r = e.run("ollama_web_search", &json!({}));
        assert!(r.is_error);
        assert!(r.content.contains("missing 'query'"));
        std::env::remove_var("OLLAMA_API_KEY");
        let _ = &mut e;
    }

    #[test]
    fn resolve_toggle_semantics() {
        assert_eq!(resolve_toggle("", true), Some(false)); // toggle
        assert_eq!(resolve_toggle("", false), Some(true));
        assert_eq!(resolve_toggle("on", false), Some(true));
        assert_eq!(resolve_toggle("disable", true), Some(false));
        assert_eq!(resolve_toggle("bogus", true), None);
    }

    #[test]
    fn missing_key_surfaces_clear_error() {
        std::env::remove_var("OLLAMA_API_KEY");
        // Ensure no auth.json / ollama-cloud.json in HOME during the test.
        let dir = std::env::temp_dir().join("pir-test-no-ollama");
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("HOME", &dir);
        let r = show_usage();
        assert!(r.is_error);
        assert!(r.content.contains("No Ollama Cloud API key"), "got: {}", r.content);
        std::env::set_var("HOME", "/"); // harmless restore (tests don't depend on it)
    }
}
