//! Background conversation-title generation using a cheap "light" model.
//!
//! Every interactive / one-shot / background session leaves a JSONL transcript.
//! Rather than show a raw `pir-<timestamp>-sh<pid>` filename in `/sessions` and
//! `/unfinished`, pir can ask a *fast, cheap* model (default `cerebras/gemma4`)
//! to suggest a short title from the last few prompts. The title is written to
//! a `<log>.title` sidecar so it can be displayed without re-reading/summarizing
//! the whole transcript — and the light model runs **silently** (never to
//! stdout), so the user's foreground session is never interrupted.
//!
//! Cerebras (and other cheap providers) impose strict per-minute request/token
//! limits. Title generation is therefore *throttled*: a single process-wide
//! rate limiter caps how often we call the light model, and the work is done on
//! a detached worker thread so the REPL stays responsive. If the limiter is
//! tapped (or the light model is unavailable), the call is silently skipped —
//! the worst case is that a title is generated a little later, or not at all.

use crate::config;
use crate::session;
use serde_json::{json, Value};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Minimum spacing between light-model title calls. Cerebras's free tier is
/// roughly ~30 requests/min *per account*, and `pir` is process-per-terminal —
/// so two or three open terminals would otherwise each fire their own cadence
/// and collectively burst the shared quota. We therefore pace calls **across
/// processes** by stamping a small token file (see [`throttle_wait`]) that every
/// `pir` instance reads before calling. 30s is deliberately conservative (≈2
/// calls/min from each terminal) so the whole fleet stays well under the limit.
/// The title sidecar is durable, so spreading calls out in time costs nothing —
/// a title just lands a little later when the account is busy.
const MIN_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum number of recent user prompts fed to the light model for a title.
/// Keep it tiny (3) so the request is a few hundred tokens at most — well within
/// Cerebras's per-request limits and fast enough to not matter.
const MAX_PROMPTS: usize = 3;

/// Process-wide throttle: the instant the last title call *started*.
/// We use a file token to pace calls across multiple `pir` processes.
fn throttle_wait() -> Duration {
    let token_path = std::env::current_dir()
        .ok()
        .map(|p| p.join(".pir_titler_throttle"))
        .unwrap_or_else(|| PathBuf::from(".pir_titler_throttle"));

    let now = SystemTime::now();
    let last_call = std::fs::metadata(&token_path)
        .and_then(|m| m.modified())
        .ok();

    if let Some(last) = last_call {
        if let Ok(elapsed) = now.duration_since(last) {
            return MIN_INTERVAL.saturating_sub(elapsed);
        }
    }
    Duration::ZERO
}

/// Stamp the throttle token to mark a title call start.
fn stamp_throttle() {
    let token_path = std::env::current_dir()
        .ok()
        .map(|p| p.join(".pir_titler_throttle"))
        .unwrap_or_else(|| PathBuf::from(".pir_titler_throttle"));
    let _ = std::fs::File::create(token_path);
}

/// One-shot guard so that, within a single process, we don't enqueue more than
/// one pending title job at a time (the REPL calls this after every turn).
static PENDING: AtomicBool = AtomicBool::new(false);

