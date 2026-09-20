use serde::Deserialize;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKind {
    Anthropic,
    OpenAi,
    /// OpenAI Responses API (`/responses`): used by models whose catalog
    /// entry says `openai-responses` (e.g. opencode-go's muse-spark, grok,
    /// gpt-5.6-luna — see opencode.ai/docs/go endpoints table). Chat-style
    /// `/chat/completions` calls to these models fail, so they need their own
    /// request shape, streaming events, and usage envelope.
    OpenAiResponses,
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
    /// pi `reasoning`: whether the model supports extended thinking. Gates all
    /// thinking controls — when false, pir sends no `thinking`,
    /// `reasoning_effort`, or `reasoning` params at all (pi sends nothing for
    /// non-reasoning models either). Defaults to false (pi's default); the
    /// catalog sets it true for reasoning models. Parsed from the top-level
    /// `reasoning` key in `models-store.json` (see `load_providers`).
    #[serde(default)]
    pub reasoning: bool,
    /// pi `compat.thinkingFormat` (`deepseek`, `openrouter`, `qwen`,
    /// `qwen-chat-template`, `openai`, …). `None` means the default
    /// OpenAI-style handling (legacy pir behavior: raw `reasoning_effort`).
    /// Model-level value wins; provider-level `compat` is the fallback.
    #[serde(default)]
    pub thinking_format: Option<String>,
    /// pi `compat.supportsReasoningEffort`. `None` means true (send the mapped
    /// effort). Model-level wins; provider-level `compat` is the fallback.
    #[serde(default)]
    pub supports_reasoning_effort: Option<bool>,
    /// pi `compat.sessionAffinityFormat` (e.g. `openai-nosession` on
    /// opencode-go's muse-spark/grok/gpt-5.6-luna): when it says nosession,
    /// pir must NOT send `x-opencode-session` for that model (Console Go
    /// 400s `invalid_request_error` otherwise). `None` means "send the
    /// session header as before". Model-level wins; provider-level `compat`
    /// is the fallback (same merge rule as the other compat keys).
    #[serde(default)]
    pub session_affinity_format: Option<String>,
    /// pi `thinkingLevelMap`: pi level name (`off`, `minimal`, `low`,
    /// `medium`, `high`, `xhigh`, `max`) → provider value, or `None` when the
    /// level is explicitly unsupported/hidden (`null` in JSON). A missing key
    /// falls back to pir's default effort names. Parsed from the top-level
    /// `thinkingLevelMap` object in `models-store.json`.
    #[serde(default)]
    pub thinking_level_map: std::collections::BTreeMap<String, Option<String>>,
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

    /// pi `compat.thinkingFormat`, defaulting to OpenAI-style handling.
    pub fn thinking_format_name(&self) -> &str {
        self.thinking_format.as_deref().unwrap_or("openai")
    }

    /// pi `compat.supportsReasoningEffort`, defaulting to true.
    pub fn supports_effort(&self) -> bool {
        self.supports_reasoning_effort.unwrap_or(true)
    }

    /// Whether this model opts out of the `x-opencode-session` routing header:
    /// true when `compat.sessionAffinityFormat` says nosession (either
    /// spelling, case-insensitive, e.g. `openai-nosession`). Console Go
    /// rejects the header on such models with HTTP 400
    /// (`invalid_request_error`); every other model keeps sending it.
    pub fn no_session_affinity(&self) -> bool {
        self.session_affinity_format
            .as_deref()
            .map(|s| {
                let t = s.trim().to_ascii_lowercase();
                t.contains("nosession") || t.contains("no_session") || t == "none"
            })
            .unwrap_or(false)
    }

    /// Whether `thinkingLevelMap` explicitly hides the `off` level (`off` is
    /// `null`). Only explicit null skips the disable toggle — a missing `off`
    /// key still sends it (pi's `thinkingLevelMap?.off !== null` check).
    pub fn off_is_null(&self) -> bool {
        matches!(self.thinking_level_map.get("off"), Some(None))
    }

    /// Map a thinking level to the provider effort string, honoring
    /// `thinkingLevelMap`: an explicit string wins, otherwise pir's default
    /// effort names apply (`ThinkingLevel::oai_effort`). `Off` only maps when
    /// `off` is an explicit string (e.g. `"none"`); explicit-null levels fall
    /// back to the defaults too (the picker hides them, so this path only
    /// triggers for persisted/forced selections — same fallback pi's `??`
    /// applies, but with pir's server-compatible collapsed names instead of
    /// the raw level). Pure mapping: the caller gates on `reasoning`.
    pub fn mapped_effort(&self, level: ThinkingLevel) -> Option<String> {
        if level == ThinkingLevel::Off {
            return self.thinking_level_map.get("off").and_then(|v| v.clone());
        }
        if let Some(Some(s)) = self.thinking_level_map.get(level.as_str()) {
            return Some(s.clone());
        }
        level.oai_effort().map(str::to_string)
    }

    /// pi `getSupportedThinkingLevels` (pi-ai `models.js`): non-reasoning
    /// models offer only `off`. Otherwise `off`/`minimal`/`low`/`medium`/`high`
    /// show unless explicitly null, while `xhigh`/`max` additionally require
    /// an explicit string entry (a missing extended key means hidden).
    pub fn supported_levels(&self) -> Vec<ThinkingLevel> {
        if !self.reasoning {
            return vec![ThinkingLevel::Off];
        }
        const ORDER: [ThinkingLevel; 7] = [
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::XHigh,
            ThinkingLevel::Max,
        ];
        ORDER
            .into_iter()
            .filter(|l| match self.thinking_level_map.get(l.as_str()) {
                Some(None) => false, // explicit null → hidden
                Some(Some(_)) => true, // explicit string → shown
                // Missing: standard levels shown, extended levels hidden.
                None => !matches!(l, ThinkingLevel::XHigh | ThinkingLevel::Max),
            })
            .collect()
    }

    /// pi `clampThinkingLevel`: the nearest available level, preferring
    /// upward (toward stronger thinking) then downward.
    pub fn clamp_thinking(&self, level: ThinkingLevel) -> ThinkingLevel {
        let avail = self.supported_levels();
        if avail.contains(&level) {
            return level;
        }
        const ORDER: [ThinkingLevel; 7] = [
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::XHigh,
            ThinkingLevel::Max,
        ];
        let idx = ORDER.iter().position(|l| *l == level).unwrap_or(0);
        for candidate in ORDER.iter().skip(idx) {
            if avail.contains(candidate) {
                return *candidate;
            }
        }
        for candidate in ORDER[..idx].iter().rev() {
            if avail.contains(candidate) {
                return *candidate;
            }
        }
        ThinkingLevel::Off
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
                // Same effort names ride `reasoning.effort` on Responses.
                Some(ApiKind::OpenAiResponses) => self.oai_effort().is_some(),
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
            Some(a) if a.contains("responses") => Some(ApiKind::OpenAiResponses),
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
            } else if a.contains("responses") {
                Some(ApiKind::OpenAiResponses)
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
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    PathBuf::from(s)
}

/// Parse pi `thinkingLevelMap` (`{ level: string|null }`, also accepted as
/// `thinking_level_map`) from a model value. Keys are lowercased; explicit
/// `null` becomes `None` (hidden level), strings become `Some(value)`, and
/// non-string values are ignored. Missing/invalid shapes yield an empty map
/// (no constraints — legacy pir behavior).
fn parse_thinking_map(mv: &Value) -> std::collections::BTreeMap<String, Option<String>> {
    let mut map = std::collections::BTreeMap::new();
    let obj = mv
        .get("thinkingLevelMap")
        .or(mv.get("thinking_level_map"))
        .and_then(Value::as_object);
    if let Some(obj) = obj {
        for (k, v) in obj {
            let key = k.to_lowercase();
            if v.is_null() {
                map.insert(key, None);
            } else if let Some(s) = v.as_str() {
                map.insert(key, Some(s.to_string()));
            }
        }
    }
    map
}

/// Parse a pi `compat` string key, preferring the model value and falling
/// back to the provider value (pi merges provider-level `compat` under
/// model-level `compat`). Accepts both camelCase and snake_case spellings.
fn parse_compat_str(mv: &Value, pval: &Value, camel: &str, snake: &str) -> Option<String> {
    mv.get("compat")
        .and_then(|c| c.get(camel).or(c.get(snake)))
        .or(pval.get("compat").and_then(|c| c.get(camel).or(c.get(snake))))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Parse a pi `compat` bool key with the same model-over-provider precedence.
fn parse_compat_bool(mv: &Value, pval: &Value, camel: &str, snake: &str) -> Option<bool> {
    mv.get("compat")
        .and_then(|c| c.get(camel).or(c.get(snake)))
        .or(pval.get("compat").and_then(|c| c.get(camel).or(c.get(snake))))
        .and_then(Value::as_bool)
}

/// Parse pi `reasoning` (top-level model key). Missing means false — pi's
/// documented default; only catalog-declared reasoning models get thinking
/// controls and extended picker levels.
fn parse_reasoning(mv: &Value) -> bool {
    mv.get("reasoning").and_then(Value::as_bool).unwrap_or(false)
}

/// Process-wide snapshot of the catalog as read by the *invoking* user, before
/// the privilege drop. `main` seeds this at startup (see `seed_catalog`).
///
/// Why this exists: `become_user` drops pir to the sandbox user (`ai_X`) and
/// rewrites `HOME`, but the catalog itself lives in the invoking user's `~/.pi`
/// and is typically mode `0600` — the dropped identity cannot read it. pir
/// loads providers pre-drop for exactly that reason, but later code
/// (`Agent::new`'s cache, the mid-session `/provider` reload, the titler) calls
/// `load_providers` again *after* the drop, where the read fails EACCES. That
/// read is not an agent action, so it must not be subject to the drop; serving
/// the pre-drop snapshot keeps model switching working instead of silently
/// degrading to the (much smaller) sandbox store or the auth fallback.
static INVOKER_CATALOG: std::sync::OnceLock<Vec<Provider>> = std::sync::OnceLock::new();

/// Snapshot the catalog read as the invoking user, for post-drop reuse. Called
/// by `main` right after the pre-drop `load_providers`. No-op when unset.
pub fn seed_catalog(providers: &[Provider]) {
    if !providers.is_empty() {
        let _ = INVOKER_CATALOG.set(providers.to_vec());
    }
}

/// The pre-drop catalog snapshot, if `main` seeded one.
pub fn invoker_catalog() -> Option<&'static Vec<Provider>> {
    INVOKER_CATALOG.get()
}

pub fn load_providers() -> Result<Vec<Provider>, String> {
    match load_providers_uncached() {
        Ok(p) if !p.is_empty() => Ok(p),
        // The read failed or came back empty *after* the drop (or otherwise):
        // prefer the catalog the invoking user could read. This is the
        // difference between "model switch works" and "no model matches".
        _ => match invoker_catalog() {
            Some(p) if !p.is_empty() => Ok(p.clone()),
            _ => load_providers_uncached(),
        },
    }
}

fn load_providers_uncached() -> Result<Vec<Provider>, String> {
    let path = pi_dir().join("agent").join("models-store.json");

    // When a pre-drop snapshot exists, an unreadable store is expected (the
    // dropped sandbox identity can't read the invoking user's 0600 file) and
    // `load_providers` will serve the snapshot — so stay quiet instead of
    // printing a misleading "Falling back" warning on every call.
    let quiet = invoker_catalog().is_some();
    if !path.exists() {
        if !quiet {
            eprintln!("! models-store.json not found. Falling back to auth.json");
        }
        return load_from_auth_fallback();
    }

    let raw = match fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) => {
            if !quiet {
                eprintln!("! Cannot read models-store.json: {e}. Falling back");
            }
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
                            // Per-model `api` (OpenCode Zen marks individual
                            // models `openai-responses` while the provider is
                            // `openai-completions`; see `model_api`).
                            api_override: mv.get("api").and_then(Value::as_str).map(String::from),
                            // Per-model `baseUrl` (Zen routes some models to a
                            // different host than the provider default).
                            url_override: mv.get("baseUrl").or(mv.get("base_url")).and_then(Value::as_str).map(String::from),
                            no_reasoning_effort: false,
                            reasoning: parse_reasoning(mv),
                            thinking_format: parse_compat_str(mv, pval, "thinkingFormat", "thinking_format"),
                            supports_reasoning_effort: parse_compat_bool(mv, pval, "supportsReasoningEffort", "supports_reasoning_effort"),
                            session_affinity_format: parse_compat_str(mv, pval, "sessionAffinityFormat", "session_affinity_format"),
                            thinking_level_map: parse_thinking_map(mv),
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
                        // Per-model `api` / `baseUrl` (see the array branch
                        // above for why these are not hardcoded to None).
                        api_override: mv.get("api").and_then(Value::as_str).map(String::from),
                        url_override: mv.get("baseUrl").or(mv.get("base_url")).and_then(Value::as_str).map(String::from),
                        no_reasoning_effort: false,
                        reasoning: parse_reasoning(mv),
                        thinking_format: parse_compat_str(mv, pval, "thinkingFormat", "thinking_format"),
                        supports_reasoning_effort: parse_compat_bool(mv, pval, "supportsReasoningEffort", "supports_reasoning_effort"),
                        session_affinity_format: parse_compat_str(mv, pval, "sessionAffinityFormat", "session_affinity_format"),
                        thinking_level_map: parse_thinking_map(mv),
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
    maybe_add_fake_provider(&mut providers);
    Ok(providers)
}

/// Append the offline scripted `fake` provider (see `crate::fake`) when
/// `PIR_FAKE_MODEL` is set: opt-in test/puppet model, never in normal lists.
fn maybe_add_fake_provider(providers: &mut Vec<Provider>) {
    if std::env::var_os("PIR_FAKE_MODEL").map(|v| !v.is_empty()).unwrap_or(false)
        && !providers.iter().any(|p| p.pid() == "fake")
    {
        providers.push(Provider {
            id: Some("fake".to_string()),
            name: Some("Fake (testing only, no network)".to_string()),
            base_url: Some("fake://localhost".to_string()),
            api_key: Some("fake".to_string()),
            api: Some("openai".to_string()),
            models: vec![Model {
                id: "slow".to_string(),
                name: Some("Fake slow (scripted turns)".to_string()),
                context: Some(200_000),
                max_tokens: Some(8192),
                api_override: None,
                url_override: None,
                no_reasoning_effort: false,
                reasoning: false,
                thinking_format: None,
                supports_reasoning_effort: None,
                session_affinity_format: None,
                thinking_level_map: Default::default(),
                price_per_1k: None,
            }],
        });
    }
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
///
/// Returns `None` when nothing is configured.
pub fn ollama_cloud_api_key() -> Option<String> {
    if let Ok(v) = std::env::var("OLLAMA_API_KEY")
        && !v.is_empty() {
            return Some(v);
        }
    if let Some(k) = load_auth_keys().get("ollama-cloud")
        && !k.is_empty() {
            return Some(k.clone());
        }
    // Package-style per-extension config: ~/.pi/agent/ollama-cloud.json
    let cfg = pi_dir().join("agent").join("ollama-cloud.json");
    if let Ok(raw) = fs::read_to_string(&cfg)
        && let Ok(v) = serde_json::from_str::<Value>(&raw)
            && let Some(k) = v.get("apiKey").or(v.get("key")).and_then(Value::as_str)
                && !k.is_empty() {
                    return Some(k.to_string());
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
            reasoning: false,
            thinking_format: None,
            supports_reasoning_effort: None,
            session_affinity_format: None,
            thinking_level_map: Default::default(),
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
    if let Ok(raw) = fs::read_to_string(&p)
        && let Ok(v) = serde_json::from_str::<Value>(&raw)
            && let Some(prices) = v.get("prices").and_then(Value::as_object) {
                for (label, pv) in prices {
                    if let Some(arr) = pv.as_array()
                        && let (Some(i), Some(o)) = (arr.first().and_then(Value::as_f64), arr.get(1).and_then(Value::as_f64)) {
                            table.insert(label.to_lowercase(), (i, o));
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
            if val.get("type").and_then(Value::as_str) == Some("api_key")
                && let Some(key) = val.get("key").and_then(Value::as_str)
                    && !key.is_empty() {
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
                                reasoning: false,
                                thinking_format: None,
                                supports_reasoning_effort: None,
                                session_affinity_format: None,
                                thinking_level_map: Default::default(),
                                price_per_1k: None,
                            }],
                        });
                    }
        }
    }
    
    if providers.is_empty() { Err("No providers found in auth.json".into()) } else { maybe_add_fake_provider(&mut providers); Ok(providers) }
}

