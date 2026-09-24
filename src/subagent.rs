//! Sub-agent delegation (Feature 6).
//!
//! The `agent` tool spawns a *child* agent — its own conversation, its own
//! tool loop — and returns only its final report. The parent's context stays
//! clean: an exploration that needs twenty file reads costs the parent one tool
//! result instead of twenty tool round-trips (and none of the bytes).
//!
//! # Types
//!
//! | type      | posture                          | intended for                          |
//! |-----------|----------------------------------|---------------------------------------|
//! | `explore` | read-only (plan mode)            | finding things, answering questions   |
//! | `plan`    | read-only, writes a plan         | proposing an approach                 |
//! | `general` | inherits the parent's posture    | doing the work                        |
//!
//! The child is *not* a separate process: it is another `Agent` in this one,
//! constructed with the parent's provider/model and the same security policy.
//! That keeps the security boundary intact (a child cannot exceed the parent's
//! level — `explore`/`plan` are strictly weaker, `general` is identical) while
//! avoiding the cost and the failure modes of spawning a whole `pir`.
//!
//! # Runaway protection
//!
//! A sub-agent cannot ask the operator anything (there is no channel to a
//! second renderer from inside a turn), so `ask_user_question` and
//! `request_stop` are removed from its tool list, and its turn is bounded by
//! the same loop detector, token budget and wall-clock guard as a normal turn.
//! The wall clock is the important one: a child that wedges must not hold the
//! parent forever, so the caller runs it on a thread with a deadline and
//! reports a timeout as an ordinary tool result.

use crate::agent::Agent;
use crate::config::{Model, Provider};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The kind of sub-agent to spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Read-only reconnaissance: find the code, answer the question, report.
    Explore,
    /// Read-only planning: produce an approach (no changes).
    Plan,
    /// Full capability, inheriting the parent's security posture.
    General,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "explore" | "explorer" | "search" | "read" | "readonly" | "read-only" => Some(Kind::Explore),
            "plan" | "planner" | "planning" => Some(Kind::Plan),
            "general" | "general-purpose" | "default" | "work" | "code" => Some(Kind::General),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Explore => "explore",
            Kind::Plan => "plan",
            Kind::General => "general",
        }
    }
    /// Extra system-prompt instruction that shapes the child's behaviour.
    pub fn preamble(self) -> &'static str {
        match self {
            Kind::Explore => {
                "\n# Sub-agent: EXPLORE (read-only)\n\n\
                 You are a reconnaissance sub-agent. Your job is to find the answer to the \
                 question you were given and report it. You are READ-ONLY: write_file, edit_file \
                 and bash are refused by the security policy, and asking it to change anything is \
                 not your job. Use glob, grep, read_file and list_dir freely. Finish with a \
                 compact report: the answer, the exact file paths and line numbers that support \
                 it, and anything important you could NOT determine. Do not pad the report with \
                 the code you read — the caller can read the files itself.\n"
            }
            Kind::Plan => {
                "\n# Sub-agent: PLAN (read-only)\n\n\
                 You are a planning sub-agent. Study the code read-only and produce a concrete, \
                 ordered implementation plan: the goal, the approach, the exact files to change, \
                 the risks, and how the change would be verified. You are READ-ONLY — do not \
                 write code, do not modify files, do not run commands. Record the plan with the \
                 plan_write tool (it is allowed) and finish with a short summary for the caller.\n"
            }
            Kind::General => {
                "\n# Sub-agent: GENERAL\n\n\
                 You are a delegated sub-agent working under the same rules as your caller. Do \
                 the task you were given, to completion, and report back: what you changed, the \
                 files involved, and anything the caller must know. Do not ask questions — there \
                 is no operator attached to you; if something is genuinely ambiguous, state the \
                 assumption you made.\n"
            }
        }
    }
}

/// A request to run a sub-agent.
#[derive(Debug, Clone)]
pub struct Spawn {
    pub kind: Kind,
    pub description: String,
    pub prompt: String,
    pub background: bool,
    /// Optional provider/model selector for this child (e.g.
    /// `cerebras/gpt-oss-120b`). `None` means "inherit the parent's model".
    /// Only honoured for read-only kinds — see [`resolve_child_model`].
    pub model: Option<String>,
}