/// Maybe generate (and persist) a conversation title for `log`, **eventually**.
///
/// This is the only entry point the rest of the app calls. It:
///   1. skips work when there's no session log (one-shot sessions that opted
///      out of a transcript, or an already-titled session that hasn't changed);
///   2. spawns a detached worker thread so the caller (the REPL) never blocks;
///   3. on the worker, respects the process-wide [`MIN_INTERVAL`] throttle and
///      the availability of the light model, then asks it for a short title and
///      writes it to `<log>.title`.
///
/// It never prints to stdout and never errors to the caller — title generation
/// is best-effort and purely cosmetic.
pub fn maybe_generate_title(log: Option<&PathBuf>) {
    let log = match log {
        Some(p) if !p.as_os_str().is_empty() => p.clone(),
        _ => return, // no transcript to name
    };
    // If a job is already queued/pending in this process, don't stack up more.
    if PENDING.swap(true, Ordering::SeqCst) {
        return;
    }
    let providers = config::load_providers().unwrap_or_default();
    let (prov, model) = match config::resolve_light_model(&providers) {
        Some(pm) => (pm.0.clone(), pm.1.clone()),
        None => {
            PENDING.store(false, Ordering::SeqCst);
            return;
        }
    };
    let base_url = prov.model_base_url(&model).unwrap_or("").to_string();
    let api_kind = prov.model_api(&model).unwrap_or(config::ApiKind::OpenAi);
    let api_key = match prov.api_key() {
        Some(k) => k,
        None => {
            PENDING.store(false, Ordering::SeqCst);
            return;
        }
    };
    let model_id = model.id.clone();

    thread::spawn(move || {
        // Throttle across processes: read the shared token's mtime, claim the
        // slot (stamp the file so the next process sees us), then sleep the
        // remaining interval before calling the light model. Best-effort
        // against a tiny TOCTOU race, but keeps the whole fleet well under the
        // shared per-account quota.
        let wait = throttle_wait();
        stamp_throttle();
        if !wait.is_zero() {
            thread::sleep(wait);
        }

        if let Some(t) = generate_title(&log, api_kind, &base_url, &api_key, &model_id) {
            session::write_title(&log, &t);
        }
        PENDING.store(false, Ordering::SeqCst);
    });
}

// Turn-outcome verdict classification.
//
// Like titles, the *meaning* of how a turn ended ("done", "waiting on you",
// "needs a retry", "blocked", "errored") is derived by the same cheap light
// model, on the same throttled/silent background pipeline. The agent already
// records a coarse structural end-state in `<log>.status.json` (`completed` /
// `interrupted` …); the light model refines that into a short human label so
// `/sessions` and the resume picker can show at a glance whether a thread is
// finished or still needs the user. The verdict is a single lowercase token in
// `<log>.verdict`, parsed into a canonical set so display stays stable even if
// the model wanders a little.
// ---------------------------------------------------------------------------

/// Canonical verdict tokens the light model may assign. Anything else returned
/// by the model is mapped to the closest member (or the structural fallback).
const VERDICTS: &[&str] = &["complete", "waiting", "retry", "blocked", "error"];

/// One-shot guard so this process doesn't stack more than one pending verdict
/// job at a time (mirrors the title guard; the two share the cross-process
/// throttle token so they never burst the shared quota together).
static VERDICT_PENDING: AtomicBool = AtomicBool::new(false);

/// Human-readable expansion of a canonical verdict token for display. Returns
/// an empty string for unknown/empty tokens (so callers can skip the line).
pub fn verdict_label(token: &str) -> &'static str {
    match token.trim() {
        "complete" => "complete",
        "waiting" => "waiting for input",
        "retry" => "needs retry",
        "blocked" => "blocked",
        "error" => "error",
        "interrupted" => "interrupted",
        "incomplete" => "incomplete",
        _ => "",
    }
}

