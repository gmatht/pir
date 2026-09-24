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
    Ok(Spawn { kind, description, prompt, background })
}

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
}
