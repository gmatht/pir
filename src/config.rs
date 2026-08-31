use serde::Deserialize;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKind {
    Anthropic,
    OpenAi,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Provider {
    pub id: Option<String>,
    pub name: Option<String>,
    #[serde(alias = "baseUrl", alias = "url")]
    pub base_url: Option<String>,
    #[serde(alias = "apiKey", alias = "key")]
    pub api_key: Option<String>,
    pub api: Option<String>,
    #[serde(default)]
    pub models: Vec<Model>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: Option<String>,
    pub context: Option<u64>,
    #[serde(alias = "maxTokens")]
    pub max_tokens: Option<u64>,
    /// Optional per-model API override ("openai", "anthropic", "openai-responses",
    /// "google"). When set it wins over the provider-level `api`; used by the
    /// built-in OpenCode Zen catalog where the API varies per model.
    #[serde(default)]
    #[serde(skip_deserializing)]
    pub api_override: Option<String>,
    /// Optional per-model request URL override (full base URL for this model).
    #[serde(default)]
    #[serde(skip_deserializing)]
    pub url_override: Option<String>,
    /// Whether this model accepts an OpenAI `reasoning_effort` field. `None`
    /// means "decide from the API kind as before".
    #[serde(default)]
    #[serde(skip_deserializing)]
    pub no_reasoning_effort: bool,
    /// Optional per-1k-token price (USD) for input/output, used by the
    /// cost/price tracking in `Usage::cost`. Set via `set_price` after loading
    /// from a user-supplied price map; not read from the provider config.
    #[serde(skip)]
    pub price_per_1k: Option<(f64, f64)>,
}

impl Model {
    /// Attach a (input $/1k, output $/1k) price tuple. Returns `&mut Self` so it
    /// can be chained when building the model list.
    pub fn with_price(mut self, input: f64, output: f64) -> Self {
        self.price_per_1k = Some((input, output));
        self
    }
}

/// Reasoning / "extended thinking" level for models that support it (Anthropic
/// Claude, OpenAI o-series, etc.). `Off` disables thinking entirely; the other
/// levels scale the reasoning budget (Anthropic) or `reasoning_effort`
/// (OpenAI). Parsed case-insensitively from `/thinking`, `--thinking`, or
/// `PIR_THINKING`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ThinkingLevel {
    /// Parse a level name (case-insensitive). Accepts a few synonyms.
    pub fn parse(s: &str) -> Option<ThinkingLevel> {
        match s.trim().to_lowercase().as_str() {
            "off" | "none" | "false" | "0" | "disable" | "disabled" => Some(ThinkingLevel::Off),
            "min" | "minimal" | "tiny" => Some(ThinkingLevel::Minimal),
            "low" => Some(ThinkingLevel::Low),
            "med" | "medium" => Some(ThinkingLevel::Medium),
            "high" => Some(ThinkingLevel::High),
            "xhigh" | "x-high" | "extra" => Some(ThinkingLevel::XHigh),
            "max" | "maximum" => Some(ThinkingLevel::Max),
            _ => None,
        }
    }

    /// The canonical name of this level (used for display + persistence).
    pub fn as_str(&self) -> &'static str {
        match self {
            ThinkingLevel::Off => "off",
            ThinkingLevel::Minimal => "minimal",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::XHigh => "xhigh",
            ThinkingLevel::Max => "max",
        }
    }

    /// Whether this level enables any extended thinking at all.
    pub fn enabled(&self) -> bool {
        !matches!(self, ThinkingLevel::Off)
    }

    /// Anthropic thinking budget (in tokens) for this level, given the model's
    /// context window. Returns `None` when thinking is disabled or the context
    /// is too small to afford a meaningful budget. The caller must ensure the
    /// budget stays strictly below `max_tokens` (Anthropic requires it).
    pub fn anthropic_budget(&self, ctx: u64) -> Option<u64> {
        let c = ctx.max(1);
        let b = match self {
            ThinkingLevel::Off => return None,
            ThinkingLevel::Minimal => return Some(1024),
            ThinkingLevel::Low => c / 32,
            ThinkingLevel::Medium => c / 12,
            ThinkingLevel::High => c / 6,
            ThinkingLevel::XHigh => c / 3,
            ThinkingLevel::Max => (c * 2) / 3,
        };
        if b < 1024 {
            None
        } else {
            Some(b)
        }
    }

    /// OpenAI `reasoning_effort` value for this level, or `None` when thinking
    /// is disabled. (Anthropic maps the same levels to a token budget instead;
    /// OpenAI only exposes coarse effort levels.)
    pub fn oai_effort(&self) -> Option<&'static str> {
        match self {
            ThinkingLevel::Off | ThinkingLevel::Minimal => None,
            ThinkingLevel::Low => Some("low"),
            ThinkingLevel::Medium => Some("medium"),
            ThinkingLevel::High | ThinkingLevel::XHigh | ThinkingLevel::Max => Some("high"),
        }
    }

    /// Whether selecting this level actually takes effect for the given
    /// provider kind + context window. Some levels have no effect on certain
    /// providers and would be silently ignored (`minimal` has no OpenAI
    /// `reasoning_effort`; Anthropic budget levels degrade to nothing when the
    /// context is too small to afford a budget). Filtering these out of a
    /// picker (/menu → thinking) avoids offering options that do nothing.
    /// Unknown provider kinds (`None`) always show every level.
    pub fn effective(&self, kind: Option<ApiKind>, ctx: u64) -> bool {
        match self {
            ThinkingLevel::Off => true,
            _ => match kind {
                Some(ApiKind::OpenAi) => self.oai_effort().is_some(),
                Some(ApiKind::Anthropic) => self.anthropic_budget(ctx).is_some(),
                None => true,
            },
        }
    }
}

impl Provider {
    pub fn pid(&self) -> String {
        if let Some(id) = &self.id { return id.clone(); }
        if let Some(n) = &self.name { return n.to_lowercase().replace(' ', "-"); }
        "custom".into()
    }

    pub fn label(&self, m: &Model) -> String {
        format!("{}/{}", self.pid(), m.id)
    }

    pub fn kind(&self) -> Option<ApiKind> {
        let api = self.api.as_deref().map(str::to_lowercase);
        let base = self.base_url.as_deref().unwrap_or_default().to_lowercase();
        match api.as_deref() {
            Some(a) if a.contains("anthropic") => Some(ApiKind::Anthropic),
            Some(_) => Some(ApiKind::OpenAi),
            None if base.contains("anthropic.com") => Some(ApiKind::Anthropic),
            None if base.is_empty() => None,
            None => Some(ApiKind::OpenAi),
        }
    }

    pub fn api_key(&self) -> Option<String> {
        self.api_key.as_ref().and_then(|k| expand_env(k))
    }