/// Maybe classify the last finished turn of `log` into a verdict — **eventually**,
/// on the same throttled/silent background worker as titles. The coarse outcome
/// is read from the session's existing `<log>.status.json` (written by the agent
/// at turn end): a `completed` turn is refined into `complete` vs `waiting for
/// input`, an `interrupted` turn with an error reason into `retry`/`blocked`/
/// `error`. Structural outcomes (`cancelled`, token-budget) are written
/// directly without calling the model, so they cost zero quota. Best-effort:
/// if the light model is unavailable or throttled, the verdict is simply absent
/// (the structural status is still shown by `/unfinished`).
pub fn maybe_generate_verdict(log: Option<&PathBuf>) {
    let log = match log {
        Some(p) if !p.as_os_str().is_empty() => p.clone(),
        _ => return, // no transcript to classify
    };
    // Derive the coarse outcome from the status sidecar the agent already wrote.
    let outcome = match session::read_status(&log) {
        Some(m) => match m.status {
            session::SessionStatus::Completed | session::SessionStatus::Finished => "complete".to_string(),
            session::SessionStatus::Active => "interrupted".to_string(),
            session::SessionStatus::Interrupted => {
                if m.reason.contains("budget") {
                    "budget".to_string()
                } else if m.reason.contains("cancel") {
                    "cancelled".to_string()
                } else if m.reason.contains("turn error") || m.reason.contains("error") {
                    format!("error:{}", m.reason)
                } else {
                    "interrupted".to_string()
                }
            }
        },
        None => return, // nothing recorded yet
    };
    // Structural outcomes need no model call (and shouldn't spend quota).
    let structural = match outcome.as_str() {
        "cancelled" => Some("interrupted"),
        "budget" => Some("incomplete"),
        _ => None,
    };
    if let Some(v) = structural {
        session::write_verdict(&log, v);
        return;
    }
    // Otherwise refine via the light model (throttled + silent, like titles).
    if VERDICT_PENDING.swap(true, Ordering::SeqCst) {
        return;
    }
    let providers = config::load_providers().unwrap_or_default();
    let (prov, model) = match config::resolve_light_model(&providers) {
        Some(pm) => (pm.0.clone(), pm.1.clone()),
        None => {
            VERDICT_PENDING.store(false, Ordering::SeqCst);
            return;
        }
    };
    let base_url = prov.model_base_url(&model).unwrap_or("").to_string();
    let api_kind = prov.model_api(&model).unwrap_or(config::ApiKind::OpenAi);
    let api_key = match prov.api_key() {
        Some(k) => k,
        None => {
            VERDICT_PENDING.store(false, Ordering::SeqCst);
            return;
        }
    };
    let model_id = model.id.clone();

    thread::spawn(move || {
        let wait = throttle_wait();
        stamp_throttle();
        if !wait.is_zero() {
            thread::sleep(wait);
        }
        if let Some(v) = classify_verdict(&log, &outcome, api_kind, &base_url, &api_key, &model_id) {
            session::write_verdict(&log, &v);
        }
        VERDICT_PENDING.store(false, Ordering::SeqCst);
    });
}

/// Synchronously classify `log`'s last finished turn into a canonical verdict,
/// returning `None` when the light model is unavailable/errors. Unlike
/// [`maybe_generate_verdict`] (fire-and-forget + throttled), this *blocks* on
/// the verdict so a caller that must *act* on it now (e.g. auto-retry) can. It
/// reuses the exact outcome-derivation + [`classify_verdict`] the background
/// path uses, so structural outcomes (`cancelled`/`budget`) still need no model
/// call and return immediately.
pub fn classify_now(log: &Path) -> Option<String> {
    let outcome = match session::read_status(log) {
        Some(m) => match m.status {
            session::SessionStatus::Completed | session::SessionStatus::Finished => "complete".to_string(),
            session::SessionStatus::Active => "interrupted".to_string(),
            session::SessionStatus::Interrupted => {
                if m.reason.contains("budget") {
                    "budget".to_string()
                } else if m.reason.contains("cancel") {
                    "cancelled".to_string()
                } else if m.reason.contains("turn error") || m.reason.contains("error") {
                    format!("error:{}", m.reason)
                } else {
                    "interrupted".to_string()
                }
            }
        },
        None => return None, // nothing recorded yet
    };
    // Structural outcomes need no model call (and shouldn't spend quota).
    match outcome.as_str() {
        "cancelled" => return Some("interrupted".to_string()),
        "budget" => return Some("incomplete".to_string()),
        _ => {}
    }
    let providers = config::load_providers().unwrap_or_default();
    let (prov, model) = config::resolve_light_model(&providers)?;
    let base_url = prov.model_base_url(&model).unwrap_or("").to_string();
    let api_kind = prov.model_api(&model).unwrap_or(config::ApiKind::OpenAi);
    let api_key = prov.api_key()?;
    classify_verdict(log, &outcome, api_kind, &base_url, &api_key, &model.id)
}

