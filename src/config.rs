use crate::term;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

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

    /// Wire format: explicit `api` field wins, else infer from the base URL.
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
        self.api_key.as_ref().map(|k| expand_env(k)).filter(|k| !k.is_empty())
    }
}

/// pi stores keys as "{env:VARNAME}" — expand them, pass literals through.
pub fn expand_env(s: &str) -> String {
    if let Some(var) = s.strip_prefix("{env:").and_then(|r| r.strip_suffix('}')) {
        std::env::var(var).unwrap_or_default()
    } else {
        s.to_string()
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

/// Read ~/.pi/models.json. Tolerant to providers being a list (pi's default)
/// or a map keyed by provider id. Never writes over an existing file.
pub fn load_providers() -> Result<Vec<Provider>, String> {
    let path = pi_dir().join("models.json");
    if !path.exists() {
        let _ = fs::create_dir_all(pi_dir());
        let _ = fs::write(&path, DEFAULT_MODELS_JSON);
        eprintln!("{} created starter {}", term::dim("·"), path.display());
    }
    let raw = fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut v: Value =
        serde_json::from_str(&raw).map_err(|e| format!("parsing {}: {e}", path.display()))?;

    if let Some(ps) = v.get_mut("providers") {
        if ps.is_object() {
            let map: BTreeMap<String, Value> =
                serde_json::from_value(ps.take()).map_err(|e| e.to_string())?;
            let mut list = Vec::new();
            for (key, mut pv) in map {
                if pv.get("id").is_none() {
                    if let Some(o) = pv.as_object_mut() {
                        o.insert("id".into(), Value::String(key));
                    }
                }
                list.push(pv);
            }
            *ps = Value::Array(list);
        }
    }

    #[derive(Deserialize)]
    struct ModelsFile {
        #[serde(default)]
        providers: Vec<Provider>,
    }
    let f: ModelsFile =
        serde_json::from_value(v).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(f.providers)
}

/// Optional: pi's agent settings may pin a default model.
pub fn default_model_setting() -> Option<String> {
    let p = pi_dir().join("agent").join("settings.json");
    let raw = fs::read_to_string(p).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    for key in ["model", "defaultModel", "default_model"] {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            return Some(s.to_string());
        }
    }
    None
}

/// Resolve "provider/model", bare model id, or a unique fuzzy substring.
pub fn select<'a>(
    providers: &'a [Provider],
    selector: &str,
) -> Result<(&'a Provider, &'a Model), String> {
    let sel = selector.trim().to_lowercase();

    if let Some((pid, mid)) = selector.trim().split_once('/') {
        for p in providers {
            if p.pid().eq_ignore_ascii_case(pid) {
                for m in &p.models {
                    if m.id.eq_ignore_ascii_case(mid) {
                        return Ok((p, m));
                    }
                }
            }
        }
    }
    for p in providers {
        for m in &p.models {
            if m.id.eq_ignore_ascii_case(selector.trim()) {
                return Ok((p, m));
            }
        }
    }
    let hits: Vec<(&'a Provider, &'a Model)> = providers
        .iter()
        .flat_map(|p| p.models.iter().map(move |m| (p, m)))
        .filter(|(p, m)| {
            format!("{}/{}", p.pid(), m.id).to_lowercase().contains(&sel)
                || m.name.as_deref().unwrap_or("").to_lowercase().contains(&sel)
        })
        .collect();
    match hits.as_slice() {
        [only] => Ok(*only),
        [] => Err(format!("no model matches '{selector}'")),
        _ => Err(format!(
            "'{selector}' is ambiguous: {}",
            hits.iter().map(|(p, m)| p.label(m)).collect::<Vec<_>>().join(", ")
        )),
    }
}

const DEFAULT_MODELS_JSON: &str = r#"{
  "providers": [
    {
      "id": "anthropic",
      "name": "Anthropic",
      "api": "anthropic",
      "baseUrl": "https://api.anthropic.com/v1",
      "apiKey": "{env:ANTHROPIC_API_KEY}",
      "models": [
        { "id": "claude-sonnet-4-5", "name": "Claude Sonnet 4.5", "context": 200000, "maxTokens": 64000 },
        { "id": "claude-haiku-4-5", "name": "Claude Haiku 4.5", "context": 200000, "maxTokens": 32000 }
      ]
    },
    {
      "id": "openai",
      "name": "OpenAI",
      "api": "openai",
      "baseUrl": "https://api.openai.com/v1",
      "apiKey": "{env:OPENAI_API_KEY}",
      "models": [
        { "id": "gpt-4.1", "name": "GPT-4.1", "context": 1000000, "maxTokens": 32000 },
        { "id": "gpt-4.1-mini", "name": "GPT-4.1 mini", "context": 1000000, "maxTokens": 32000 }
      ]
    },
    {
      "id": "openrouter",
      "name": "OpenRouter",
      "api": "openai",
      "baseUrl": "https://openrouter.ai/api/v1",
      "apiKey": "{env:OPENROUTER_API_KEY}",
      "models": [
        { "id": "anthropic/claude-sonnet-4.5", "name": "Claude Sonnet 4.5", "context": 200000, "maxTokens": 64000 },
        { "id": "google/gemini-2.5-pro", "name": "Gemini 2.5 Pro", "context": 1000000, "maxTokens": 64000 }
      ]
    }
  ]
}"#;