    /// The effective API for `model`: the model's own override when present
    /// (OpenCode Zen's per-model mapping), else the provider-level `api`.
    pub fn model_api(&self, model: &Model) -> Option<ApiKind> {
        if let Some(api) = &model.api_override {
            let a = api.to_lowercase();
            return if a.contains("anthropic") {
                Some(ApiKind::Anthropic)
            } else {
                Some(ApiKind::OpenAi)
            };
        }
        self.kind()
    }

    /// The effective request base URL for `model`: the model's own override
    /// when present, else the provider-level `baseUrl`.
    pub fn model_base_url<'a>(&'a self, model: &'a Model) -> Option<&'a str> {
        model
            .url_override
            .as_deref()
            .or(self.base_url.as_deref())
            .filter(|s| !s.is_empty())
    }
}

/// Expand a `{env:VAR}` reference. Returns `None` when `s` begins with
/// `{env:` but the named variable is unset/empty, so callers can surface a
/// clear "missing API key env var" error instead of silently failing later
/// with an opaque "no API key". Non-`{env:...}` values pass through unchanged.
pub fn expand_env(s: &str) -> Option<String> {
    if let Some(var) = s.strip_prefix("{env:").and_then(|r| r.strip_suffix('}')) {
        let v = std::env::var(var).unwrap_or_default();
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    } else {
        Some(s.to_string())
    }
}

pub fn pi_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("PI_DIR") {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".pi")
}

/// Convert a config string (possibly with a leading `~/`) into a `PathBuf`.
/// Expands a leading `~/` to the user's home directory; everything else is taken
/// literally so absolute and relative paths both work. Used by the security
/// quarantine config keys (`quarantine-staging`, `overlay`, …).
pub fn path_from_string(s: &str) -> PathBuf {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
}

pub fn load_providers() -> Result<Vec<Provider>, String> {
    let path = pi_dir().join("agent").join("models-store.json");
    
    if !path.exists() {
        eprintln!("! models-store.json not found. Falling back to auth.json");
        return load_from_auth_fallback();
    }

    let raw = match fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("! Cannot read models-store.json: {e}. Falling back");
            return load_from_auth_fallback();
        }
    };

    let v: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("! Cannot parse models-store.json: {e}. Falling back");
            return load_from_auth_fallback();
        }
    };

    let providers_val = v.get("providers").cloned().unwrap_or(v);
    let mut providers = Vec::new();

    let provider_iter: Vec<(String, &Value)> = if let Some(arr) = providers_val.as_array() {
        arr.iter().enumerate().map(|(i, p)| {
            let id = p.get("id").or(p.get("name")).and_then(Value::as_str).unwrap_or(&format!("provider-{}", i)).to_string();
            (id, p)
        }).collect()
    } else if let Some(obj) = providers_val.as_object() {
        obj.iter().map(|(k, p)| {
            let id = p.get("id").and_then(Value::as_str).unwrap_or(k).to_string();
            (id, p)
        }).collect()
    } else {
        return Err("models-store.json has invalid format".into());
    };

    let auth_keys = load_auth_keys();

    for (pid, pval) in provider_iter {
        let mut base_url = pval.get("baseUrl").or(pval.get("base_url")).or(pval.get("url")).and_then(Value::as_str).map(String::from);
        let mut api = pval.get("api").and_then(Value::as_str).map(String::from);
        
        let api_key = pval.get("apiKey").or(pval.get("key")).and_then(Value::as_str).map(String::from)
            .or_else(|| auth_keys.get(&pid.to_lowercase()).cloned());

        let mut models = Vec::new();
        if let Some(m) = pval.get("models") {
            if let Some(arr) = m.as_array() {
                for mv in arr {
                    // CRITICAL FIX: If baseUrl is missing at the provider level, 
                    // steal it from the individual model definition
                    if base_url.is_none() {
                        base_url = mv.get("baseUrl").or(mv.get("base_url")).and_then(Value::as_str).map(String::from);
                    }
                    if api.is_none() {
                        api = mv.get("api").and_then(Value::as_str).map(String::from);
                    }

                    if let Some(id) = mv.get("id").and_then(Value::as_str) {
                        models.push(Model {
                            id: id.to_string(),
                            name: mv.get("name").and_then(Value::as_str).map(String::from),
                            context: mv.get("context").or(mv.get("contextWindow")).and_then(Value::as_u64),
                            max_tokens: mv.get("maxTokens").or(mv.get("max_tokens")).and_then(Value::as_u64),
                            api_override: None,
                            url_override: None,
                            no_reasoning_effort: false,
                            price_per_1k: None,
                        });
                    }
                }
            } else if let Some(obj) = m.as_object() {
                for (mid, mv) in obj {
                    if base_url.is_none() {
                        base_url = mv.get("baseUrl").or(mv.get("base_url")).and_then(Value::as_str).map(String::from);
                    }
                    if api.is_none() {
                        api = mv.get("api").and_then(Value::as_str).map(String::from);
                    }

                    models.push(Model {
                        id: mid.clone(),
                        name: mv.get("name").and_then(Value::as_str).map(String::from),
                        context: mv.get("context").or(mv.get("contextWindow")).and_then(Value::as_u64),
                        max_tokens: mv.get("maxTokens").or(mv.get("max_tokens")).and_then(Value::as_u64),
                        api_override: None,
                        url_override: None,
                        no_reasoning_effort: false,
                        price_per_1k: None,
                    });
                }
            }
        }

        if !models.is_empty() {
            providers.push(Provider {
                id: Some(pid),
                name: pval.get("name").and_then(Value::as_str).map(String::from),
                base_url,
                api_key,
                api,
                models,
            });
        }
    }

    if providers.is_empty() {
        eprintln!("! models-store.json had 0 valid providers. Falling back");
        return load_from_auth_fallback();
    }

    apply_prices(&mut providers);
    merge_ollama_cloud(&mut providers);
    Ok(providers)
}

/// Merge the `ollama-cloud` provider (if not already present) into the catalog.
///
/// `pi-ollama-cloud` is a *pi* (TypeScript) extension and cannot run under
/// pir's compile-time-linked native extension layer. This is the native Rust
/// equivalent: the `ollama-cloud` provider is synthesized from the package's
/// baked-in fallback model list (so `/model` shows it on first launch without
/// any network call — exactly the "generated fallback" the package ships), and
/// the `extensions/ollama-cloud` backend contributes the matching
/// `ollama_web_search` / `ollama_web_fetch` tools and slash commands.
///
/// We only synthesize the provider when there is *some* way to authenticate
/// (env key, `auth.json` entry package's own `~/.pi/agent/ollama-cloud.json`), unattended installs without an Ollama Cloud key don't get a provider that
/// can never complete a request. If the user later adds a key, the next `pir`
/// launch picks it up. The package itself always registers the provider and
/// fails only at request time; we're slightly stricter to avoid a dead entry.
pub fn merge_ollama_cloud(providers: &mut Vec<Provider>) {
    if providers.iter().any(|p| p.pid() == "ollama-cloud") {
        return; // user already declared it (e.g. in models-store.json)
    }
    let key = ollama_cloud_api_key();
    let Some(key) = key else { return };
    if key.is_empty() {
        return;
    }
    let models = ollama_cloud_models();
    if models.is_empty() {
        return;
    }
    providers.push(Provider {
        id: Some("ollama-cloud".into()),
        name: Some("Ollama Cloud".into()),
        base_url: Some("https://ollama.com/v1".into()),
        api_key: Some(key),
        api: Some("openai".into()),
        models,
    });
}