/// The last (user, assistant) text exchange from `log`, for verdict context.
/// Returns `None` when the log can't be read or has no assistant message.
pub(crate) fn last_exchange(log: &Path) -> Option<(String, String)> {
    let Ok(f) = std::fs::File::open(log) else { return None };
    let mut last_user = String::new();
    let mut last_asst = String::new();
    for line in std::io::BufReader::new(f).lines().flatten() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        match v.get("role").and_then(Value::as_str) {
            Some("user") => {
                let t = block_text(&v);
                if !t.is_empty() {
                    last_user = t;
                }
            }
            Some("assistant") => {
                let t = block_text(&v);
                if !t.is_empty() {
                    last_asst = t;
                }
            }
            _ => {}
        }
    }
    if last_asst.is_empty() {
        None
    } else {
        Some((last_user, last_asst))
    }
}

/// Pull the concatenated text blocks out of a JSONL transcript line.
fn block_text(v: &Value) -> String {
    v.get("blocks")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string()
        })
        .unwrap_or_default()
}

/// Ask the light model to refine `outcome` (a coarse hint: "complete" or
/// "error:<msg>") into a canonical verdict from [`VERDICTS`], given the last
/// exchange. Returns `None` on any failure — callers treat that as "skip",
/// never as a hard error (the structural status is still available).
/// POST `system`+`user` to the light model and return its raw first word
/// (lower-cased, trimmed of punctuation/quotes), or `None` on any failure.
/// Shared by the light-model classifiers so they never duplicate the HTTP
/// boilerplate.
fn call_light(system: &str, user: &str) -> Option<String> {
    let providers = config::load_providers().unwrap_or_default();
    let (prov, model) = config::resolve_light_model(&providers)?;
    let base_url = prov.model_base_url(&model).unwrap_or("").to_string();
    let api_kind = prov.model_api(&model).unwrap_or(config::ApiKind::OpenAi);
    let api_key = prov.api_key()?;
    let model_id = model.id.clone();

    let body = json!({
        "model": model_id,
        "max_tokens": 12,
        "stream": false,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user },
        ],
    });

    let url = match api_kind {
        config::ApiKind::Anthropic => format!("{}/messages", base_url.trim_end_matches('/')),
        config::ApiKind::OpenAi => {
            let mut u = base_url.trim_end_matches('/').to_string();
            if !u.ends_with("/chat/completions") && !u.contains('?') {
                u.push_str("/chat/completions");
            }
            u
        }
    };

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(30))
        .timeout_write(Duration::from_secs(15))
        .build();
    let mut req = agent.post(&url);
    req = match api_kind {
        config::ApiKind::Anthropic => req
            .set("x-api-key", &api_key)
            .set("anthropic-version", "2023-06-01"),
        config::ApiKind::OpenAi => req.set("Authorization", &format!("Bearer {api_key}")),
    };
    let resp = match req.send_json(body) {
        Ok(r) => r,
        Err(_) => return None,
    };
    let Ok(v) = serde_json::from_reader::<_, Value>(resp.into_reader()) else {
        return None;
    };
    let raw = match api_kind {
        config::ApiKind::Anthropic => v
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .map(str::to_string),
        config::ApiKind::OpenAi => v
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    let raw = raw?;
    let t = raw.trim().to_lowercase();
    Some(
        t.split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_string(),
    )
}

fn classify_verdict(
    log: &Path,
    outcome: &str,
    _api_kind: config::ApiKind,
    _base_url: &str,
    _api_key: &str,
    _model_id: &str,
) -> Option<String> {
    let (last_user, last_asst) = last_exchange(log)?;
    let u = last_user.chars().take(300).collect::<String>();
    let a = last_asst.chars().take(600).collect::<String>();

    let system = "You classify how a coding-agent turn ended. Reply with EXACTLY ONE word from this list: complete, waiting, retry, blocked, error.\n- complete: the task was finished and a result/summary was given.\n- waiting: the assistant ended by asking the user a question or explicitly needs more input from them.\n- retry: something failed and the user should retry or fix it.\n- blocked: a tool call, permission, or security policy blocked the action.\n- error: a tool or the model returned an error.\nNo other words, no quotes, no punctuation.";
    let user = format!(
        "Turn outcome hint: {outcome}\nLast user prompt: {u}\nLast assistant message: {a}\n\nVerdict:"
    );

    let first = call_light(system, &user)?;
    Some(clean_verdict(&first, outcome))
}