/// Parse the `agent` tool input.
pub fn parse(input: &serde_json::Value) -> Result<Spawn, String> {
    let prompt = input
        .get("prompt")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or("agent: 'prompt' is required (the task for the sub-agent)")?;
    let kind_raw = input
        .get("subagent_type")
        .or_else(|| input.get("subagentType"))
        .and_then(|v| v.as_str())
        .unwrap_or("general");
    let kind = Kind::parse(kind_raw).ok_or_else(|| {
        format!("agent: unknown subagent_type '{kind_raw}' (want explore, plan or general)")
    })?;
    let description = input
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let background = input
        .get("run_in_background")
        .or_else(|| input.get("runInBackground"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let model = input
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok(Spawn { kind, description, prompt, background, model })
}

/// A request to fan several sub-agents out at once.
#[derive(Debug, Clone)]
pub struct Batch {
    pub tasks: Vec<Spawn>,
}

/// Parse a batched `agent` call (`{"tasks": [...]}`). Each task is parsed with
/// the same rules as a single call, so `prompt` is still required per task.
pub fn parse_batch(input: &serde_json::Value) -> Result<Batch, String> {
    let arr = input
        .get("tasks")
        .and_then(|v| v.as_array())
        .ok_or("agent: 'tasks' must be a non-empty array of task objects")?;
    if arr.is_empty() {
        return Err("agent: 'tasks' must not be empty".into());
    }
    if arr.len() > MAX_BATCH {
        return Err(format!(
            "agent: too many tasks ({}); the limit is {MAX_BATCH} — split the work into smaller batches",
            arr.len()
        ));
    }
    let mut tasks = Vec::with_capacity(arr.len());
    for (i, t) in arr.iter().enumerate() {
        let spawn = parse(t).map_err(|e| format!("agent: task #{}: {e}", i + 1))?;
        tasks.push(spawn);
    }
    Ok(Batch { tasks })
}

/// Upper bound on one batched fan-out. Each child is a real agent with its own
/// provider calls, so an unbounded list is a way to accidentally open fifty
/// concurrent requests; 8 is enough for the usual "sweep these areas" pattern
/// without hammering a provider's rate limit.
pub const MAX_BATCH: usize = 8;

/// How long a sub-agent may run before the caller gives up on it. Overridable
/// so tests and scripted runs stay fast.
pub fn timeout() -> Duration {
    let secs = std::env::var("PIR_SUBAGENT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(10 * 60);
    Duration::from_secs(secs)
}

/// The outcome of a delegated run.
pub struct Report {
    pub text: String,
    pub elapsed: Duration,
    pub timed_out: bool,
    /// True when the child ended on an error (provider failure, bad config).
    pub error: bool,
}

impl Report {
    /// Render the report as the tool result the parent model sees.
    pub fn render(&self, spawn: &Spawn) -> String {
        let head = if spawn.description.is_empty() {
            format!("sub-agent {} finished", spawn.kind.as_str())
        } else {
            format!("sub-agent {} ({}) finished", spawn.kind.as_str(), spawn.description)
        };
        let mut out = format!(
            "{head} in {:.1}s{}.\n---\n",
            self.elapsed.as_secs_f32(),
            if self.error { " (with errors)" } else { "" }
        );
        if self.timed_out {
            out.push_str(&format!(
                "TIMED OUT after {:.0}s — the report below is partial. The sub-agent was stopped; \
                 treat anything it did as incomplete.\n---\n",
                self.elapsed.as_secs_f32()
            ));
        }
        if self.text.trim().is_empty() {
            out.push_str("(the sub-agent produced no output)");
        } else {
            out.push_str(self.text.trim());
        }
        crate::plugin::truncate(&mut out, 30_000);
        out
    }
}

/// Run a sub-agent to completion (or the timeout) and return its report.
///
/// The child is built on its own thread so that (a) a wedged child can be
/// abandoned at the deadline instead of hanging the parent's turn, and (b) its
/// `Agent` never contends for the parent's slot/locks.
pub fn run(
    provider: Provider,
    model: Model,
    spawn: &Spawn,
    parent_full_auto: bool,
    policy: Option<crate::security::SecurityPolicy>,
    cancel: Arc<AtomicBool>,
) -> Report {
    let started = Instant::now();
    let (tx, rx) = mpsc::channel::<Result<String, String>>();
    let spawn = spawn.clone();
    std::thread::spawn(move || {
        let res = run_inner(&provider, &model, &spawn, parent_full_auto, policy, cancel);
        let _ = tx.send(res);
    });
    let budget = timeout();
    match rx.recv_timeout(budget) {
        Ok(Ok(text)) => Report { text, elapsed: started.elapsed(), timed_out: false, error: false },
        Ok(Err(e)) => Report { text: e, elapsed: started.elapsed(), timed_out: false, error: true },
        Err(_) => Report {
            text: String::new(),
            elapsed: started.elapsed(),
            timed_out: true,
            error: false,
        },
    }
}

/// Resolve the `(provider, model)` a child should use.
///
/// * `spawn.model` is `Some(selector)` **and** the kind is read-only
///   (`explore`/`plan`): resolve the selector against the catalog. The
///   selector is only a *hint* — read-only children run with a clamped policy,
///   so running one on a different model cannot widen the security boundary.
/// * `spawn.model` is `Some` for `general`: **ignored**, with a note, because a
///   `general` child inherits the parent's posture verbatim and a weaker model
///   there can silently produce a wrong write. The parent's model is used.
/// * `None`: inherit the parent's model.
///
/// Returns the resolved pair plus an optional advisory note for the report (so
/// a caller that passed `model` on a `general` task is told why it was ignored
/// rather than silently overridden).
pub fn resolve_child_model(
    providers: &[Provider],
    parent: (&Provider, &Model),
    spawn: &Spawn,
) -> (Provider, Model, Option<String>) {
    let Some(sel) = spawn.model.as_deref() else {
        return (parent.0.clone(), parent.1.clone(), None);
    };
    if spawn.kind == Kind::General {
        return (
            parent.0.clone(),
            parent.1.clone(),
            Some(format!(
                "note: `model` ('{sel}') was ignored for a general sub-agent — general inherits \
                 the parent's model and posture, because a weaker model there can silently get \
                 the work wrong. Use `explore` to run a cheaper model."
            )),
        );
    }
    match crate::config::select(providers, sel) {
        Ok((p, m)) => (p.clone(), m.clone(), None),
        Err(e) => (
            parent.0.clone(),
            parent.1.clone(),
            Some(format!(
                "note: `model` ('{sel}') could not be resolved ({e}); using the parent's model."
            )),
        ),
    }
}

/// Run several sub-agents **concurrently** and return one report per task, in
/// submission order.
///
/// This is the fan-out path: each child runs on its own thread (the same
/// mechanism as [`run`]) but they overlap instead of being serialized. The
/// merged-report rule is what keeps this safe:
///
/// * Every child is constructed *quiet* (`Agent::new_subagent(..., quiet=true)`),
///   so no child ever writes to the parent's terminal — concurrently-finishing
///   children cannot interleave on screen.
/// * Each child writes only to **its own** session log file, so there is no
///   shared output buffer to race on.
/// * The batch driver is the single writer of the report: it joins the results
///   in submission order and renders them as one `Outcome`. Result order is
///   therefore deterministic even though completion order is not.
///
/// Each child keeps its own wall-clock deadline: a wedged child is abandoned
/// with a partial/empty report rather than delaying the whole batch.
pub struct BatchOutcome {
    pub reports: Vec<Report>,
    /// Advisory notes (e.g. an ignored `model` on a general task), one per task
    /// where relevant, keyed by task index.
    pub notes: Vec<(usize, String)>,
    pub elapsed: Duration,
}

pub fn run_batch(
    parent: (&Provider, &Model),
    providers: &[Provider],
    batch: &Batch,
    parent_full_auto: bool,
    policy: Option<crate::security::SecurityPolicy>,
    cancel: Arc<AtomicBool>,
) -> BatchOutcome {
    let started = Instant::now();
    let budget = timeout();
    let mut notes: Vec<(usize, String)> = Vec::new();
    // Channel carrying (index, result) so we can restore submission order even
    // though threads finish in arbitrary order.
    let (tx, rx) = mpsc::channel::<(usize, Result<String, String>, Duration)>();
    let mut expected = 0usize;
    for (i, spawn) in batch.tasks.iter().enumerate() {
        let (provider, model, note) = resolve_child_model(providers, parent, spawn);
        if let Some(n) = note {
            notes.push((i, n));
        }
        let spawn = spawn.clone();
        let cancel = cancel.clone();
        let policy = policy.clone();
        let tx = tx.clone();
        expected += 1;
        std::thread::spawn(move || {
            // Time each child individually so the parent can see which task was
            // slow, rather than one batch-wide number for all of them.
            let child_started = Instant::now();
            let res = run_inner(&provider, &model, &spawn, parent_full_auto, policy, cancel);
            let _ = tx.send((i, res, child_started.elapsed()));
        });
    }
    // Drop our own sender so `recv` reports disconnection once every child has
    // sent (instead of blocking forever on a live sender with no children).
    drop(tx);

    let mut slots: Vec<Option<Report>> = (0..batch.tasks.len()).map(|_| None).collect();
    let mut received = 0usize;
    let deadline = started + budget;
    while received < expected {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok((i, res, child_elapsed)) => {
                received += 1;
                let report = match res {
                    Ok(text) => Report {
                        text,
                        elapsed: child_elapsed,
                        timed_out: false,
                        error: false,
                    },
                    Err(e) => Report {
                        text: e,
                        elapsed: child_elapsed,
                        timed_out: false,
                        error: true,
                    },
                };
                slots[i] = Some(report);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // Any task that never reported is a timeout for that child specifically —
    // the batch as a whole is not failed, so the children that did finish are
    // still useful.
    let reports = slots
        .into_iter()
        .map(|slot| {
            slot.unwrap_or(Report {
                text: String::new(),
                elapsed: started.elapsed(),
                timed_out: true,
                error: false,
            })
        })
        .collect();
    BatchOutcome { reports, notes, elapsed: started.elapsed() }
}

/// Render a [`BatchOutcome`] as the single tool result the parent sees.
pub fn render_batch(batch: &Batch, outcome: &BatchOutcome) -> String {
    let n = batch.tasks.len();
    let ok = outcome.reports.iter().filter(|r| !r.error && !r.timed_out).count();
    let timed_out = outcome.reports.iter().filter(|r| r.timed_out).count();
    let failed = outcome.reports.iter().filter(|r| r.error).count();
    let mut out = format!(
        "sub-agent batch finished: {n} task(s) in {:.1}s — {ok} ok, {failed} failed, {timed_out} timed out.\n\
         Reports are in submission order, not completion order.\n",
        outcome.elapsed.as_secs_f32()
    );
    for (i, (spawn, report)) in batch.tasks.iter().zip(&outcome.reports).enumerate() {
        out.push_str(&format!("\n=== task #{} ({}) ===\n", i + 1, spawn.kind.as_str()));
        if !spawn.description.is_empty() {
            out.push_str(&format!("{}\n", spawn.description));
        }
        if let Some((_, note)) = outcome.notes.iter().find(|(idx, _)| *idx == i) {
            out.push_str(&format!("{note}\n"));
        }
        if report.timed_out {
            out.push_str(&format!(
                "TIMED OUT after {:.0}s — no usable report. The sub-agent was stopped.\n",
                report.elapsed.as_secs_f32()
            ));
        } else if report.text.trim().is_empty() {
            out.push_str("(the sub-agent produced no output)\n");
        } else {
            out.push_str(report.text.trim());
            out.push('\n');
        }
    }
    crate::plugin::truncate(&mut out, 30_000);
    out
}

fn run_inner(
    provider: &Provider,
    model: &Model,
    spawn: &Spawn,
    parent_full_auto: bool,
    policy: Option<crate::security::SecurityPolicy>,
    cancel: Arc<AtomicBool>,
) -> Result<String, String> {
    let started = Instant::now();
    // The child is always quiet: its turn must not print to the parent's
    // terminal (the parent is the only writer while a turn runs) — its report
    // travels back through the tool result, and its transcript lands in its own
    // session log for later inspection.
    let mut child = Agent::new_subagent(
        provider.clone(),
        model.clone(),
        parent_full_auto,
        spawn.kind,
        policy,
        cancel,
    )?;
    let task = format!("{}{}", task_prompt(spawn), "");
    match child.turn(&task) {
        Ok(()) => {
            let text = child.last_reply_text();
            if text.trim().is_empty() {
                // A child that finished without writing a report still did work;
                // say what, so the delegation is not a total loss of
                // information (and the parent can see whether it was wedged,
                // cancelled, or just silent).
                Ok(format!(
                    "(the sub-agent finished without a written report{})",
                    activity_note(&child)
                ))
            } else {
                Ok(text)
            }
        }
        Err(e) => Err(format!("sub-agent failed: {e} (turn ran for {:.1}s)", started.elapsed().as_secs_f32())),
    }
}

/// A short "what did it do" note from the child's history, used when it
/// finished without a written report.
fn activity_note(child: &Agent) -> String {
    let tools = child.tool_call_count();
    if tools == 0 {
        return " — it made no tool calls".to_string();
    }
    format!(
        " — it made {tools} tool call(s) before ending; the transcript is in {}",
        child
            .log_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "no session log".to_string())
    )
}

fn task_prompt(spawn: &Spawn) -> String {
    let mut s = String::new();
    if !spawn.description.is_empty() {
        s.push_str(&format!("Task: {}\n\n", spawn.description));
    }
    s.push_str(&spawn.prompt);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kind_parses_aliases_and_rejects_junk() {
        assert_eq!(Kind::parse("explore"), Some(Kind::Explore));
        assert_eq!(Kind::parse("read-only"), Some(Kind::Explore));
        assert_eq!(Kind::parse("PLAN"), Some(Kind::Plan));
        assert_eq!(Kind::parse("general"), Some(Kind::General));
        assert_eq!(Kind::parse("default"), Some(Kind::General));
        assert_eq!(Kind::parse("wizard"), None);
        // Every kind has a distinct label and a preamble that states its posture.
        assert_eq!(Kind::Explore.as_str(), "explore");
        assert!(Kind::Explore.preamble().contains("READ-ONLY"));
        assert!(Kind::Plan.preamble().contains("READ-ONLY"));
        assert!(Kind::General.preamble().contains("same rules"));
    }

    #[test]
    fn parse_requires_a_prompt_and_validates_the_type() {
        let e = parse(&json!({})).unwrap_err();
        assert!(e.contains("'prompt' is required"), "got: {e}");
        let e = parse(&json!({"prompt": "   "})).unwrap_err();
        assert!(e.contains("'prompt' is required"), "got: {e}");
        let e = parse(&json!({"prompt": "x", "subagent_type": "wizard"})).unwrap_err();
        assert!(e.contains("unknown subagent_type"), "got: {e}");
        // Defaults: general, foreground, no description.
        let s = parse(&json!({"prompt": "find the parser"})).unwrap();
        assert_eq!(s.kind, Kind::General);
        assert!(!s.background);
        assert!(s.description.is_empty());
        // The camelCase alias is accepted too.
        let s = parse(&json!({"prompt": "x", "subagentType": "explore", "runInBackground": true})).unwrap();
        assert_eq!(s.kind, Kind::Explore);
        assert!(s.background);
    }

    #[test]
    fn report_renders_head_body_timeout_and_error() {
        let s = Spawn {
            kind: Kind::Explore,
            description: "auth module".into(),
            prompt: "find it".into(),
            background: false,
            model: None,
        };
        let r = Report {
            text: "found it in src/auth.rs:12".into(),
            elapsed: Duration::from_millis(1500),
            timed_out: false,
            error: false,
        };
        let out = r.render(&s);
        assert!(out.contains("sub-agent explore (auth module) finished"), "got: {out}");
        assert!(out.contains("1.5s"), "must report elapsed time: {out}");
        assert!(out.contains("src/auth.rs:12"));
        assert!(!out.contains("TIMED OUT"));

        let timed = Report { timed_out: true, text: "partial".into(), ..r };
        let out = timed.render(&s);
        assert!(out.contains("TIMED OUT"), "got: {out}");
        assert!(out.contains("partial"));

        let empty = Report { text: "  ".into(), ..r };
        assert!(empty.render(&s).contains("no output"));
    }

    #[test]
    fn task_prompt_prepends_the_description_only_when_present() {
        let mut s = Spawn {
            kind: Kind::General,
            description: String::new(),
            prompt: "do the thing".into(),
            background: false,
            model: None,
        };
        assert_eq!(task_prompt(&s), "do the thing");
        s.description = "retry logic".into();
        let t = task_prompt(&s);
        assert!(t.starts_with("Task: retry logic"), "got: {t}");
        assert!(t.ends_with("do the thing"));
    }

    #[test]
    fn timeout_is_configurable_and_positive() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("PIR_SUBAGENT_TIMEOUT_SECS");
        // SAFETY: test-local env mutation under the shared env lock.
        unsafe { std::env::set_var("PIR_SUBAGENT_TIMEOUT_SECS", "7"); }
        assert_eq!(timeout(), Duration::from_secs(7));
        unsafe { std::env::set_var("PIR_SUBAGENT_TIMEOUT_SECS", "0"); }
        assert_eq!(timeout(), Duration::from_secs(600), "0 is rejected, not a zero budget");
        match old {
            Some(v) => unsafe { std::env::set_var("PIR_SUBAGENT_TIMEOUT_SECS", v) },
            None => unsafe { std::env::remove_var("PIR_SUBAGENT_TIMEOUT_SECS") },
        }
    }

    /// A batch parses every task with the single-task rules, rejects an empty
    /// list, and caps the count (each child is real provider traffic).
    #[test]
    fn batch_parses_and_validates_each_task() {
        let b = parse_batch(&json!({
            "tasks": [
                {"prompt": "find auth", "subagent_type": "explore", "description": "auth"},
                {"prompt": "find migrations"}
            ]
        }))
        .expect("batch parses");
        assert_eq!(b.tasks.len(), 2);
        assert_eq!(b.tasks[0].kind, Kind::Explore);
        assert_eq!(b.tasks[0].description, "auth");
        assert_eq!(b.tasks[1].kind, Kind::General, "kind defaults per task");

        let e = parse_batch(&json!({"tasks": []})).unwrap_err();
        assert!(e.contains("must not be empty"), "got: {e}");
        let e = parse_batch(&json!({"prompt": "x"})).unwrap_err();
        assert!(e.contains("'tasks' must be a non-empty array"), "got: {e}");
        // A bad task names its index so the model can fix the right one.
        let e = parse_batch(&json!({"tasks": [{"prompt": "ok"}, {"subagent_type": "explore"}]}))
            .unwrap_err();
        assert!(e.contains("task #2"), "got: {e}");
        assert!(e.contains("'prompt' is required"), "got: {e}");
        // Too many tasks is refused rather than silently truncated.
        let many: Vec<serde_json::Value> =
            (0..MAX_BATCH + 1).map(|i| json!({"prompt": format!("t{i}")})).collect();
        let e = parse_batch(&json!({"tasks": many})).unwrap_err();
        assert!(e.contains("too many tasks"), "got: {e}");
    }

    /// `model` is parsed and defaults to `None` (inherit the parent's).
    #[test]
    fn spawn_parses_model_selector() {
        let s = parse(&json!({"prompt": "x"})).unwrap();
        assert_eq!(s.model, None, "no model means inherit");
        let s = parse(&json!({"prompt": "x", "model": "cerebras/gpt-oss-120b"})).unwrap();
        assert_eq!(s.model.as_deref(), Some("cerebras/gpt-oss-120b"));
        // Whitespace-only is treated as absent, not as a broken selector.
        let s = parse(&json!({"prompt": "x", "model": "   "})).unwrap();
        assert_eq!(s.model, None);
    }

    fn provider(id: &str, model: &str) -> Provider {
        serde_json::from_str(&format!(
            r#"{{"id":"{id}","baseUrl":"https://example.invalid/v1","apiKey":"x","api":"openai","models":[{{"id":"{model}"}}]}}"#
        ))
        .unwrap()
    }

    /// Read-only children may be pointed at another model; `general` may not.
    /// This is the restriction the doc-comment promises, asserted directly.
    #[test]
    fn resolve_child_model_honours_only_readonly_kinds() {
        let big = provider("big", "big-model");
        let small = provider("cerebras", "gpt-oss-120b");
        let providers = vec![big.clone(), small.clone()];
        let parent = (&big, &big.models[0]);

        // No selector: inherit the parent exactly.
        let s = Spawn {
            kind: Kind::Explore,
            description: String::new(),
            prompt: "x".into(),
            background: false,
            model: None,
        };
        let (p, m, note) = resolve_child_model(&providers, parent, &s);
        assert_eq!(p.pid(), big.pid());
        assert_eq!(m.id, "big-model");
        assert!(note.is_none());

        // explore + selector: honoured, no note.
        let s = Spawn { model: Some("cerebras/gpt-oss-120b".into()), ..s.clone() };
        let (p, m, note) = resolve_child_model(&providers, parent, &s);
        assert_eq!(p.pid(), small.pid(), "explore must be allowed a cheaper model");
        assert_eq!(m.id, "gpt-oss-120b");
        assert!(note.is_none(), "a honoured selector needs no note");

        // plan + selector: honoured too (still read-only).
        let s = Spawn { kind: Kind::Plan, ..s.clone() };
        let (p, m, _) = resolve_child_model(&providers, parent, &s);
        assert_eq!(p.pid(), small.pid());
        assert_eq!(m.id, "gpt-oss-120b");

        // general + selector: IGNORED, with a note explaining why. This is the
        // safety property: a general child inherits the parent's model so a
        // weaker model can never silently do the writing.
        let s = Spawn { kind: Kind::General, ..s.clone() };
        let (p, m, note) = resolve_child_model(&providers, parent, &s);
        assert_eq!(p.pid(), big.pid(), "general must keep the parent's model");
        assert_eq!(m.id, "big-model");
        let note = note.expect("general must be told its model was ignored");
        assert!(note.contains("ignored"), "got: {note}");
        assert!(note.contains("general"), "got: {note}");

        // An unresolvable selector falls back to the parent, with a note.
        let s = Spawn { kind: Kind::Explore, model: Some("nope/nothing".into()), ..s.clone() };
        let (p, m, note) = resolve_child_model(&providers, parent, &s);
        assert_eq!(p.pid(), big.pid(), "unresolvable selector must fall back");
        assert_eq!(m.id, "big-model");
        assert!(note.expect("fallback needs a note").contains("could not be resolved"));
    }

    /// The merge format: one report per task, in submission order, with a
    /// header and a per-task section — even when completion order differs.
    #[test]
    fn render_batch_is_submission_ordered_with_headers() {
        let tasks = vec![
            Spawn {
                kind: Kind::Explore,
                description: "auth".into(),
                prompt: "find auth".into(),
                background: false,
                model: None,
            },
            Spawn {
                kind: Kind::Plan,
                description: String::new(),
                prompt: "plan it".into(),
                background: false,
                model: None,
            },
        ];
        let batch = Batch { tasks: tasks.clone() };
        // Deliberately build the outcome as if task #2 finished first: the
        // renderer must still emit #1 then #2.
        let outcome = BatchOutcome {
            reports: vec![
                Report { text: "first-report".into(), elapsed: Duration::from_millis(900), timed_out: false, error: false },
                Report { text: "second-report".into(), elapsed: Duration::from_millis(300), timed_out: false, error: false },
            ],
            notes: vec![(1, "note: model ignored".to_string())],
            elapsed: Duration::from_millis(1200),
        };
        let out = render_batch(&batch, &outcome);
        let i1 = out.find("first-report").expect("task 1 report present");
        let i2 = out.find("second-report").expect("task 2 report present");
        assert!(i1 < i2, "reports must be in submission order, got: {out}");
        assert!(out.contains("=== task #1 (explore) ==="), "got: {out}");
        assert!(out.contains("=== task #2 (plan) ==="), "got: {out}");
        assert!(out.contains("auth"), "description must appear: {out}");
        assert!(out.contains("note: model ignored"), "notes must appear: {out}");
        assert!(out.contains("2 ok"), "summary counts: {out}");
        assert!(out.contains("submission order"), "must state the ordering: {out}");
    }

    /// A task that never reported is marked timed out *for that task only* —
    /// the batch as a whole is not failed, so the good reports survive.
    #[test]
    fn render_batch_marks_a_lone_timeout_without_failing_the_batch() {
        let tasks = vec![Spawn {
            kind: Kind::Explore,
            description: "wedged".into(),
            prompt: "hang".into(),
            background: false,
            model: None,
        }];
        let batch = Batch { tasks };
        let outcome = BatchOutcome {
            reports: vec![Report {
                text: String::new(),
                elapsed: Duration::from_secs(600),
                timed_out: true,
                error: false,
            }],
            notes: Vec::new(),
            elapsed: Duration::from_secs(600),
        };
        let out = render_batch(&batch, &outcome);
        assert!(out.contains("1 timed out"), "got: {out}");
        assert!(out.contains("TIMED OUT"), "got: {out}");
        assert!(out.contains("no usable report"), "got: {out}");
    }
}