/// Resolve the Ollama Cloud API key from the same sources the pi package and
/// pir's auth store consult, in priority order:
///   1. `OLLAMA_API_KEY` env var (the package's documented primary source)
///   2. an `ollama-cloud` entry in `~/.pi/agent/auth.json`
///   3. `~/.pi/agent/ollama-cloud.json` (`{ "apiKey": "..." }`, the package's
///      own per-extension config file)
/// Returns `None` when nothing is configured.
pub fn ollama_cloud_api_key() -> Option<String> {
    if let Ok(v) = std::env::var("OLLAMA_API_KEY") {
        if !v.is_empty() {
            return Some(v);
        }
    }
    if let Some(k) = load_auth_keys().get("ollama-cloud") {
        if !k.is_empty() {
            return Some(k.clone());
        }
    }
    // Package-style per-extension config: ~/.pi/agent/ollama-cloud.json
    let cfg = pi_dir().join("agent").join("ollama-cloud.json");
    if let Ok(raw) = fs::read_to_string(&cfg) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            if let Some(k) = v.get("apiKey").or(v.get("key")).and_then(Value::as_str) {
                if !k.is_empty() {
                    return Some(k.to_string());
                }
            }
        }
    }
    None
}

/// The baked-in Ollama Cloud model catalog (the 18-entry
/// `models.generated.ts` fallback shipped by `pi-ollama-cloud` 0.9.0). Only
/// tool-capable models are listed, matching the package's `tools` filter.
pub fn ollama_cloud_models() -> Vec<Model> {
    // (id, context_window, max_tokens). Context windows and max output tokens
    /// are copied verbatim from the package's generated fallback so `/model`
    /// shows the same catalog.
    const SPEC: &[(&str, u64, u64)] = &[
        ("deepseek-v4-flash:0731", 1_048_576, 32768),
        ("deepseek-v4-flash:preview", 1_048_576, 32768),
        ("deepseek-v4-pro", 524_288, 32768),
        ("gemma4:31b", 262_144, 32768),
        ("glm-5.1", 202_752, 32768),
        ("glm-5.2", 1_000_000, 32768),
        ("gpt-oss:120b", 131_072, 32768),
        ("gpt-oss:20b", 131_072, 32768),
        ("kimi-k2.6", 262_144, 32768),
        ("kimi-k2.7-code", 262_144, 32768),
        ("kimi-k3", 1_048_576, 32768),
        ("minimax-m2.7", 196_608, 32768),
        ("minimax-m3", 524_288, 32768),
        ("mistral-large-3:675b", 262_144, 32768),
        ("nemotron-3-nano:30b", 262_144, 32768),
        ("nemotron-3-super", 262_144, 32768),
        ("nemotron-3-ultra", 262_144, 32768),
        ("qwen3.5:397b", 262_144, 32768),
    ];
    SPEC
        .iter()
        .map(|(id, ctx, max)| Model {
            id: id.to_string(),
            name: Some(id.to_string()),
            context: Some(*ctx),
            max_tokens: Some(*max),
            api_override: None,
            url_override: None,
            no_reasoning_effort: false,
            price_per_1k: None,
        })
        .collect()
}

/// A small built-in table of per-1k-token USD prices (input, output) for common
/// models. Used only when the user hasn't supplied their own in
/// `~/.pi/agent/settings.json` (`prices` key). Prices are approximate reference
/// values and may be out of date; override them per-model in settings.
fn default_prices() -> std::collections::BTreeMap<String, (f64, f64)> {
    let mut m = std::collections::BTreeMap::new();
    // Anthropic (Claude 4 / 3.5-era list prices, USD per 1M tokens -> per 1k).
    for (id, p) in [
        ("claude-opus-4", (15.0, 75.0)),
        ("claude-sonnet-4", (3.0, 15.0)),
        ("claude-sonnet-4-5", (3.0, 15.0)),
        ("claude-3-5-sonnet", (3.0, 15.0)),
        ("claude-3-5-haiku", (0.80, 4.0)),
        ("claude-3-haiku", (0.25, 1.25)),
        ("claude-3-opus", (15.0, 75.0)),
    ] {
        m.insert(id.to_string(), (p.0 / 1000.0, p.1 / 1000.0));
    }
    // OpenAI.
    for (id, p) in [
        ("gpt-4o", (2.5, 10.0)),
        ("gpt-4o-mini", (0.15, 0.60)),
        ("gpt-4-turbo", (10.0, 30.0)),
        ("o1", (15.0, 60.0)),
        ("o3", (10.0, 40.0)),
        ("o4-mini", (1.10, 4.40)),
    ] {
        m.insert(id.to_string(), (p.0 / 1000.0, p.1 / 1000.0));
    }
    m
}

/// Enrich loaded providers' models with per-1k-token prices. User-supplied
/// prices from `~/.pi/agent/settings.json` (`prices`: { "provider/model":
/// [in, out] }) win over the built-in table; matching is by model id (case-
/// insensitive). Best-effort: any parse failure is silently ignored.
fn apply_prices(providers: &mut [Provider]) {
    let mut table = default_prices();
    // Merge user prices from settings.json.
    let p = pi_dir().join("agent").join("settings.json");
    if let Ok(raw) = fs::read_to_string(&p) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            if let Some(prices) = v.get("prices").and_then(Value::as_object) {
                for (label, pv) in prices {
                    if let Some(arr) = pv.as_array() {
                        if let (Some(i), Some(o)) = (arr.first().and_then(Value::as_f64), arr.get(1).and_then(Value::as_f64)) {
                            table.insert(label.to_lowercase(), (i, o));
                        }
                    }
                }
            }
        }
    }
    for prov in providers.iter_mut() {
        let pid = prov.pid();
        for m in prov.models.iter_mut() {
            let key = format!("{}/{}", pid, m.id).to_lowercase();
            let by_label = table.get(&key).copied();
            let by_id = table.get(&m.id.to_lowercase()).copied();
            if let Some((i, o)) = by_label.or(by_id) {
                m.price_per_1k = Some((i, o));
            }
        }
    }
}