fn guess_base_url(pid: &str) -> Option<String> {
    let env_var = format!("{}_BASE_URL", pid.to_uppercase().replace('-', "_"));
    if let Ok(url) = std::env::var(&env_var)
        && !url.is_empty() { return Some(url); }

    if pid.contains("openrouter") { return Some("https://openrouter.ai/api/v1".into()); }
    if pid.contains("anthropic") { return Some("https://api.anthropic.com/v1".into()); }
    if pid.contains("openai") { return Some("https://api.openai.com/v1".into()); }
    
    None
}

fn load_auth_keys() -> std::collections::BTreeMap<String, String> {
    let path = pi_dir().join("agent").join("auth.json");
    let mut map = std::collections::BTreeMap::new();
    if let Ok(raw) = fs::read_to_string(&path)
        && let Ok(v) = serde_json::from_str::<Value>(&raw)
            && let Some(obj) = v.as_object() {
                for (id, val) in obj {
                    if val.get("type").and_then(Value::as_str) == Some("api_key")
                        && let Some(key) = val.get("key").and_then(Value::as_str)
                            && !key.is_empty() { map.insert(id.to_lowercase(), key.to_string()); }
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
        fs::create_dir_all(parent).map_err(|e| settings_write_error(parent, &e))?;
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
        .map_err(|e| settings_write_error(&p, &e))?;
    Ok(p)
}

/// Explain a settings-write failure with the path, the effective user, and
/// HOME. A bare "Permission denied" hides the two usual causes: the file
/// belongs to another user (sandbox drop or su/sudo left HOME pointing at
/// someone else's `~/.pi`), or the dir isn't writable. Naming all three
/// makes that visible instead of a dead-end os error 13.
fn settings_write_error(path: &std::path::Path, e: impl std::fmt::Display) -> String {
    format!(
        "cannot write {}: {e} [running as {}, HOME={:?}] — fix ownership, run as the owning user, or point pir at a writable dir with PI_DIR=...",
        path.display(),
        current_user_label(),
        std::env::var_os("HOME").map(|h| h.to_string_lossy().into_owned()),
    )
}

/// `name (euid N)` for the effective uid, or `euid N` when unresolvable.
fn current_user_label() -> String {
    #[cfg(unix)]
    {
        // SAFETY: geteuid/getpwuid are async-signal-safe getters; the
        // returned passwd pointer is not freed (static storage).
        let euid = unsafe { libc::geteuid() };
        let name = unsafe {
            let pw = libc::getpwuid(euid);
            if pw.is_null() {
                None
            } else {
                std::ffi::CStr::from_ptr((*pw).pw_name).to_str().ok()
            }
        };
        match name {
            Some(n) => format!("{n} (euid {euid})"),
            None => format!("euid {euid}"),
        }
    }
    #[cfg(not(unix))]
    {
        "unknown user".to_string()
    }
}

/// The persisted "use per-agent worktrees by default" flag (key `worktrees`
/// in `~/.pi/agent/settings.json`). Default: `false` — the guard posture is
/// "pi plus a seatbelt" (the in-process guardrail protects `.git` and the test
/// oracle); per-agent worktrees are the *opt-in* stronger posture
/// (`security.level = "worktree"`, `PIR_WT=1`, or the `/menu` Worktrees
/// toggle). `PIR_WT=1` at launch still wins over this for that session.
pub fn worktrees_default() -> bool {
    let p = pi_dir().join("agent").join("settings.json");
    let Some(raw) = fs::read_to_string(p).ok() else { return false };
    let v: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    v.get("worktrees").and_then(Value::as_bool).unwrap_or(false)
}

/// Selected HTTP transport backend (`http_backend` in
/// `~/.pi/agent/settings.json`: `"isahc"` or `"ureq"`). `PIR_HTTP_BACKEND`
/// wins when set. Returns the raw value for the caller to parse; `None`
/// (or anything unparseable) means the default backend.
pub fn http_backend_name() -> Option<String> {
    if let Ok(v) = std::env::var("PIR_HTTP_BACKEND")
        && !v.trim().is_empty() {
            return Some(v);
        }
    let p = pi_dir().join("agent").join("settings.json");
    let raw = fs::read_to_string(p).ok()?;
    let v: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    v.get("http_backend").and_then(Value::as_str).map(str::to_string)
}

/// Persist the worktree default flag (writes `worktrees` into
/// `~/.pi/agent/settings.json`, preserving the other keys).
pub fn set_worktrees_default(on: bool) -> Result<PathBuf, String> {
    let p = pi_dir().join("agent").join("settings.json");
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).map_err(|e| settings_write_error(parent, &e))?;
    }
    let mut v: Value = fs::read_to_string(&p)
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    if !v.is_object() {
        v = Value::Object(serde_json::Map::new());
    }
    v.as_object_mut().unwrap().insert("worktrees".into(), Value::Bool(on));
    fs::write(&p, serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?)
        .map_err(|e| settings_write_error(&p, &e))?;
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
pub fn resolve_light_model(
    providers: &[Provider],
) -> Option<(&Provider, &Model)> {
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
    if let Ok(raw) = fs::read_to_string(&p)
        && let Ok(v) = serde_json::from_str::<Value>(&raw)
            && let Some(obj) = v.as_object() {
                for (id, val) in obj {
                    if val.get("type").and_then(Value::as_str) == Some("api_key")
                        && val.get("key").and_then(Value::as_str).map(|k| !k.is_empty()).unwrap_or(false)
                    {
                        out.push(id.clone());
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
    if let Some(p) = projects.get(project)
        && let Some(u) = p.get("user").and_then(Value::as_str)
            && !u.is_empty() {
                return Some(u.to_string());
            }
    // Fall back: match by path prefix.
    let cwd = std::env::current_dir().ok()?;
    let cwd_s = cwd.to_string_lossy().to_string();
    for (_, p) in projects {
        if let Some(path) = p.get("path").and_then(Value::as_str)
            && !path.is_empty() && cwd_s.starts_with(path)
                && let Some(u) = p.get("user").and_then(Value::as_str)
                    && !u.is_empty() {
                        return Some(u.to_string());
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
/// Candidates are always the full `provider/model` labels, so Tab expands a
/// short fragment to its unambiguous qualified form: typing `mu` completes to
/// `opencode-go/muse-spark-1.` (the common prefix of the muse entries) rather
/// than a bare `muse-spark-1.` that hides which provider it belongs to. The
/// bare id still resolves via [`select`], but completion shows the qualified
/// label so there is never a provider ambiguity to resolve afterwards.
///
/// Ranking: a `provider/...` label starting with the prefix wins (rank 0),
/// then a model id/name *starting* with the prefix (rank 1, e.g. `mu` ->
/// `muse-spark-...`), then any other substring hit (rank 2). The sort is
/// stable, so ties keep catalog order.
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
        // Always the provider-qualified label: Tab must expand `mu` to
        // `opencode-go/muse-spark-1.`, never to a bare id that drops the
        // provider. The qualified label still resolves via [`select`].
        let candidate = prov.label(m);
        if seen.insert(candidate.clone()) {
            out.push(candidate);
        }
    }

    // Rank what the user is typing first: `provider/...` prefix hits (rank 0),
    // then model id/name prefix hits like `mu` -> `muse-spark-...` (rank 1),
    // then any other substring hit (rank 2, e.g. `anthrop**op**ic/...` for
    // `op`, which previously won merely by alphabetical order). Sort is
    // stable, so ties keep the catalog order.
    out.sort_by_key(|c| {
        let hit = c.to_lowercase();
        if hit.starts_with(&p) {
            return 0;
        }
        // Strip the `provider/` qualifier before testing the model-id prefix:
        // a qualified label never starts with `mu`, so check the part after
        // the `/` (and the bare-id form can't occur here, but handle it).
        let after_slash = hit.split('/').next_back().unwrap_or(&hit);
        if after_slash.starts_with(&p) || hit.starts_with(&p) {
            1
        } else {
            2
        }
    });
    out.truncate(limit);
    out
}

/// Tab-complete a `/model` input line (`/model`, `/m`, `/default-model`,
/// `/dm`) for the TUI/GUI front-ends, which don't use rustyline's completer.
/// A bare `/model` or `/default-model` gains a trailing space (parity with
/// `/thinking` -> `/thinking `); with an argument fragment the matches from
/// [`match_models`] collapse to a single label or their longest common prefix
/// (so `/model mu` becomes `/model opencode-go/muse-spark-1.`). Returns `None`
/// when nothing completes (unknown command, no matches, or the fragment is
/// already at the common prefix).
pub fn complete_model_buffer(buf: &str, providers: &[Provider]) -> Option<String> {
    if buf == "/model" || buf == "/default-model" {
        return Some(format!("{buf} "));
    }
    let mut parts = buf.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let rest = parts.next()?;
    if !matches!(cmd, "/model" | "/m" | "/default-model" | "/dm") {
        return None;
    }
    let frag = rest.trim_start();
    let matches = match_models(providers, frag, crate::term::MODEL_COMPLETION_LIMIT);
    if matches.is_empty() {
        return None;
    }
    if matches.len() == 1 {
        return Some(format!("{cmd} {}", matches[0]));
    }
    let lcp = longest_common_prefix(&matches);
    if !lcp.is_empty() && lcp != frag {
        return Some(format!("{cmd} {lcp}"));
    }
    None
}

/// Longest common prefix of `strs` (empty when empty/inconsistent). Shared by
/// [`complete_model_buffer`]; the TUI/GUI front-ends keep their own copies
/// for command-name completion.
fn longest_common_prefix(strs: &[String]) -> String {
    let Some(first) = strs.first() else { return String::new() };
    let mut end = first.len();
    for s in strs.iter().skip(1) {
        let mut i = 0;
        while i < end && i < s.len() && s.as_bytes()[i] == first.as_bytes()[i] {
            i += 1;
        }
        end = i;
        if end == 0 {
            break;
        }
    }
    first[..end].to_string()
}

/// Given a `/model` completion candidate (always a `provider/model` label
/// from [`match_models`]) and the fragment the user typed, return the ghost
/// suffix to preview after the cursor. Only when the label literally starts
/// with the fragment (`opencode-go/` + `muse` -> `se-spark-...`) is a suffix
/// preview possible: rustyline hints can only *append*, never replace, so a
/// model-id-only fragment like `mu` (whose expansion `opencode-go/muse-...`
/// does not start with `mu`) yields `None` here — Tab completion (which
/// *replaces* the fragment) is what expands it. Returns `None` when neither
/// applies.
pub fn hint_remainder(candidate: &str, prefix: &str) -> Option<String> {
    let p = prefix.trim();
    if p.is_empty() {
        return None;
    }
    let c = candidate.as_bytes();
    let q = p.as_bytes();
    if c.len() > q.len() && c[..q.len()].eq_ignore_ascii_case(q) {
        return Some(candidate[q.len()..].to_string());
    }
    None
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
    if let Some(num) = sel.strip_prefix(':')
        && !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
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

/// Whether the explicit-stop skill (`request_stop` tool + auto-nudge) is on
/// by default. On unless disabled via `PIR_STOP_SKILL=0` (also accepts
/// `off`/`false`/`no`) or `"stopSkill": false` in
/// `~/.pi/agent/settings.json`. `PIR_STOP_SKILL` wins when set; anything
/// unparseable falls through to the file, then to on.
pub fn stop_skill_default() -> bool {
    if let Ok(v) = std::env::var("PIR_STOP_SKILL") {
        let t = v.trim().to_ascii_lowercase();
        if ["0", "off", "false", "no", "disable", "disabled"].contains(&t.as_str()) {
            return false;
        }
        if ["1", "on", "true", "yes", "enable", "enabled"].contains(&t.as_str()) {
            return true;
        }
    }
    let p = pi_dir().join("agent").join("settings.json");
    let raw = fs::read_to_string(p).unwrap_or_default();
    let v: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    v.get("stopSkill").and_then(Value::as_bool).unwrap_or(true)
}

/// Persist the stop-skill default (writes `stopSkill` into
/// `~/.pi/agent/settings.json`, preserving the other keys).
pub fn set_stop_skill_default(on: bool) -> Result<PathBuf, String> {
    let p = pi_dir().join("agent").join("settings.json");
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).map_err(|e| settings_write_error(parent, &e))?;
    }
    let mut v: Value = fs::read_to_string(&p)
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    if !v.is_object() {
        v = Value::Object(serde_json::Map::new());
    }
    v.as_object_mut().unwrap().insert("stopSkill".into(), Value::Bool(on));
    fs::write(&p, serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?)
        .map_err(|e| settings_write_error(&p, &e))?;
    Ok(p)
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
            reasoning: false,
            thinking_format: None,
            supports_reasoning_effort: None,
            session_affinity_format: None,
            thinking_level_map: Default::default(),
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
        // Provider-qualified labels: `/model op<Tab>` offers the full
        // `opencode/opencode-chat` label (resolves unambiguously), and the
        // prefix hit still ranks above the infix fallbacks.
        assert_eq!(ms.first().map(String::as_str), Some("opencode/opencode-chat"));
        // The infix-only matches are still offered as fallbacks, just after.
        assert!(ms.iter().any(|c| c.starts_with("anthropic/")), "infix matches must remain: {ms:?}");
    }

    /// The `opencode-go/muse-spark-1.3-contributor` bug: `/model opencode-go/`
    /// showed only the first 10 models (alphabetically ending at `grok-4.6`)
    /// because both the completer and the hinter passed a hardcoded `limit` of
    /// 10 to `match_models`, which truncates *after* ranking. `opencode-go`
    /// ships 27 models, so the two `muse-*` entries (alphabetical positions
    /// 20/21) never appeared in the list — the provider looked complete while
    /// silently hiding models. Pin the full-provider-listing behaviour: every
    /// model of a provider must be offered when completing `provider/`, using
    /// the real 27-entry `opencode-go` id set.
    ///
    /// NOTE: the live catalog renamed these to bare `muse-spark-1.2` /
    /// `muse-spark-1.3` (no `-contributor` suffix). This test keeps the
    /// historical `-contributor` ids because it pins the truncation behaviour,
    /// not the live names; the `mu_short_fragment_expands_to_qualified_muse`
    /// test below uses the live ids.
    #[test]
    fn provider_prefix_completion_lists_every_model() {
        // The exact live `opencode-go` model ids, in catalog order.
        const OPCODE_GO: &[&str] = &[
            "deepseek-v4-flash",
            "deepseek-v4-flash-vision-exp",
            "deepseek-v4-pro",
            "deepseek-v4.1-flash",
            "glm-5.1",
            "glm-5.2",
            "glm-5.3",
            "glm-5.3-flash",
            "gpt-5.6-luna",
            "grok-4.6",
            "hy3",
            "hy4-preview",
            "kimi-k2.6",
            "kimi-k2.7-code",
            "kimi-k3",
            "longcat-2.0",
            "mimo-v2.5",
            "mimo-v2.5-pro",
            "minimax-m2.7",
            "minimax-m3",
            "muse-spark-1.2-contributor",
            "muse-spark-1.3-contributor",
            "qwen3.6-plus",
            "qwen3.7-max",
            "qwen3.7-plus",
            "qwen3.8-flash",
            "qwen3.8-max",
        ];
        let provs = vec![Provider {
            id: Some("opencode-go".into()),
            name: None,
            api: Some("openai-completions".into()),
            base_url: Some("https://opencode.ai/zen/go/v1".into()),
            api_key: None,
            models: OPCODE_GO.iter().map(|id| mk(id, id)).collect(),
        }];

        let ms = match_models(&provs, "opencode-go/", crate::term::MODEL_COMPLETION_LIMIT);

        // Every model is offered — nothing is silently truncated.
        assert_eq!(
            ms.len(),
            OPCODE_GO.len(),
            "all {} models must be listed, got {}: {ms:?}",
            OPCODE_GO.len(),
            ms.len()
        );
        // The two that the old cap of 10 hid are present.
        for id in ["muse-spark-1.3-contributor", "muse-spark-1.2-contributor"] {
            let label = format!("opencode-go/{id}");
            assert!(ms.contains(&label), "missing {label} from completion: {ms:?}");
        }
        // And the old cap would genuinely have missed them (guards the test
        // itself against a catalogue-order change that made muse sort early).
        let capped = match_models(&provs, "opencode-go/", 10);
        assert!(
            !capped.iter().any(|c| c.contains("muse")),
            "with the old cap of 10, muse must be absent — otherwise this test \
             no longer proves the truncation bug: {capped:?}"
        );
    }

    /// The `/model mu` report: typing `mu` must offer the provider-qualified
    /// muse labels (`opencode-go/muse-spark-1.2`, `opencode-go/muse-spark-1.3`)
    /// ranked first, and the buffer helper must expand `/model mu` to the
    /// common prefix `/model opencode-go/muse-spark-1.`. The old matcher
    /// returned bare ids (`muse-spark-1.3`), dropping the provider; the old
    /// TUI/GUI completers had no `/model` branch at all.
    #[test]
    fn mu_short_fragment_expands_to_qualified_muse() {
        let provs = vec![
            Provider {
                id: Some("anthropic".into()),
                name: None,
                api: Some("anthropic".into()),
                base_url: Some("https://api.anthropic.com/v1".into()),
                api_key: None,
                models: vec![mk("claude-sonnet-4-5", "Claude Sonnet 4.5")],
            },
            Provider {
                id: Some("opencode-go".into()),
                name: None,
                api: Some("openai-completions".into()),
                base_url: Some("https://opencode.ai/zen/go/v1".into()),
                api_key: None,
                models: vec![
                    mk("muse-spark-1.2", "Muse Spark 1.2"),
                    mk("muse-spark-1.3", "Muse Spark 1.3"),
                    mk("deepseek-v4-flash", "DeepSeek V4 Flash"),
                ],
            },
        ];
        let ms = match_models(&provs, "mu", crate::term::MODEL_COMPLETION_LIMIT);
        // Provider-qualified, never bare ids.
        assert!(
            ms.contains(&"opencode-go/muse-spark-1.2".to_string()),
            "missing qualified muse 1.2: {ms:?}"
        );
        assert!(
            ms.contains(&"opencode-go/muse-spark-1.3".to_string()),
            "missing qualified muse 1.3: {ms:?}"
        );
        assert!(
            !ms.iter().any(|c| c == "muse-spark-1.2" || c == "muse-spark-1.3"),
            "bare ids must not be offered: {ms:?}"
        );
        // Model-id-prefix hits rank first (ahead of any substring fallback).
        assert_eq!(
            ms.first().map(String::as_str),
            Some("opencode-go/muse-spark-1.2"),
            "muse must rank first for 'mu': {ms:?}"
        );
        // End to end: `/model mu` + Tab -> the common qualified prefix.
        assert_eq!(
            complete_model_buffer("/model mu", &provs),
            Some("/model opencode-go/muse-spark-1.".to_string()),
        );
        assert_eq!(
            complete_model_buffer("/m mu", &provs),
            Some("/m opencode-go/muse-spark-1.".to_string()),
        );
        // A bare command gains a trailing space; unknown fragments stay None.
        assert_eq!(complete_model_buffer("/model", &provs), Some("/model ".to_string()));
        assert_eq!(complete_model_buffer("/model zzz", &provs), None);
    }
}

/// Serializes tests that mutate process-global env (`PI_DIR` / `PIR_WT`) so
/// they can't race when the test binary runs them in parallel. Acquired by the
/// config, security, and wt-extension tests that set these vars.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod worktree_settings_tests {
    use super::*;

    // The worktree default is persisted to settings.json via PI_DIR (which
    // `pi_dir()` honours in tests); verify the roundtrip and the true default.
    #[test]
    fn worktrees_default_roundtrip() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("pir_wt_cfg_{}", std::process::id()));
        let old = std::env::var_os("PI_DIR");
        // SAFETY: edition 2024 marks env mutation unsafe; pir confines
        // it to startup config and explicit session toggles.
        unsafe { std::env::set_var("PI_DIR", &dir); }
        assert!(!worktrees_default(), "default is off (guard posture; worktrees are opt-in)");
        set_worktrees_default(true).unwrap();
        assert!(worktrees_default(), "persisted on must read back");
        set_worktrees_default(false).unwrap();
        assert!(!worktrees_default(), "persisted off must read back");
        match old {
            Some(v) => unsafe { std::env::set_var("PI_DIR", v) },
            None => unsafe { std::env::remove_var("PI_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // `PIR_HTTP_BACKEND` wins; otherwise the `http_backend` settings.json
    // key under an isolated PI_DIR; otherwise None (caller defaults). File
    // writes go to a temp dir so the real user config is never touched.
    #[test]
    fn http_backend_name_env_and_file() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap();
        let old_backend = std::env::var_os("PIR_HTTP_BACKEND");
        let old_dir = std::env::var_os("PI_DIR");
        let dir = std::env::temp_dir().join(format!("pir_http_cfg_{}", std::process::id()));
        unsafe {
            std::env::set_var("PI_DIR", &dir);
            std::env::set_var("PIR_HTTP_BACKEND", "ureq");
        }
        assert_eq!(http_backend_name(), Some("ureq".to_string()), "env wins");
        unsafe { std::env::remove_var("PIR_HTTP_BACKEND"); }
        assert_eq!(http_backend_name(), None, "no file means default");
        let agent = dir.join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(agent.join("settings.json"), "{\"http_backend\": \"ureq\"}").unwrap();
        assert_eq!(http_backend_name(), Some("ureq".to_string()), "file fallback");
        match old_backend {
            Some(v) => unsafe { std::env::set_var("PIR_HTTP_BACKEND", v) },
            None => unsafe { std::env::remove_var("PIR_HTTP_BACKEND") },
        }
        match old_dir {
            Some(v) => unsafe { std::env::set_var("PI_DIR", v) },
            None => unsafe { std::env::remove_var("PI_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The stop skill is on by default; `PIR_STOP_SKILL` wins over the
    // `stopSkill` settings.json key. Env handling mirrors the other
    // `PIR_*` defaults (file writes go to a temp PI_DIR).
    #[test]
    fn stop_skill_default_env_and_file() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap();
        let old_skill = std::env::var_os("PIR_STOP_SKILL");
        let old_dir = std::env::var_os("PI_DIR");
        let dir = std::env::temp_dir().join(format!("pir_stop_cfg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::set_var("PI_DIR", &dir);
            std::env::remove_var("PIR_STOP_SKILL");
        }
        assert!(stop_skill_default(), "default is on");
        unsafe { std::env::set_var("PIR_STOP_SKILL", "0"); }
        assert!(!stop_skill_default(), "env 0 disables");
        unsafe { std::env::set_var("PIR_STOP_SKILL", "off"); }
        assert!(!stop_skill_default(), "env off disables");
        unsafe { std::env::remove_var("PIR_STOP_SKILL"); }
        set_stop_skill_default(false).unwrap();
        assert!(!stop_skill_default(), "persisted off must read back");
        set_stop_skill_default(true).unwrap();
        assert!(stop_skill_default(), "persisted on must read back");
        unsafe { std::env::set_var("PIR_STOP_SKILL", "1"); }
        assert!(stop_skill_default(), "env 1 wins over a persisted off");
        // An unparseable env value falls through to the file (still on here).
        unsafe { std::env::set_var("PIR_STOP_SKILL", "maybe"); }
        assert!(stop_skill_default(), "junk env must fall through to the file");
        match old_skill {
            Some(v) => unsafe { std::env::set_var("PIR_STOP_SKILL", v) },
            None => unsafe { std::env::remove_var("PIR_STOP_SKILL") },
        }
        match old_dir {
            Some(v) => unsafe { std::env::set_var("PI_DIR", v) },
            None => unsafe { std::env::remove_var("PI_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_write_error_names_path_and_user() {
        // The bug: `/default-model` failed with a bare "Permission denied
        // (os error 13)" — no path, no user. The message must name the file
        // and the effective user/HOME so an euid/HOME mismatch (sandbox drop
        // or su/sudo leaving HOME at someone else's `~/.pi`) is visible.
        let err = settings_write_error(
            std::path::Path::new("/root/.pi/agent/settings.json"),
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Permission denied (os error 13)",
            ),
        );
        assert!(err.contains("/root/.pi/agent/settings.json"), "must name the file: {err}");
        assert!(err.contains("Permission denied"), "must keep the cause: {err}");
        assert!(err.contains("euid"), "must name the effective user: {err}");
        assert!(err.contains("HOME"), "must show HOME: {err}");
    }

    #[test]
    fn model_api_maps_responses_kind() {
        // opencode-go marks Responses-only models `openai-responses`; they
        // need the `/responses` shape (chat bodies 500 on them).
        let mut p = Provider {
            id: Some("opencode-go".into()),
            name: None,
            base_url: Some("https://opencode.ai/zen/go/v1".into()),
            api_key: Some("k".into()),
            api: None,
            models: vec![],
        };
        let mk = |api: &str| Model {
            id: "m".into(),
            name: None,
            api_override: Some(api.into()),
            url_override: None,
            no_reasoning_effort: false,
            reasoning: false,
            thinking_format: None,
            supports_reasoning_effort: None,
            session_affinity_format: None,
            thinking_level_map: Default::default(),
            context: None,
            max_tokens: None,
            price_per_1k: None,
        };
        assert_eq!(p.model_api(&mk("openai-responses")), Some(ApiKind::OpenAiResponses));
        assert_eq!(p.model_api(&mk("openai-completions")), Some(ApiKind::OpenAi));
        assert_eq!(p.model_api(&mk("anthropic-messages")), Some(ApiKind::Anthropic));
        p.api = Some("openai-responses".into());
        assert_eq!(p.kind(), Some(ApiKind::OpenAiResponses));
    }

    /// `load_providers` must carry each model's own `api`/`baseUrl` into
    /// `api_override`/`url_override`. Regression: both branches hardcoded
    /// `api_override: None`, so a per-model `openai-responses` entry inherited
    /// the provider-level `openai-completions` and pir POSTed a chat body
    /// (`messages`/`max_tokens`) to `/responses` — the provider answered
    /// `500 Internal server error` and the turn retried forever. This is the
    /// exact `opencode-go` store shape that made muse-spark unreachable while
    /// the catalog itself was fine.
    #[test]
    fn model_api_override_survives_store_load() {
        let _env = TEST_ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("pir_apiovr_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let agent = dir.join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        // Mirrors the live store: provider is openai-completions, the
        // muse-spark entry overrides to openai-responses.
        std::fs::write(
            agent.join("models-store.json"),
            serde_json::json!({
                "providers": {
                    "opencode-go": {
                        "baseUrl": "https://opencode.ai/zen/go/v1",
                        "apiKey": "k",
                        "api": "openai-completions",
                        "models": [
                            { "id": "deepseek-v4-flash", "api": "openai-completions" },
                            { "id": "muse-spark-1.3-contributor",
                              "api": "openai-responses",
                              "baseUrl": "https://opencode.ai/zen/go/v1",
                              "reasoning": true },
                        ]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let old = std::env::var_os("PI_DIR");
        // SAFETY: edition 2024 marks env mutation unsafe; pir confines
        // it to startup config and explicit session toggles.
        unsafe { std::env::set_var("PI_DIR", &dir); }
        let providers = load_providers().expect("store must load");
        match old {
            Some(v) => unsafe { std::env::set_var("PI_DIR", v) },
            None => unsafe { std::env::remove_var("PI_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);

        let p = providers
            .iter()
            .find(|p| p.pid() == "opencode-go")
            .expect("opencode-go present");
        let muse = p
            .models
            .iter()
            .find(|m| m.id == "muse-spark-1.3-contributor")
            .expect("muse model present");
        assert_eq!(
            muse.api_override.as_deref(),
            Some("openai-responses"),
            "per-model `api` must survive the load"
        );
        assert_eq!(
            p.model_api(muse),
            Some(ApiKind::OpenAiResponses),
            "muse must route to /responses, not chat/completions"
        );
        assert_eq!(muse.url_override.as_deref(), Some("https://opencode.ai/zen/go/v1"));
        // The sibling that does NOT override still inherits the provider API.
        let flash = p.models.iter().find(|m| m.id == "deepseek-v4-flash").unwrap();
        assert_eq!(p.model_api(flash), Some(ApiKind::OpenAi));
    }

    /// The pre-drop catalog snapshot must be served when a later (post-drop)
    /// read can't reach the store — the `PI_DIR`-at-root-owned-store case,
    /// where `become_user` has already dropped pir to `ai_X` and the invoker's
    /// `0600` store is now EACCES. Without the snapshot the catalog silently
    /// degrades to the auth fallback and model switching stops resolving.
    #[test]
    fn seeded_catalog_survives_unreadable_store() {
        let _env = TEST_ENV_LOCK.lock().unwrap();
        // A store path that cannot be read (a directory, not a file).
        let dir = std::env::temp_dir().join(format!("pir_seed_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let agent = dir.join("agent");
        std::fs::create_dir_all(agent.join("models-store.json")).unwrap();
        let old = std::env::var_os("PI_DIR");
        // SAFETY: edition 2024 marks env mutation unsafe; pir confines
        // it to startup config and explicit session toggles.
        unsafe { std::env::set_var("PI_DIR", &dir); }
        // An unreadable store alone would fall back to auth (empty here).
        let unseeded = load_providers().map(|p| p.len()).unwrap_or(0);
        // Seed the snapshot the way `main` does, then confirm it is served
        // even though the on-disk read still fails.
        let seeded = vec![Provider {
            id: Some("opencode-go".into()),
            name: None,
            base_url: Some("https://opencode.ai/zen/go/v1".into()),
            api_key: Some("k".into()),
            api: Some("openai-completions".into()),
            models: vec![compat_model(None, &[])],
        }];
        seed_catalog(&seeded);
        let got = load_providers().expect("snapshot must be served");
        match old {
            Some(v) => unsafe { std::env::set_var("PI_DIR", v) },
            None => unsafe { std::env::remove_var("PI_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            got.iter().filter(|p| p.pid() == "opencode-go").count(),
            1,
            "seeded catalog must survive an unreadable store (unseeded len was {unseeded})"
        );
        assert!(
            invoker_catalog().is_some(),
            "snapshot must remain available for the process lifetime"
        );
    }

    /// The real-world bug this pins: `opencode-go/muse-spark-1.3-contributor`
    /// missing from `/models` was never a catalog/filter bug — pir read the
    /// *sandbox* user's store (`~ai_pir/.pi/...`, one `local/fake` provider)
    /// instead of the invoking user's rich catalog, so the whole `opencode-go`
    /// provider was absent from the list.
    ///
    /// `main` resolves `HOME` as the invoking user *before* `become_user`
    /// (which rewrites `HOME` to the sandbox account). `pi_dir()` is derived
    /// from `HOME`, so the pre-drop read must land on the invoking user's
    /// `~/.pi/agent/models-store.json` — not the `ai_X` store. This simulates
    /// both sides of the drop by moving `HOME` between two real stores and
    /// asserts the catalog follows: the rich one is seen pre-drop, the stub
    /// never shadows it.
    #[test]
    fn pre_drop_load_reads_invoking_user_store_not_sandbox() {
        let _env = TEST_ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("pir_homedrop_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        // The INVOKING user's home (`/root`): the real 0600 store with the
        // opencode-go provider, including the exact muse entry from the report.
        let invoker_home = root.join("invoker");
        let invoker_agent = invoker_home.join(".pi").join("agent");
        std::fs::create_dir_all(&invoker_agent).unwrap();
        std::fs::write(
            invoker_agent.join("models-store.json"),
            serde_json::json!({
                "providers": {
                    "opencode-go": {
                        "baseUrl": "https://opencode.ai/zen/go/v1",
                        "apiKey": "k",
                        "api": "openai-completions",
                        "models": [
                            { "id": "deepseek-v4-flash", "api": "openai-completions" },
                            { "id": "muse-spark-1.3-contributor",
                              "api": "openai-responses",
                              "baseUrl": "https://opencode.ai/zen/go/v1" },
                        ]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        // The SANDBOX user's home (`/home/ai_pir`): a tiny stub store with no
        // opencode-go at all — exactly what made muse unreachable.
        let sandbox_home = root.join("ai_pir");
        let sandbox_agent = sandbox_home.join(".pi").join("agent");
        std::fs::create_dir_all(&sandbox_agent).unwrap();
        std::fs::write(
            sandbox_agent.join("models-store.json"),
            serde_json::json!({
                "providers": [{
                    "id": "local",
                    "baseUrl": "http://127.0.0.1:8799/v1",
                    "apiKey": "literalkey123",
                    "api": "openai",
                    "models": [{ "id": "fake", "name": "Fake", "context": 8000 }]
                }]
            })
            .to_string(),
        )
        .unwrap();

        let old_home = std::env::var_os("HOME");
        let old_pidir = std::env::var_os("PI_DIR");
        // `PI_DIR` wins over `HOME` in `pi_dir()`, so clear it to exercise the
        // HOME-derived path this test is about.
        unsafe { std::env::remove_var("PI_DIR"); }

        // --- Pre-drop: running as the invoking user (HOME=/root-like). ---
        // SAFETY: edition 2024 marks env mutation unsafe; pir confines
        // it to startup config and explicit session toggles.
        unsafe { std::env::set_var("HOME", &invoker_home); }
        let pre_drop = load_providers().expect("invoking-user store must load");
        let pre_has_muse = pre_drop.iter().any(|p| {
            p.pid() == "opencode-go"
                && p.models
                    .iter()
                    .any(|m| m.id == "muse-spark-1.3-contributor")
        });

        // --- Post-drop: `become_user` rewrote HOME to the sandbox account. ---
        unsafe { std::env::set_var("HOME", &sandbox_home); }
        let post_drop = load_providers().expect("sandbox store must load");
        let post_has_muse = post_drop.iter().any(|p| {
            p.pid() == "opencode-go"
                && p.models
                    .iter()
                    .any(|m| m.id == "muse-spark-1.3-contributor")
        });

        // Restore env before any assertion so a failure can't leak state.
        match old_home {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match old_pidir {
            Some(v) => unsafe { std::env::set_var("PI_DIR", v) },
            None => unsafe { std::env::remove_var("PI_DIR") },
        }
        let _ = std::fs::remove_dir_all(&root);

        // The whole point: the pre-drop read IS the invoking user's authority.
        assert!(
            pre_has_muse,
            "pre-drop load must read the INVOKING user's store (HOME-derived), \
             so opencode-go/muse-spark-1.3-contributor is present"
        );
        // And the sandbox store genuinely lacks it — proving the drop is what
        // changes the answer (a regression that read the sandbox store first
        // would fail the assertion above, not silently pass).
        assert!(
            !post_has_muse,
            "the sandbox ai_ user's stub store has no opencode-go, so this test \
             actually discriminates between the two paths"
        );
    }

    /// Build a reasoning model with the given format + level map, mirroring a
    /// `models-store.json` entry (e.g. opencode-go's deepseek-v4.1-flash).
    fn compat_model(
        format: Option<&str>,
        map: &[(&str, Option<&str>)],
    ) -> Model {
        Model {
            id: "m".into(),
            name: None,
            context: Some(1_000_000),
            max_tokens: Some(384_000),
            api_override: None,
            url_override: None,
            no_reasoning_effort: false,
            reasoning: true,
            thinking_format: format.map(str::to_string),
            supports_reasoning_effort: None,
            session_affinity_format: None,
            thinking_level_map: map
                .iter()
                .map(|(k, v)| (k.to_string(), v.map(str::to_string)))
                .collect(),
            price_per_1k: None,
        }
    }

    fn level_names(levels: &[ThinkingLevel]) -> Vec<&'static str> {
        levels.iter().map(|l| l.as_str()).collect()
    }

    #[test]
    fn thinking_map_parses_from_store_json() {
        // The exact opencode-go deepseek-v4.1-flash shape: top-level
        // `reasoning`, nested `compat.thinkingFormat`, `thinkingLevelMap`
        // with nulls.
        let mv: Value = serde_json::json!({
            "id": "deepseek-v4.1-flash",
            "reasoning": true,
            "compat": { "thinkingFormat": "deepseek" },
            "thinkingLevelMap": {
                "minimal": null, "low": null, "medium": null,
                "high": "high", "max": "max"
            }
        });
        let pval: Value = serde_json::json!({});
        assert!(parse_reasoning(&mv));
        assert_eq!(
            parse_compat_str(&mv, &pval, "thinkingFormat", "thinking_format"),
            Some("deepseek".to_string())
        );
        let map = parse_thinking_map(&mv);
        assert_eq!(map.get("minimal"), Some(&None));
        assert_eq!(map.get("medium"), Some(&None));
        assert_eq!(map.get("high"), Some(&Some("high".to_string())));
        assert_eq!(map.get("max"), Some(&Some("max".to_string())));
        assert_eq!(map.get("off"), None); // missing ≠ null
    }

    #[test]
    fn provider_compat_is_fallback_for_model() {
        // pi merges provider-level `compat` under model-level `compat`.
        let mv: Value = serde_json::json!({ "id": "m", "reasoning": true });
        let pval: Value =
            serde_json::json!({ "compat": { "thinkingFormat": "qwen" } });
        assert_eq!(
            parse_compat_str(&mv, &pval, "thinkingFormat", "thinking_format"),
            Some("qwen".to_string())
        );
        // Model-level wins over provider-level.
        let mv2: Value = serde_json::json!({
            "id": "m",
            "compat": { "thinkingFormat": "deepseek" }
        });
        assert_eq!(
            parse_compat_str(&mv2, &pval, "thinkingFormat", "thinking_format"),
            Some("deepseek".to_string())
        );
    }

    #[test]
    fn supported_levels_match_pi_for_deepseek_v41() {
        // pi `getSupportedThinkingLevels` for
        // `{minimal:null, low:null, medium:null, high:"high", max:"max"}`
        // (off/xhigh missing): off/high/max only.
        let m = compat_model(
            Some("deepseek"),
            &[
                ("minimal", None),
                ("low", None),
                ("medium", None),
                ("high", Some("high")),
                ("max", Some("max")),
            ],
        );
        assert_eq!(level_names(&m.supported_levels()), vec!["off", "high", "max"]);
        // Clamp walks up first (pi `clampThinkingLevel`): medium → high,
        // minimal → high, xhigh → max, low → high.
        assert_eq!(m.clamp_thinking(ThinkingLevel::Medium), ThinkingLevel::High);
        assert_eq!(m.clamp_thinking(ThinkingLevel::Minimal), ThinkingLevel::High);
        assert_eq!(m.clamp_thinking(ThinkingLevel::XHigh), ThinkingLevel::Max);
        assert_eq!(m.clamp_thinking(ThinkingLevel::Low), ThinkingLevel::High);
        assert_eq!(m.clamp_thinking(ThinkingLevel::High), ThinkingLevel::High);
        assert_eq!(m.clamp_thinking(ThinkingLevel::Off), ThinkingLevel::Off);
    }

    #[test]
    fn non_reasoning_models_offer_only_off() {
        // pi: `!model.reasoning` → `["off"]`.
        let mut m = compat_model(None, &[]);
        m.reasoning = false;
        assert_eq!(level_names(&m.supported_levels()), vec!["off"]);
        assert_eq!(m.clamp_thinking(ThinkingLevel::High), ThinkingLevel::Off);
    }

    #[test]
    fn mapped_effort_honors_explicit_strings() {
        // Max must stay "max" (not collapse to pir's default "high").
        let m = compat_model(
            Some("deepseek"),
            &[("high", Some("high")), ("max", Some("max"))],
        );
        assert_eq!(m.mapped_effort(ThinkingLevel::High), Some("high".to_string()));
        assert_eq!(m.mapped_effort(ThinkingLevel::Max), Some("max".to_string()));
        // No map entry → legacy collapsed names; minimal → none.
        let plain = compat_model(None, &[]);
        assert_eq!(
            plain.mapped_effort(ThinkingLevel::XHigh),
            Some("high".to_string())
        );
        assert_eq!(plain.mapped_effort(ThinkingLevel::Minimal), None);
        // Off only maps with an explicit string (e.g. "none").
        assert_eq!(plain.mapped_effort(ThinkingLevel::Off), None);
        let off_none = compat_model(None, &[("off", Some("none"))]);
        assert_eq!(
            off_none.mapped_effort(ThinkingLevel::Off),
            Some("none".to_string())
        );
    }

    #[test]
    fn session_affinity_parses_model_over_provider_both_spellings() {
        // The exact muse-spark catalog shape that 400s Console Go when the
        // session header is sent: `compat.sessionAffinityFormat`.
        let mv: Value = serde_json::json!({
            "id": "muse-spark-1.3-contributor",
            "compat": { "sessionAffinityFormat": "openai-nosession" }
        });
        let pval: Value = serde_json::json!({});
        assert_eq!(
            parse_compat_str(&mv, &pval, "sessionAffinityFormat", "session_affinity_format"),
            Some("openai-nosession".to_string())
        );
        // snake_case spelling is accepted too.
        let mv_snake: Value = serde_json::json!({
            "id": "m",
            "compat": { "session_affinity_format": "openai-nosession" }
        });
        assert_eq!(
            parse_compat_str(&mv_snake, &pval, "sessionAffinityFormat", "session_affinity_format"),
            Some("openai-nosession".to_string())
        );
        // Provider-level compat is the fallback; model-level wins.
        let pval_prov: Value =
            serde_json::json!({ "compat": { "sessionAffinityFormat": "openai-nosession" } });
        let mv_plain: Value = serde_json::json!({ "id": "m" });
        assert_eq!(
            parse_compat_str(&mv_plain, &pval_prov, "sessionAffinityFormat", "session_affinity_format"),
            Some("openai-nosession".to_string())
        );
        let mv_win: Value = serde_json::json!({
            "id": "m",
            "compat": { "sessionAffinityFormat": "openai-session" }
        });
        assert_eq!(
            parse_compat_str(&mv_win, &pval_prov, "sessionAffinityFormat", "session_affinity_format"),
            Some("openai-session".to_string())
        );
    }

    #[test]
    fn no_session_affinity_matches_nosession_variants() {
        let mut m = compat_model(None, &[]);
        assert!(!m.no_session_affinity(), "unset means send the header");
        for v in ["openai-nosession", "OpenAI-NoSession", " openai-nosession ", "no_session", "none"] {
            m.session_affinity_format = Some(v.to_string());
            assert!(m.no_session_affinity(), "{v:?} must suppress the header");
        }
        for v in ["openai-session", "openai", ""] {
            m.session_affinity_format = Some(v.to_string());
            assert!(!m.no_session_affinity(), "{v:?} must keep the header");
        }
    }
}
