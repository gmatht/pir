use serde::Deserialize;
use serde_json::Value;
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

    Ok(providers)
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

/// Path to the projects.json file that maps project names to the per-project
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
        .unwrap()
        .entry("projects")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let obj = projects.as_object_mut().unwrap();
    let entry = obj.entry(project.to_string()).or_insert_with(|| Value::Object(serde_json::Map::new()));
    entry.as_object_mut().unwrap().insert("user".into(), Value::String(user.to_string()));
    entry.as_object_mut().unwrap().insert("path".into(), Value::String(path.to_string()));
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

    out.sort();
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