fn load_from_auth_fallback() -> Result<Vec<Provider>, String> {
    let auth_path = pi_dir().join("agent").join("auth.json");
    let settings_path = pi_dir().join("agent").join("settings.json");
    
    let auth_raw = fs::read_to_string(&auth_path).map_err(|e| format!("Missing {}: {e}", auth_path.display()))?;
    let auth_v: Value = serde_json::from_str(&auth_raw).map_err(|e| format!("Parsing {}: {e}", auth_path.display()))?;
    
    let settings_raw = fs::read_to_string(&settings_path).unwrap_or_default();
    let settings_v: Value = serde_json::from_str(&settings_raw).unwrap_or(Value::Null);
    let default_model = settings_v.get("defaultModel").and_then(Value::as_str).unwrap_or("default-model").to_string();

    let mut providers = Vec::new();
    if let Some(obj) = auth_v.as_object() {
        for (id, val) in obj {
            if val.get("type").and_then(Value::as_str) == Some("api_key") {
                if let Some(key) = val.get("key").and_then(Value::as_str) {
                    if !key.is_empty() {
                        let pid = id.to_lowercase();
                        providers.push(Provider {
                            id: Some(id.clone()),
                            name: None,
                            base_url: guess_base_url(&pid),
                            api_key: Some(key.to_string()),
                            api: if pid.contains("anthropic") { Some("anthropic".into()) } else { Some("openai".into()) },
                            models: vec![Model {
                                id: default_model.clone(),
                                name: None,
                                context: Some(128000),
                                max_tokens: Some(8192),
                                api_override: None,
                                url_override: None,
                                no_reasoning_effort: false,
                                price_per_1k: None,
                            }],
                        });
                    }
                }
            }
        }
    }
    
    if providers.is_empty() { Err("No providers found in auth.json".into()) } else { Ok(providers) }
}

fn guess_base_url(pid: &str) -> Option<String> {
    let env_var = format!("{}_BASE_URL", pid.to_uppercase().replace('-', "_"));
    if let Ok(url) = std::env::var(&env_var) {
        if !url.is_empty() { return Some(url); }
    }

    if pid.contains("openrouter") { return Some("https://openrouter.ai/api/v1".into()); }
    if pid.contains("anthropic") { return Some("https://api.anthropic.com/v1".into()); }
    if pid.contains("openai") { return Some("https://api.openai.com/v1".into()); }
    
    None
}

fn load_auth_keys() -> std::collections::BTreeMap<String, String> {
    let path = pi_dir().join("agent").join("auth.json");
    let mut map = std::collections::BTreeMap::new();
    if let Ok(raw) = fs::read_to_string(&path) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            if let Some(obj) = v.as_object() {
                for (id, val) in obj {
                    if val.get("type").and_then(Value::as_str) == Some("api_key") {
                        if let Some(key) = val.get("key").and_then(Value::as_str) {
                            if !key.is_empty() { map.insert(id.to_lowercase(), key.to_string()); }
                        }
                    }
                }
            }
        }
    }
    map
}

pub fn default_model_setting() -> Option<String> {
    let p = pi_dir().join("agent").join("settings.json");
    let raw = fs::read_to_string(p).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    
    let provider = v.get("defaultProvider").and_then(Value::as_str)?;
    let model = v.get("defaultModel").and_then(Value::as_str)?;
    
    if provider.is_empty() || model.is_empty() { return None; }
    
    Some(format!("{}/{}", provider, model))
}

/// Persist a provider/model as the default for new pir sessions by writing it
/// into `~/.pi/agent/settings.json` under `defaultProvider`/`defaultModel`
/// (the keys [`default_model_setting`] reads at startup). Creates the file /
/// `agent` dir if missing, and preserves any other keys already present.
pub fn set_default_model(provider: &str, model: &str) -> Result<PathBuf, String> {
    let p = pi_dir().join("agent").join("settings.json");
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let mut v: Value = fs::read_to_string(&p)
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    if !v.is_object() {
        v = Value::Object(serde_json::Map::new());
    }
    let obj = v.as_object_mut().unwrap();
    obj.insert("defaultProvider".into(), Value::String(provider.to_string()));
    obj.insert("defaultModel".into(), Value::String(model.to_string()));
    fs::write(&p, serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok(p)
}

/// Path to `auth.json` (credential store). Mirrors the file pi writes;
/// `load_auth_keys` / `load_from_auth_fallback` already consult it.
pub fn auth_path() -> PathBuf {
    pi_dir().join("agent").join("auth.json")
}

/// The default selector for the *light* model used to summarize conversations
/// into a short title in the background. Cheap, fast models (Cerebras) keep
/// this within the provider's strict per-minute token/request limits; the
/// user can override it via `PIR_LIGHT_MODEL` or `~/.pi/agent/settings.json`
/// (`lightModel`).
pub const DEFAULT_LIGHT_MODEL: &str = "cerebras/gemma4";

/// Resolve the (provider, model) to use for light/background summarization
/// work (conversation titles). Resolution order:
///   1. `PIR_LIGHT_MODEL` env var
///   1. `PIR_LIGHT_MODEL` env var
///   2. `lightModel` in `~/.pi/agent/settings.json`
///   3. the built-in [`DEFAULT_LIGHT_MODEL`] (`cerebras/gemma4`)
///
/// Returns `None` when the resolved selector names a model that isn't present
/// in the loaded catalog (e.g. the user hasn't configured Cerebras yet), so the
/// caller can skip title generation rather than erroring. The returned
/// `(Provider, Model)` borrows from `providers` and must outlive the call.
pub fn resolve_light_model<'a>(
    providers: &'a [Provider],
) -> Option<(&'a Provider, &'a Model)> {
    let mut selector = std::env::var("PIR_LIGHT_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            let p = pi_dir().join("agent").join("settings.json");
            fs::read_to_string(&p)
                .ok()
                .and_then(|r| serde_json::from_str::<Value>(&r).ok())
                .and_then(|v| v.get("lightModel").and_then(Value::as_str).map(str::to_string))
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_LIGHT_MODEL.to_string())
        });
    selector = selector.trim().to_string();
    select(providers, &selector).ok()
}

