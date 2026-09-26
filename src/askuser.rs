//! Interactive questions from the agent to the operator (Feature 3).
//!
//! The `ask_user_question` tool lets the model *pause* a turn and ask a
//! structured question — a header, a body, and a list of options — instead of
//! guessing or stopping with a free-text request. The answer is returned as the
//! tool result, so the model resumes with the operator's decision in context.
//!
//! # Threading
//!
//! A turn runs on a worker thread (`run_foreground_turn`), while the REPL owns
//! stdin on the main thread. The worker therefore cannot read the keyboard
//! itself. The bridge is a pair of `std::sync::mpsc` channels plus a stored
//! renderer:
//!
//! ```text
//! worker thread                        REPL / main thread
//! ─────────────                        ──────────────────
//! askuser::ask(q) ── req_tx.send(q) ─► renderer(q)  (modal or line prompt)
//!      blocks on resp_rx.recv()        ◄── resp_tx.send(answer)
//! ```
//!
//! The renderer is installed once at startup with [`install`]. When nothing is
//! installed (headless `-bg`, a piped one-shot run, tests) [`ask`] returns
//! `None` immediately — the tool then reports "no interactive user" instead of
//! hanging forever, which is the failure mode that matters most: an unattended
//! agent must never block on a question nobody can answer.
//!
//! # Timeout and abandonment
//!
//! A renderer that *is* installed may still never produce an answer — the
//! operator walked away, or never saw the dialog. [`ask`] bounds the wait with
//! [`timeout`] and then returns [`Answer::TimedOut`], which is deliberately
//! distinct from [`Answer::Dismissed`] (a real "no" from a real operator).
//!
//! Crucially, timing out must not leave the dialog on screen: a modal owns the
//! alternate screen and the keyboard, so an abandoned one would both deny the
//! operator the chance to answer and fight the REPL for the terminal. [`ask`]
//! therefore sets a shared abort flag that the modal polls (`modal::abort`),
//! and waits briefly for the renderer to tear itself down before returning.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// One question, as the model asked it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// Short label for the dialog title (e.g. "Approach").
    pub header: String,
    /// The question itself (may be multi-line).
    pub question: String,
    /// Selectable answers. Empty means free-text.
    pub options: Vec<Choice>,
    /// Whether more than one option may be selected.
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub label: String,
    pub description: String,
}

/// What the operator answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// One (single-select) or more (multi-select) chosen option labels.
    Chosen(Vec<String>),
    /// Free text typed by the operator.
    Text(String),
    /// The operator dismissed the question without answering (pressed Esc /
    /// ctrl-c on a dialog, or declined a line prompt).
    Dismissed,
    /// Nobody answered within [`timeout`] — the question was *never seen* (or
    /// the operator walked away), so the dialog was abandoned and torn down.
    ///
    /// Distinct from [`Answer::Dismissed`] on purpose: "the operator decided
    /// against this" and "the operator never got a chance to decide" call for
    /// completely different behaviour from the model (proceed with a stated
    /// assumption vs. treat it as an explicit refusal).
    TimedOut,
}

impl Answer {
    /// Render the answer for the tool result the model sees.
    pub fn render(&self) -> String {
        match self {
            Answer::Chosen(v) if v.is_empty() => "(no option selected)".to_string(),
            Answer::Chosen(v) => v.join(", "),
            Answer::Text(t) if t.trim().is_empty() => "(empty answer)".to_string(),
            Answer::Text(t) => t.clone(),
            Answer::Dismissed => "(dismissed — no answer given)".to_string(),
            Answer::TimedOut => {
                "(no answer — the operator did not respond before the timeout)".to_string()
            }
        }
    }
}

/// The function that actually presents a question. Implemented by the REPL
/// (modal picker, falling back to a line prompt) and by tests. Held behind an
/// `Arc` so [`ask`] can clone it out under the lock and then run it *without*
/// holding the lock — a blocking modal must never pin the registry (or the
/// operator's answer the next question through a lock the first one still
/// holds).
pub type Renderer = std::sync::Arc<dyn Fn(&Question) -> Answer + Send + Sync>;