/// Decide (with the cheap light model) whether the active goal is actually
/// complete yet, independent of whatever status the big model last set. Used by
/// `drive_goal` as a backstop so a goal keeps running until it's genuinely done
/// (the big model sometimes stops with a summary but forgets to mark the goal
/// complete). Returns `Some(true)` when the light model judges the objective
/// met, `Some(false)` when not, and `None` when the light model is unavailable
/// or errors (callers then trust the big `update_goal` status).
///
/// Synchronous (blocks briefly on the light model) like [`classify_now`], so the
/// driver can act on the result immediately before deciding to loop again.
pub fn goal_complete_now(log: &Path, goal_summary: &str) -> Option<bool> {
    let (last_user, last_asst) = last_exchange(log)?;
    let u = last_user.chars().take(300).collect::<String>();
    let a = last_asst.chars().take(1000).collect::<String>();

    let system = "You judge whether a coding-agent's GOAL is complete. You are given the goal plan/status and the last assistant message. Reply with EXACTLY ONE word: 'complete' or 'incomplete'.\n- complete: every step is done and the objective is satisfied (or nothing remains to do).\n- incomplete: steps are undone, the objective isn't met, or the assistant is still mid-work / waiting.\nNo other words, no quotes, no punctuation.";
    let user = format!(
        "GOAL PLAN / STATUS:\n{goal_summary}\n\nLast user prompt: {u}\nLast assistant message: {a}\n\nIs the goal complete? (complete or incomplete):"
    );

    let first = call_light(system, &user)?;
    Some(first == "complete" || first.starts_with("complet"))
}

/// Normalize a light-model verdict into a canonical token, falling back to the
/// structural outcome when the model wanders. Always returns a token from
/// [`VERDICTS`] (or `complete`) — never an empty string.
fn clean_verdict(raw: &str, outcome: &str) -> String {
    let t = raw.trim().to_lowercase();
    // Take the first whitespace-delimited word (models sometimes add a period).
    let first = t.split_whitespace().next().unwrap_or("").trim_matches(|c: char| !c.is_alphanumeric());
    if VERDICTS.contains(&first) {
        return first.to_string();
    }
    // Fuzzy fallback by keyword.
    if first.contains("wait") {
        "waiting".to_string()
    } else if first.contains("retry") || first.contains("fail") {
        "retry".to_string()
    } else if first.contains("block") {
        "blocked".to_string()
    } else if first.contains("error") {
        "error".to_string()
    } else if outcome.starts_with("error") {
        "error".to_string()
    } else {
        "complete".to_string()
    }
}

/// Read (and normalize) a cached verdict for display, or an empty string if
/// none.
pub fn display_verdict(log: &Path) -> String {
    session::read_verdict(log).unwrap_or_default()
}

/// Extract the last [`MAX_PROMPTS`] user prompts from `log` (newest last), for
/// feeding to the light model. Returns an empty vec when the log can't be read.
fn recent_prompts(log: &Path) -> Vec<String> {
    let Ok(f) = std::fs::File::open(log) else { return Vec::new() };
    let mut prompts: Vec<String> = Vec::new();
    for line in std::io::BufReader::new(f).lines().flatten() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if v.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        if let Some(blocks) = v.get("blocks").and_then(Value::as_array) {
            let text: String = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();
            if !text.is_empty() {
                // Collapse to a single line for the prompt (the model only needs
                // the gist), and truncate so the request stays tiny.
                let one = text
                    .lines()
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                prompts.push(one.chars().take(400).collect());
            }
        }
    }
    if prompts.len() > MAX_PROMPTS {
        prompts
            .into_iter()
            .rev()
            .take(MAX_PROMPTS)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    } else {
        prompts
    }
}