/// Persist an API-key credential for `provider` into `auth.json` as
/// `{ "type": "api_key", "key": "..." }`, creating/updating the file and
/// preserving any other entries. Returns the path that was written. Used by
/// the `/login` command. Best-effort: surfaces an error string on failure.
pub fn set_auth_key(provider: &str, key: &str) -> Result<PathBuf, String> {
    let p = auth_path();
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let mut v: Value = fs::read_to_string(&p)
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    if !v.is_object() {
        v = Value::Object(serde_json::Map::new());
    }
    let obj = v.as_object_mut().unwrap();
    obj.insert(
        provider.to_string(),
        json!({ "type": "api_key", "key": key }),
    );
    fs::write(&p, serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok(p)
}

/// Remove the stored credential (API key or OAuth) for `provider` from
/// `auth.json`. Leaves environment-variable / models.json config untouched
/// (those are not stored here). Returns `Ok(true)` when an entry was removed,
/// `Ok(false)` when there was nothing to remove. Used by the `/logout`
/// command.
pub fn remove_auth_key(provider: &str) -> Result<bool, String> {
    let p = auth_path();
    if !p.exists() {
        return Ok(false);
    }
    let mut v: Value = serde_json::from_str(&fs::read_to_string(&p).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    if !v.is_object() {
        return Ok(false);
    }
    let obj = v.as_object_mut().unwrap();
    let removed = obj.remove(provider).is_some();
    if removed {
        fs::write(&p, serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    }
    Ok(removed)
}

/// The provider ids that currently have a stored credential (API key) in
/// `auth.json`, in file order. Used by `/logout` to list what can be removed.
pub fn stored_auth_providers() -> Vec<String> {
    let p = auth_path();
    let mut out = Vec::new();
    if let Ok(raw) = fs::read_to_string(&p) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            if let Some(obj) = v.as_object() {
                for (id, val) in obj {
                    if val.get("type").and_then(Value::as_str) == Some("api_key")
                        && val.get("key").and_then(Value::as_str).map(|k| !k.is_empty()).unwrap_or(false)
                    {
                        out.push(id.clone());
                    }
                }
            }
        }
    }
    out
}
/// execution user and path. Created/updated by `pir project init`.
pub fn projects_file() -> PathBuf {
    pi_dir().join("agent").join("projects.json")
}

/// The user a project's commands should run as. Resolution order:
///   1. explicit `-u/--as <user>` (passed as `explicit`)
///   2. an entry under `projects` keyed by project name in `projects.json`
///   3. auto-derived `ai_<sanitized-basename(cwd)>`
pub fn resolve_project_user(explicit: Option<&str>, project: Option<&str>) -> String {
    if let Some(u) = explicit {
        return u.to_string();
    }
    let name = project.map(|p| p.to_string()).unwrap_or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|c| c.file_name().map(|n| n.to_string_lossy().to_string()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "default".into())
    });
    if let Some(u) = lookup_project_user(&name) {
        return u;
    }
    format!("ai_{}", sanitize_project(&name))
}