static RENDERER: OnceLock<Mutex<std::option::Option<Renderer>>> = OnceLock::new();

fn slot() -> &'static Mutex<std::option::Option<Renderer>> {
    RENDERER.get_or_init(|| Mutex::new(None))
}

/// Install the REPL's question renderer. Called once at startup, before the
/// first prompt; a later call replaces the previous renderer (used by tests).
pub fn install(r: Renderer) {
    *slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
}

/// Remove the renderer (e.g. when a REPL session ends).
pub fn uninstall() {
    *slot().lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// The currently installed renderer, cloned out so the caller can invoke it
/// without holding the registry lock.
fn renderer() -> std::option::Option<Renderer> {
    slot().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Whether a renderer is installed *and* the terminal can host a dialog. The
/// agent checks this before running the tool so an unattended session gets a
/// fast, honest "no interactive user" instead of a timeout.
pub fn interactive() -> bool {
    slot().lock().map(|g| g.is_some()).unwrap_or(false)
}

/// How long [`ask`] waits for the operator before giving up. A question the
/// operator walks away from must not wedge an agent turn for hours; two minutes
/// is long enough to read a long question and answer it, and the timeout is
/// reported as an explicit [`Answer::TimedOut`] rather than silently pretending
/// the user said nothing (or, worse, picking a default the operator never chose).
pub fn timeout() -> Duration {
    let secs = std::env::var("PIR_ASK_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(120);
    Duration::from_secs(secs)
}

/// Ask the operator `q`. Blocks until they answer or [`timeout`] elapses.
///
/// Returns `None` when there is no interactive operator (no renderer installed).
/// When the operator does not answer in time the result is
/// [`Answer::TimedOut`] — never a bare `None` — so the caller can tell "nobody
/// looked" apart from "the operator said no".
///
/// # Abandonment
///
/// The renderer runs on its own thread (a modal reads the tty, so it cannot run
/// on the tool loop). On timeout we must **not** just walk away: an abandoned
/// dialog is still on the alternate screen owning the keyboard, so the operator
/// would never get a chance to answer *and* the REPL and the modal would fight
/// over the terminal. We therefore:
///
/// 1. set the shared abort flag the modal polls (`modal::set_abort_flag`), so it
///    tears itself down and restores the terminal, then
/// 2. join the renderer thread briefly, so the caller resumes with the terminal
///    in a known state rather than racing a live modal.
///
/// If the renderer does not honour the flag within [`ABORT_GRACE`] we still
/// return `TimedOut` (never hang the agent) but say so through the distinct
/// variant, which is the honest report: the question was abandoned, not
/// answered.
pub fn ask(q: &Question) -> std::option::Option<Answer> {
    // Clone the renderer *out* of the registry and release the lock before
    // running it: a modal blocks for as long as the operator takes to answer,
    // and holding the lock that long would serialise every question (and poison
    // it if the renderer ever panicked).
    let renderer = renderer()?;
    // A channel pair carries the question to the renderer thread and the answer
    // back. The renderer runs on its own thread so a modal implementation (which
    // reads keys from the tty) never blocks the agent's tool loop directly.
    let (req_tx, req_rx) = mpsc::channel::<Question>();
    let (resp_tx, resp_rx) = mpsc::channel::<Answer>();
    // Cooperative abandonment: the renderer's dialog polls this so a timeout
    // closes it instead of leaving it on screen.
    let abort = Arc::new(AtomicBool::new(false));
    crate::modal::set_abort_flag(abort.clone());
    let question = q.clone();
    let handle = std::thread::spawn(move || {
        if req_rx.recv().is_err() {
            return;
        }
        let ans = renderer(&question);
        let _ = resp_tx.send(ans);
    });
    if req_tx.send(q.clone()).is_err() {
        let _ = handle.join();
        return None;
    }
    match resp_rx.recv_timeout(timeout()) {
        Ok(a) => {
            let _ = handle.join();
            crate::modal::clear_abort_flag();
            Some(a)
        }
        Err(_) => {
            // Nobody answered in time. Signal the dialog to close, then give it
            // a bounded moment to release the terminal so we don't hand control
            // back to the REPL while it still owns the alternate screen.
            abort.store(true, Ordering::SeqCst);
            let deadline = std::time::Instant::now() + ABORT_GRACE;
            while !handle.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            // Clear the flag *after* the renderer had its chance to observe it,
            // so a later unrelated modal can't inherit this question's abort.
            crate::modal::clear_abort_flag();
            Some(Answer::TimedOut)
        }
    }
}

/// How long [`ask`] waits for an abandoned dialog to notice the abort flag and
/// restore the terminal before it gives up and returns anyway. The modal polls
/// every 100ms, so this comfortably covers a well-behaved teardown while still
/// bounding the worst case.
const ABORT_GRACE: Duration = Duration::from_millis(500);

/// Parse the model's `ask_user_question` input into [`Question`]s.
///
/// Accepts the Command Code shape (`{"questions": [{question, header, options:
/// [{label, description}], multiSelect}]}`) and a convenient single-question
/// shorthand (`{"question": "...", "options": ["a", "b"]}` where options may be
/// bare strings). Returns a human-readable error for anything malformed, so the
/// model can correct itself.
pub fn parse_questions(input: &serde_json::Value) -> Result<Vec<Question>, String> {
    let arr = match input.get("questions") {
        Some(serde_json::Value::Array(a)) => a.clone(),
        Some(_) => return Err("'questions' must be an array".into()),
        None => {
            // Single-question shorthand.
            if input.get("question").is_some() {
                vec![input.clone()]
            } else {
                return Err("missing 'questions' (an array of question objects)".into());
            }
        }
    };
    if arr.is_empty() {
        return Err("'questions' must contain at least one question".into());
    }
    if arr.len() > 4 {
        return Err(format!("too many questions ({}); ask at most 4 at a time", arr.len()));
    }
    let mut out = Vec::new();
    for (i, q) in arr.iter().enumerate() {
        let question = q
            .get("question")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("question #{} is missing a non-empty 'question'", i + 1))?;
        let header = q
            .get("header")
            .and_then(|v| v.as_str())
            .unwrap_or("Question")
            .trim()
            .to_string();
        let multi_select = q
            .get("multiSelect")
            .or_else(|| q.get("multi_select"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut options: Vec<Choice> = Vec::new();
        if let Some(opts) = q.get("options") {
            let arr = opts
                .as_array()
                .ok_or_else(|| format!("question #{}: 'options' must be an array", i + 1))?;
            if arr.len() > 12 {
                return Err(format!("question #{}: too many options ({}); max 12", i + 1, arr.len()));
            }
            for (j, o) in arr.iter().enumerate() {
                match o {
                    // Bare string shorthand: `"options": ["yes", "no"]`.
                    serde_json::Value::String(s) => {
                        if s.trim().is_empty() {
                            return Err(format!("question #{}: option #{} is empty", i + 1, j + 1));
                        }
                        options.push(Choice { label: s.trim().to_string(), description: String::new() });
                    }
                    serde_json::Value::Object(_) => {
                        let label = o
                            .get("label")
                            .and_then(|v| v.as_str())
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .ok_or_else(|| {
                                format!("question #{}: option #{} is missing a 'label'", i + 1, j + 1)
                            })?;
                        let description = o
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        options.push(Choice { label, description });
                    }
                    _ => {
                        return Err(format!(
                            "question #{}: option #{} must be a string or an object with 'label'",
                            i + 1,
                            j + 1
                        ));
                    }
                }
            }
        }
        out.push(Question { header, question, options, multi_select });
    }
    Ok(out)
}

/// Serializes tests (anywhere in the crate) that install/uninstall the
/// process-global renderer. Poisoning is ignored so one failing test can't
/// cascade into unrelated failures.
#[cfg(test)]
pub(crate) fn ask_lock() -> std::sync::MutexGuard<'static, ()> {
    tests::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// The renderer registry is process-global, so tests that install/uninstall
    /// it must not run concurrently (otherwise one test's renderer answers
    /// another's question, or `uninstall` makes an in-flight ask look missing).
    pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn parse_accepts_the_command_code_shape() {
        let input = json!({
            "questions": [{
                "header": "Approach",
                "question": "Which approach do you prefer?",
                "options": [
                    {"label": "Option A", "description": "Description A"},
                    {"label": "Option B", "description": "Description B"}
                ],
                "multiSelect": false
            }]
        });
        let qs = parse_questions(&input).expect("parse");
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].header, "Approach");
        assert_eq!(qs[0].question, "Which approach do you prefer?");
        assert_eq!(qs[0].options.len(), 2);
        assert_eq!(qs[0].options[0].label, "Option A");
        assert_eq!(qs[0].options[1].description, "Description B");
        assert!(!qs[0].multi_select);
    }

    #[test]
    fn parse_accepts_single_question_and_bare_string_options() {
        let qs = parse_questions(&json!({
            "question": "Ship it?",
            "options": ["yes", "no"]
        }))
        .expect("parse");
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].header, "Question", "header defaults");
        assert_eq!(qs[0].options[0].label, "yes");
        // Free-text question (no options) is also valid.
        let qs = parse_questions(&json!({ "question": "What name?" })).expect("parse");
        assert!(qs[0].options.is_empty());
    }

    #[test]
    fn parse_rejects_malformed_input_with_actionable_errors() {
        assert!(parse_questions(&json!({})).is_err(), "no question at all");
        assert!(parse_questions(&json!({ "questions": [] })).is_err(), "empty array");
        assert!(parse_questions(&json!({ "questions": "nope" })).is_err(), "not an array");
        assert!(parse_questions(&json!({ "questions": [{}] })).is_err(), "missing question text");
        let err = parse_questions(&json!({ "questions": [{ "question": "x", "options": [{}] }] }))
            .unwrap_err();
        assert!(err.contains("missing a 'label'"), "got: {err}");
        let many: Vec<serde_json::Value> =
            (0..5).map(|i| json!({ "question": format!("q{i}") })).collect();
        assert!(parse_questions(&json!({ "questions": many })).is_err(), "too many questions");
    }

    #[test]
    fn ask_returns_none_without_a_renderer() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        uninstall();
        assert!(!interactive());
        let q = Question {
            header: "h".into(),
            question: "q".into(),
            options: vec![],
            multi_select: false,
        };
        assert_eq!(ask(&q), None, "no renderer => no answer, never a hang");
    }

    #[test]
    fn ask_round_trips_through_an_installed_renderer() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        install(std::sync::Arc::new(|q| {
            assert_eq!(q.question, "pick one");
            Answer::Chosen(vec!["Option A".into()])
        }));
        assert!(interactive());
        let q = Question {
            header: "h".into(),
            question: "pick one".into(),
            options: vec![Choice { label: "Option A".into(), description: String::new() }],
            multi_select: false,
        };
        let a = ask(&q).expect("answer");
        assert_eq!(a, Answer::Chosen(vec!["Option A".into()]));
        assert_eq!(a.render(), "Option A");
        uninstall();
        assert!(!interactive());
    }

    #[test]
    fn answer_rendering_covers_every_variant() {
        assert_eq!(Answer::Text("hi".into()).render(), "hi");
        assert_eq!(Answer::Text("  ".into()).render(), "(empty answer)");
        assert_eq!(Answer::Chosen(vec!["a".into(), "b".into()]).render(), "a, b");
        assert_eq!(Answer::Chosen(vec![]).render(), "(no option selected)");
        assert!(Answer::Dismissed.render().contains("dismissed"));
        assert!(Answer::TimedOut.render().contains("no answer"));
        // The two must be distinguishable — that is the point of the variant.
        assert_ne!(Answer::Dismissed.render(), Answer::TimedOut.render());
    }

    /// A renderer that never answers must not hang the caller: the timeout
    /// turns it into an explicit `TimedOut` (distinct from a real dismissal)
    /// that the model can act on.
    #[test]
    fn ask_times_out_instead_of_hanging() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: test-local env mutation, restored immediately below; the
        // module's tests share the config env lock where it matters.
        let old = std::env::var_os("PIR_ASK_TIMEOUT_SECS");
        unsafe { std::env::set_var("PIR_ASK_TIMEOUT_SECS", "1"); }
        install(std::sync::Arc::new(|_| {
            std::thread::sleep(Duration::from_secs(30));
            Answer::Text("too late".into())
        }));
        let q = Question {
            header: "h".into(),
            question: "q".into(),
            options: vec![],
            multi_select: false,
        };
        let start = std::time::Instant::now();
        let a = ask(&q);
        let elapsed = start.elapsed();
        uninstall();
        match old {
            Some(v) => unsafe { std::env::set_var("PIR_ASK_TIMEOUT_SECS", v) },
            None => unsafe { std::env::remove_var("PIR_ASK_TIMEOUT_SECS") },
        }
        assert_eq!(a, Some(Answer::TimedOut), "a silent renderer means nobody answered");
        assert!(elapsed < Duration::from_secs(10), "must not wait out the renderer ({elapsed:?})");
    }

    /// A timed-out question must not leave its abort flag installed, or the
    /// *next* unrelated modal (a menu, the session picker) would close the
    /// instant it opened.
    #[test]
    fn timeout_does_not_poison_later_dialogs() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("PIR_ASK_TIMEOUT_SECS");
        unsafe { std::env::set_var("PIR_ASK_TIMEOUT_SECS", "1"); }
        install(std::sync::Arc::new(|_| {
            std::thread::sleep(Duration::from_secs(30));
            Answer::Dismissed
        }));
        let q = Question {
            header: "h".into(),
            question: "q".into(),
            options: vec![],
            multi_select: false,
        };
        let a = ask(&q);
        uninstall();
        match old {
            Some(v) => unsafe { std::env::set_var("PIR_ASK_TIMEOUT_SECS", v) },
            None => unsafe { std::env::remove_var("PIR_ASK_TIMEOUT_SECS") },
        }
        assert_eq!(a, Some(Answer::TimedOut));
        assert!(
            !crate::modal::aborted(),
            "the abort flag must be cleared after ask() returns, or the next dialog aborts itself"
        );
    }

    /// An empty answer at the line prompt is a real dismissal (the operator was
    /// there and declined) and must NOT be reported as a timeout.
    #[test]
    fn dismissal_and_timeout_are_distinct() {
        assert_ne!(Answer::Dismissed, Answer::TimedOut);
        assert!(Answer::Dismissed.render().contains("dismissed"));
        assert!(Answer::TimedOut.render().contains("no answer"));
    }

    /// The whole point of the fix: on timeout the renderer must be *told to
    /// stop* (so the modal tears itself down and releases the keyboard), not
    /// silently abandoned on screen while the agent is told "no answer".
    #[test]
    fn timeout_aborts_the_renderer_instead_of_leaving_it_running() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("PIR_ASK_TIMEOUT_SECS");
        unsafe { std::env::set_var("PIR_ASK_TIMEOUT_SECS", "1"); }

        // Stand-in for the modal: run until the crate tells it to abort, exactly
        // as `modal::read_key` polls `modal::aborted()`.
        let saw_abort = Arc::new(AtomicBool::new(false));
        let flag = saw_abort.clone();
        install(std::sync::Arc::new(move |_| {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while std::time::Instant::now() < deadline {
                if crate::modal::aborted() {
                    flag.store(true, Ordering::SeqCst);
                    return Answer::Dismissed;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Answer::Dismissed
        }));

        let q = Question {
            header: "h".into(),
            question: "q".into(),
            options: vec![],
            multi_select: false,
        };
        let a = ask(&q);
        uninstall();
        match old {
            Some(v) => unsafe { std::env::set_var("PIR_ASK_TIMEOUT_SECS", v) },
            None => unsafe { std::env::remove_var("PIR_ASK_TIMEOUT_SECS") },
        }

        assert_eq!(a, Some(Answer::TimedOut));
        assert!(
            saw_abort.load(Ordering::SeqCst),
            "the renderer must observe the abort flag — otherwise the modal is still \
             on screen owning the keyboard after the agent was told 'no answer'"
        );
    }
}