/// Ask the light model for a short title of the conversation in `log`. Returns
/// `None` on any failure (network, parse, empty response) — callers treat
/// `None` as "try again later / give up", never as a hard error.
fn generate_title(
    log: &Path,
    api_kind: config::ApiKind,
    base_url: &str,
    api_key: &str,
    model_id: &str,
) -> Option<String> {
    let prompts = recent_prompts(log);
    if prompts.is_empty() {
        return None;
    }
    let joined = prompts
        .iter()
        .enumerate()
        .map(|(i, p)| format!("{}. {}", i + 1, p))
        .collect::<Vec<_>>()
        .join("\n");

    // Cheap, instruction-tuned prompt. We ask for a SHORT title only — no
    // preamble, so parsing is trivial and the token cost minimal.
    let system = "You name coding-agent conversations. Reply with ONE short title of at most 6 words that captures what the user is working on. No quotes, no 'Title:', no trailing punctuation. Examples: 'Fix parser crash on empty input', 'Add retry to upload tool', 'Refactor session picker'.";
    let user = format!("Recent prompts in this conversation:\n{joined}\n\nTitle:");

    let body = json!({
        "model": model_id,
        "max_tokens": 24,
        "stream": false,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user },
        ],
    });

    let url = match api_kind {
        config::ApiKind::Anthropic => format!("{}/messages", base_url.trim_end_matches('/')),
        config::ApiKind::OpenAi => {
            let mut u = base_url.trim_end_matches('/').to_string();
            if !u.ends_with("/chat/completions") && !u.contains('?') {
                u.push_str("/chat/completions");
            }
            u
        }
    };

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(30))
        .timeout_write(Duration::from_secs(15))
        .build();
    let mut req = agent.post(&url);
    req = match api_kind {
        config::ApiKind::Anthropic => req
            .set("x-api-key", api_key)
            .set("anthropic-version", "2023-06-01"),
        config::ApiKind::OpenAi => req.set("Authorization", &format!("Bearer {api_key}")),
    };
    // The light model is fire-and-forget; a single attempt is enough. If it
    // fails (rate-limited, offline, etc.) we just don't get a title this time.
    let resp = match req.send_json(body) {
        Ok(r) => r,
        Err(_) => return None,
    };
    let Ok(v) = serde_json::from_reader::<_, Value>(resp.into_reader()) else {
        return None;
    };

    let raw = match api_kind {
        config::ApiKind::Anthropic => v
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .map(str::to_string),
        config::ApiKind::OpenAi => v
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    let raw = raw?;
    let title = clean_title(&raw);
    if title.is_empty() { None } else { Some(title) }
}

/// Normalize a light-model title: strip quotes / a leading "Title:" label /
/// trailing punctuation, collapse whitespace, and cap the length. Returns an
/// empty string when nothing usable remains.
fn clean_title(raw: &str) -> String {
    let mut t = raw.trim().to_string();

    // Drop a leading "Title:" / "Name:" label the model sometimes prepends.
    if let Some(colon) = t.find(':') {
        let pre = t[..colon].trim().to_lowercase();
        if pre.is_empty() || pre == "title" || pre == "name" {
            t = t[colon + 1..].trim().to_string();
        }
    }

    // Strip wrapping quotes (", ', `) and surrounding whitespace.
    t = t
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim_matches('`')
        .trim()
        .to_string();

    // Trim a trailing period / colon.
    while t.ends_with('.') || t.ends_with(':') {
        t.pop();
    }
    t = t.trim().to_string();

    // Collapse internal whitespace to single spaces.
    let mut out = String::new();
    let mut prev_space = false;
    for c in t.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out = out.trim().to_string();

    // Guard against a chatty model: cap to ~60 chars.
    if out.chars().count() > 60 {
        out = out.chars().take(60).collect::<String>().trim_end().to_string();
    }
    out
}

/// Read (and normalize) a cached title for display, or an empty string if none.
pub fn display_title(log: &Path) -> String {
    session::read_title(log).unwrap_or_default()
}

/// Expose the most-recent light-model call time (epoch seconds), for tests and
/// self-checks. Returns the shared token file's mtime, or 0 when no call has
/// been made yet (the token file doesn't exist).
#[allow(dead_code)]
pub fn last_call_epoch() -> u64 {
    let token_path = std::env::current_dir()
        .ok()
        .map(|p| p.join(".pir_titler_throttle"))
        .unwrap_or_else(|| PathBuf::from(".pir_titler_throttle"));
    match std::fs::metadata(&token_path).and_then(|m| m.modified()) {
        Ok(t) => t
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        Err(_) => 0,
    }
}