/// Look up the configured execution user for a project name (or by path
/// prefix) from projects.json. Returns `None` if absent.
pub fn lookup_project_user(project: &str) -> Option<String> {
    let raw = fs::read_to_string(projects_file()).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let projects = v.get("projects").and_then(Value::as_object)?;
    if let Some(p) = projects.get(project) {
        if let Some(u) = p.get("user").and_then(Value::as_str) {
            if !u.is_empty() {
                return Some(u.to_string());
            }
        }
    }
    // Fall back: match by path prefix.
    let cwd = std::env::current_dir().ok()?;
    let cwd_s = cwd.to_string_lossy().to_string();
    for (_, p) in projects {
        if let Some(path) = p.get("path").and_then(Value::as_str) {
            if !path.is_empty() && cwd_s.starts_with(path) {
                if let Some(u) = p.get("user").and_then(Value::as_str) {
                    if !u.is_empty() {
                        return Some(u.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Default directory under which named projects are created when using the
/// `/create` command (overridable with `PIR_PROJECTS_DIR`).
///
/// When running as a per-project user (`ai_X`) we've dropped privileges but
/// `$HOME` is usually still inherited from root, so the global
/// `~/.pi/projects` is unwritable. Detect the dropped (non-root) uid and fall
/// back to a `projects/` dir under that user's `$HOME` (which they own).
pub fn projects_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("PIR_PROJECTS_DIR") {
        return PathBuf::from(d);
    }
    #[cfg(unix)]
    {
        let uid = unsafe { libc::getuid() };
        if uid != 0 {
            // `$HOME` is usually still inherited from root after a privilege
            // drop, so resolve the running user's *real* home (which they
            // own) instead. If that isn't creatable, fall back to a `projects/`
            // dir under the current working directory (which `project init`
            // chowns to the project user).
            if let Some(home) = crate::user::current_user_home() {
                let base = home.join("projects");
                if std::fs::create_dir_all(&base).is_ok() {
                    return base;
                }
            }
            if let Ok(cwd) = std::env::current_dir() {
                return cwd.join("projects");
            }
        }
    }
    pi_dir().join("projects")
}
fn sanitize_project(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .filter(|c| *c == '_' || c.is_ascii_alphanumeric())
        .collect();
    if s.is_empty() {
        s = "proj".into();
    }
    if s.len() > 24 {
        s.truncate(24);
    }
    // Usernames cannot start with a digit.
    if s.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        s.insert(0, '_');
    }
    s
}

/// Record (or update) a project -> user mapping in projects.json. Idempotent.
pub fn set_project_user(project: &str, user: &str, path: &str) -> Result<(), String> {
    let path_db = projects_file();
    let mut v: Value = fs::read_to_string(&path_db)
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    if !v.is_object() {
        v = Value::Object(serde_json::Map::new());
    }
    let projects = v
        .as_object_mut()
        .ok_or_else(|| "projects.json is not a JSON object".to_string())?
        .entry("projects")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let projects_obj = projects
        .as_object_mut()
        .ok_or_else(|| "projects.json 'projects' is not a JSON object".to_string())?;
    let entry = projects_obj
        .entry(project.to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let entry_obj = entry
        .as_object_mut()
        .ok_or_else(|| "projects.json project entry is not a JSON object".to_string())?;
    entry_obj.insert("user".into(), Value::String(user.to_string()));
    entry_obj.insert("path".into(), Value::String(path.to_string()));
    if let Some(parent) = path_db.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(&path_db, serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

/// Return up to `limit` completion candidates for `/model` matching `prefix`
/// (case-insensitive). Used for tab-completion and the live preview hint.
///
/// The completion behaves as a natural continuation of what the user typed:
/// when the prefix matches a model id/name we return the bare `model` id
/// (e.g. typing `de` -> `deepseek-v4-flash`) instead of prepending the
/// provider and producing a duplicated-looking `deollama-cloud/...`. The bare
/// id still resolves unambiguously via [`select`]; when several providers
/// share that model id the user gets the provider choices at selection time.
///
/// Only when the prefix matches the provider portion (no model id match) do we
/// keep the full `provider/model` labels, so the user can pick a model within
/// a specific provider.
pub fn match_models(providers: &[Provider], prefix: &str, limit: usize) -> Vec<String> {
    let p = prefix.trim().to_lowercase();

    if p.is_empty() {
        let mut out: Vec<String> = providers
            .iter()
            .flat_map(|prov| prov.models.iter().map(move |m| prov.label(m)))
            .collect();
        out.sort();
        out.dedup();
        out.truncate(limit);
        return out;
    }

    // (provider, model) pairs whose label, model id, or name contains the
    // prefix anywhere.
    let pairs: Vec<(&Provider, &Model)> = providers
        .iter()
        .flat_map(|prov| prov.models.iter().map(move |m| (prov, m)))
        .filter(|(prov, m)| {
            let label = prov.label(m).to_lowercase();
            let mid = m.id.to_lowercase();
            let name = m.name.as_deref().unwrap_or("").to_lowercase();
            label.contains(&p) || mid.contains(&p) || name.contains(&p)
        })
        .collect();

    if pairs.is_empty() {
        return Vec::new();
    }

    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (prov, m) in &pairs {
        let mid = m.id.to_lowercase();
        let name = m.name.as_deref().unwrap_or("").to_lowercase();
        // Model-id/name match -> bare id (continuation). Otherwise keep the
        // provider-qualified label (provider-only prefix).
        let candidate = if mid.contains(&p) || name.contains(&p) {
            m.id.clone()
        } else {
            prov.label(m)
        };
        if seen.insert(candidate.clone()) {
            out.push(candidate);
        }
    }

    // Prefix matches are what the user is typing (e.g. `op` -> `opencode/...`);
    // they must outrank infix/substring matches (e.g. `anthrop**op**ic/...`,
    // which would otherwise win merely by alphabetical order). Sort is stable,
    // so ties keep the catalog order.
    out.sort_by_key(|c| {
        let hit = c.to_lowercase();
        // 0 = starts with the prefix (best), 1 = contains it later (fallback).
        if hit.starts_with(&p) { 0 } else { 1 }
    });
    out.truncate(limit);
    out
}

/// Given a full candidate (e.g. a `/model` completion) and the prefix the user
/// has already typed, return only the trailing part so an inline hint reads as
/// a continuation of what was typed (avoids the `de` + `ollama-cloud/...`
/// duplication). Returns `None` when the candidate isn't a direct extension of
/// the prefix.
pub fn hint_remainder(candidate: &str, prefix: &str) -> Option<String> {
    let p = prefix.trim();
    if p.is_empty() {
        return None;
    }
    let c = candidate.as_bytes();
    let q = p.as_bytes();
    if c.len() > q.len() && c[..q.len()].eq_ignore_ascii_case(q) {
        Some(candidate[q.len()..].to_string())
    } else {
        None
    }
}

/// Load the `notify` policy from `~/.pi/agent/settings.json`. Missing or
/// malformed settings fall back to the built-in defaults (bell + desktop, only
/// for long turns in the background).
pub fn load_notify_policy() -> crate::notify::NotifyPolicy {
    let p = pi_dir().join("agent").join("settings.json");
    let raw = match fs::read_to_string(&p) {
        Ok(r) => r,
        Err(_) => return crate::notify::NotifyPolicy::default(),
    };
    let v: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return crate::notify::NotifyPolicy::default(),
    };
    match v.get("notify") {
        Some(n) => crate::notify::NotifyPolicy::from_json(n),
        None => crate::notify::NotifyPolicy::default(),
    }
}

pub fn select<'a>(
    providers: &'a [Provider],
    selector: &str,
) -> Result<(&'a Provider, &'a Model), String> {
    let sel = selector.trim().to_lowercase();

    // `:N` positional selector: pick the Nth model from the same flat
    // (provider, then model) order `/models` prints, so an index shown by the
    // listing always resolves. Out-of-range -> a helpful error, never a panic.
    if let Some(num) = sel.strip_prefix(':') {
        if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
            let flat: Vec<(&'a Provider, &'a Model)> = providers
                .iter()
                .flat_map(|p| p.models.iter().map(move |m| (p, m)))
                .collect();
            let n: usize = num.parse().unwrap_or(usize::MAX);
            return match flat.get(n) {
                Some((p, m)) => Ok((p, m)),
                None => Err(format!(
                    "no model at position {n} — `/models` numbers them 0..{}",
                    flat.len().saturating_sub(1)
                )),
            };
        }
    }

    if let Some((pid, mid)) = selector.trim().split_once('/') {
        for p in providers {
            if p.pid().eq_ignore_ascii_case(pid) {
                for m in &p.models {
                    if m.id.eq_ignore_ascii_case(mid) { return Ok((p, m)); }
                }
            }
        }
    }
    let sel_models: Vec<(&'a Provider, &'a Model)> = providers
        .iter()
        .flat_map(|p| p.models.iter().map(move |m| (p, m)))
        .filter(|(_, m)| m.id.eq_ignore_ascii_case(selector.trim()))
        .collect();
    if let Some((p, m)) = sel_models.first() {
        // Unique bare model id -> resolve directly. If several providers
        // expose the same model id, surface them as choices.
        let providers_with: Vec<&'a Provider> =
            sel_models.iter().map(|(p, _)| *p).collect();
        if providers_with.windows(2).all(|w| w[0].pid() == w[1].pid()) {
            return Ok((*p, *m));
        }
        return Err(format!(
            "'{}' exists in multiple providers: {}",
            selector.trim(),
            providers_with.iter().map(|p| p.label(m)).collect::<Vec<_>>().join(", ")
        ));
    }
    // Partial substring match over the `provider/model` label, the model id,
    // and the display name. If the text before the first `/` names (or
    // prefixes) a known provider, narrow to that provider and match the
    // remainder against just its models, so `openai/4.1` stays within openai
    // (and `:N`-style fragments can't spill across providers). Otherwise fall
    // back to the whole-label substring behaviour.
    let (sel_provider, sel_text) = match sel.split_once('/') {
        Some((pid, rest))
            if providers.iter().any(|p| p.pid().to_lowercase().starts_with(pid)) =>
        {
            (Some(pid.to_string()), rest.trim().to_string())
        }
        _ => (None, sel.clone()),
    };
    let hits: Vec<(&'a Provider, &'a Model)> = providers
        .iter()
        .flat_map(|p| p.models.iter().map(move |m| (p, m)))
        .filter(|(p, m)| {
            let empty = String::new();
            let mid = m.id.to_lowercase();
            let name = m.name.as_deref().unwrap_or(&empty).to_lowercase();
            match &sel_provider {
                Some(pid) => {
                    p.pid().to_lowercase().starts_with(pid)
                        && !sel_text.is_empty()
                        && (mid.contains(&sel_text) || name.contains(&sel_text))
                }
                None => {
                    format!("{}/{}", p.pid(), m.id).to_lowercase().contains(&sel)
                        || name.contains(&sel)
                }
            }
        })
        .collect();
    match hits.as_slice() {
        [only] => Ok(*only),
        [] => Err(format!(
            "no model matches '{selector}' — try a partial match (id, name, provider/model) or `:N` from `/models`"
        )),
        _ => Err(format!(
            "'{selector}' is ambiguous: {}",
            hits.iter().map(|(p, m)| p.label(m)).collect::<Vec<_>>().join(", ")
        )),
    }
}

// ---------------------------------------------------------------------------
// Cross-instance model broadcast
// ---------------------------------------------------------------------------
//
// `pir` is a process-per-terminal app: every open terminal has its own
// independent `pir` with its own agent/bus, so there is no in-process way to
// reach "all running instances". To let `/model*` switch the model in *every*
// open terminal at once, `pir` publishes a tiny broadcast file under the
// user's `~/.pi/agent/` and a lightweight watcher in each instance polls it.
//
// The file is owned by the user (under `~/.pi`), so the blast radius is
// naturally scoped to that user's own terminals — never other users. Scope is
// "same user", not "same shell", so a `/model*` from one of your terminals
// reaches all of your other terminals too.

/// Path of the cross-instance model-broadcast file.
pub fn model_broadcast_path() -> PathBuf {
    pi_dir().join("agent").join("model-broadcast.json")
}

/// The current model broadcast, if any and well-formed.
pub fn read_model_broadcast() -> Option<ModelBroadcast> {
    let raw = fs::read_to_string(model_broadcast_path()).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    Some(ModelBroadcast {
        generation: v.get("generation").and_then(Value::as_u64).unwrap_or(0),
        label: v.get("label").and_then(Value::as_str).unwrap_or("").to_string(),
        by_pid: v.get("byPid").and_then(Value::as_u64).unwrap_or(0),
        ts: v.get("ts").and_then(Value::as_u64).unwrap_or(0),
    })
}

/// A single model-broadcast event published by `/model*`.
#[derive(Clone, Debug)]
pub struct ModelBroadcast {
    /// Monotonic counter so watchers can detect "new since I last applied".
    pub generation: u64,
    /// The `provider/model` label to switch to.
    pub label: String,
    /// PID of the `pir` that originated the broadcast (so it can ignore itself).
    pub by_pid: u64,
    /// Epoch seconds when it was published.
    pub ts: u64,
}

/// Publish a model-broadcast event for `label`, stamping it with the current
/// process pid and a `generation` one greater than any previously recorded.
/// Returns the generation that was written (useful for the originator to ignore
/// its own echo). Best-effort: a write failure is silently ignored.
pub fn publish_model_broadcast(label: &str) -> Option<u64> {
    let p = model_broadcast_path();
    if let Some(parent) = p.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let prev = read_model_broadcast().map(|b| b.generation).unwrap_or(0);
    let generation = prev + 1;
    let payload = json!({
        "generation": generation,
        "label": label,
        "byPid": std::process::id(),
        "ts": crate::term::epoch(),
    });
    if fs::write(&p, serde_json::to_string_pretty(&payload).unwrap_or_default()).is_ok() {
        Some(generation)
    } else {
        None
    }
}


/// Default for incremental (in-place) markdown rendering. Enabled unless
/// explicitly disabled via `PIR_INCREMENTAL_MD=0` (see `Agent::set_incremental_md`).
pub fn incremental_md_default() -> bool {
    std::env::var("PIR_INCREMENTAL_MD")
        .map(|v| v.trim() != "0")
        .unwrap_or(true)
}

/// Markdown renderer backend used by `md::render` to turn agent replies into
/// styled terminal text. Resolution order:
///   1. `PIR_MARKDOWN_RENDERER` env var (`pulldown` | `pulldown-cmark` |
///      `comrak`)
///   2. `markdownRenderer` in `~/.pi/agent/settings.json`
///   3. the built-in default, `pulldown` (the lighter, default-enabled backend)
///
/// Returns lowercased, canonical backend name (`pulldown` or `comrak`); an
/// unknown/empty value falls back to `pulldown`. Note the `comrak` backend is
/// only compiled into the binary when the `comrak-backend` cargo feature is
/// enabled; that gate is enforced by the caller.
pub fn markdown_renderer_backend() -> &'static str {
    let from_env = std::env::var("PIR_MARKDOWN_RENDERER")
        .ok()
        .map(|v| v.trim().to_lowercase())
        .filter(|s| !s.is_empty());
    let sel = from_env.unwrap_or_else(|| {
        let p = pi_dir().join("agent").join("settings.json");
        fs::read_to_string(&p)
            .ok()
            .and_then(|r| serde_json::from_str::<Value>(&r).ok())
            .and_then(|v| v.get("markdownRenderer").and_then(Value::as_str).map(str::to_lowercase))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "pulldown".into())
    });
    match sel.as_str() {
        "pulldown" | "pulldown-cmark" => "pulldown",
        "comrak" => "comrak",
        _ => "pulldown",
    }
}

// ---------------------------------------------------------------------------
// Startup snapshot of `~/.pi`
// ---------------------------------------------------------------------------
//
// Before doing anything destructive, `pir` snapshots its config/home
// (`~/.pi`) once, so a future `/quarantine apply` or a bad plugin can be
// rolled back. The snapshot is created lazily: if *either* `~/.pi_backup.tgz`
// *or* `~/.pi_backup.zip` already exists we leave it alone (the user may have
// a fresh, deliberate backup); otherwise we create one of them. Best-effort:
// any failure is silently ignored so backup creation can never block startup.

/// Home directory of the current user (`$HOME`, else `$USERPROFILE`, else `.`).
fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Path of the `.tgz` snapshot we would create.
pub fn pi_backup_tgz() -> PathBuf {
    home_dir().join(".pi_backup.tgz")
}

/// Path of the `.zip` snapshot we would create.
pub fn pi_backup_zip() -> PathBuf {
    home_dir().join(".pi_backup.zip")
}

/// True when a usable snapshot already exists (either archive path).
pub fn pi_backup_exists() -> bool {
    pi_backup_tgz().exists() || pi_backup_zip().exists()
}

/// Ensure a one-time snapshot of `~/.pi` exists. Called once at startup. If a
/// snapshot already exists (tgz *or* zip) this is a no-op. Otherwise it creates
/// `~/.pi_backup.tgz` via the `tar` CLI (the reliable, compressed path on
/// unix). If `tar` is unavailable (e.g. stripped/non-unix image), it falls back
/// to `~/.pi_backup.zip` via the `zip` CLI -- a real compressed archive, so no
/// hand-rolled zip writer is needed. Best-effort: failures are swallowed so a
/// missing tool can never block `pir` from starting.
pub fn ensure_pi_backup() {
    if pi_backup_exists() {
        return;
    }
    let src = pi_dir();
    if !src.exists() {
        return; // nothing to snapshot
    }
    if ensure_pi_backup_tar(&src) {
        return;
    }
    // Fallback: `zip` (Info-ZIP), the system compressor on Windows and most
    // stripped *nix images. Produces a real compressed `.zip` archive.
    let _ = ensure_pi_backup_zip(&src);
}

/// Create `~/.pi_backup.tgz` with `tar -czf`. Returns true on success.
fn ensure_pi_backup_tar(src: &Path) -> bool {
    let tgz = pi_backup_tgz();
    // `tar` interprets the archive's contents relative to `-C <dir>`; we cd to
    // the *parent* of src and add `src.file_name()` so the archive contains a
    // top-level `.pi/` directory (matching the conventional layout) rather
    // than an absolute path.
    let parent = src.parent().unwrap_or_else(|| Path::new("."));
    let name = src.file_name().unwrap_or_else(|| std::ffi::OsStr::new(".pi"));
    let status = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&tgz)
        .arg("-C")
        .arg(parent)
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() && tgz.exists() => true,
        _ => {
            // Partial/garbled output: drop it so the next launch retries cleanly.
            let _ = std::fs::remove_file(&tgz);
            false
        }
    }
}

/// Create `~/.pi_backup.zip` with `zip -qr <archive> <dir>` (recursive, quiet).
/// Returns true on success. Used only as a fallback when `tar` is unavailable.
fn ensure_pi_backup_zip(src: &Path) -> bool {
    let zip = pi_backup_zip();
    // `zip` records paths relative to the process cwd, so we chdir into the
    // *parent* of `.pi` and add the `.pi` entry — that way the archive has a
    // clean top-level `.pi/` instead of an absolute path. (Info-ZIP `zip` has
    // no `-C`/chdir flag like tar does.)
    let parent = src.parent().unwrap_or_else(|| Path::new("."));
    let name = src.file_name().unwrap_or_else(|| std::ffi::OsStr::new(".pi"));
    let status = std::process::Command::new("zip")
        .arg("-qr")
        .arg(&zip)
        .arg(name)
        .current_dir(parent)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() && zip.exists() => true,
        _ => {
            let _ = std::fs::remove_file(&zip);
            false
        }
    }
}

#[cfg(test)]
mod select_tests {
    use super::*;

    fn mk(id: &str, name: &str) -> Model {
        Model {
            id: id.into(),
            name: Some(name.into()),
            context: Some(1000),
            max_tokens: None,
            api_override: None,
            url_override: None,
            no_reasoning_effort: false,
            price_per_1k: None,
        }
    }

    fn providers() -> Vec<Provider> {
        vec![
            Provider {
                id: Some("anthropic".into()),
                name: None,
                api: Some("anthropic".into()),
                base_url: Some("https://api.anthropic.com/v1".into()),
                api_key: None,
                models: vec![
                    mk("claude-sonnet-4-5", "Claude Sonnet 4.5"),
                    mk("claude-haiku-4-5", "Claude Haiku 4.5"),
                ],
            },
            Provider {
                id: Some("openai".into()),
                name: None,
                api: Some("openai".into()),
                base_url: Some("https://api.openai.com/v1".into()),
                api_key: None,
                models: vec![mk("gpt-4.1", "GPT-4.1"), mk("gpt-4.1-mini", "GPT-4.1 mini")],
            },
        ]
    }

    fn pick(provs: &[Provider], sel: &str) -> String {
        select(provs, sel)
            .map(|(p, m)| p.label(m))
            .unwrap_or_else(|e| format!("ERR: {e}"))
    }

    #[test]
    fn positional_colon_selects_models_order() {
        let provs = providers();
        // The same flat (provider, then model) order `list_models` prints.
        assert_eq!(pick(&provs, ":0"), "anthropic/claude-sonnet-4-5");
        assert_eq!(pick(&provs, ":1"), "anthropic/claude-haiku-4-5");
        assert_eq!(pick(&provs, ":2"), "openai/gpt-4.1");
        assert_eq!(pick(&provs, ":3"), "openai/gpt-4.1-mini");
    }

    #[test]
    fn positional_out_of_range_and_junk() {
        let provs = providers();
        let err = pick(&provs, ":9");
        assert!(err.starts_with("ERR: no model at position 9"), "{err}");
        // A non-numeric `:` selector is not positional (falls through to the
        // normal matcher, which errors with the usual message).
        assert!(pick(&provs, ":x").starts_with("ERR: no model matches ':x'"), "{err}");
    }

    #[test]
    fn provider_narrowed_partial_match() {
        let provs = providers();
        // `provider/fragment` narrows to that provider's models.
        assert_eq!(pick(&provs, "anthropic/haiku"), "anthropic/claude-haiku-4-5");
        // The provider part may be an abbreviated prefix; match on name too.
        assert_eq!(pick(&provs, "anth/son"), "anthropic/claude-sonnet-4-5");
        assert_eq!(pick(&provs, "openai/gpt-4.1-mini"), "openai/gpt-4.1-mini");
        // Fragments matching several models of that provider stay ambiguous,
        // but only within the named provider.
        let e = pick(&provs, "openai/gpt");
        assert!(e.starts_with("ERR: 'openai/gpt' is ambiguous:"), "{e}");
        assert!(e.contains("openai/gpt-4.1, openai/gpt-4.1-mini"), "{e}");
    }

    #[test]
    fn whole_label_substring_fallback_kept() {
        let provs = providers();
        // No known provider before the `/`: fall back to a substring match on
        // the full `provider/model` label (the old behaviour).
        assert_eq!(pick(&provs, "haiku-4-5"), "anthropic/claude-haiku-4-5");
        // Bare id substring still resolves.
        assert_eq!(pick(&provs, "4.1-mi"), "openai/gpt-4.1-mini");
    }

    #[test]
    fn ambiguous_and_missing_report_choices() {
        let provs = providers();
        let e = pick(&provs, "4.1");
        assert!(e.starts_with("ERR: '4.1' is ambiguous:"), "{e}");
        assert!(e.contains("openai/gpt-4.1, openai/gpt-4.1-mini"), "{e}");
        let e = pick(&provs, "zzz");
        assert!(e.starts_with("ERR: no model matches 'zzz'"), "{e}");
        assert!(e.contains(":N"), "error should mention the :N escape hatch: {e}");
    }

    #[test]
    fn completion_prefix_matches_rank_above_infix() {
        // `op` is an infix of "anthropic" but a prefix of "opencode": the
        // prefix hit must come first so `/model op<Tab>` offers opencode,
        // not anthropic/... (which previously won via alphabetical sort).
        let mut provs = providers();
        provs.insert(
            0,
            Provider {
                id: Some("opencode".into()),
                name: None,
                api: Some("openai".into()),
                base_url: Some("https://opencode.ai/v1".into()),
                api_key: None,
                models: vec![mk("opencode-chat", "OpenCode Chat")],
            },
        );
        let ms = match_models(&provs, "op", 10);
        // Model-id matches return the bare id (documented continuation
        // behaviour) — it resolves unambiguously to the opencode provider.
        assert_eq!(ms.first().map(String::as_str), Some("opencode-chat"));
        // The infix-only matches are still offered as fallbacks, just after.
        assert!(ms.iter().any(|c| c.starts_with("anthropic/")), "infix matches must remain: {ms:?}");
    }
}
