//! Goal-continuation framework.
//!
//! A `Goal` is the single source of truth for a long-running, multi-step
//! objective. The model records progress by calling the `update_goal` tool as
//! it works; every change is persisted to a `.goal.json` file sitting next to
//! the session transcript. Because the goal lives on disk (not in the model's
//! context window), a session can be interrupted — by ctrl-c, a crash, a
//! timeout, or simply `pir` exiting — and later resumed with `pir -c`, which
//! reloads the goal and drives the agent to the next pending step.

use serde::{Deserialize, Serialize};
use serde_json;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    #[default]
    Pending,
    InProgress,
    Done,
    Blocked,
}

impl StepStatus {
    pub fn label(self) -> &'static str {
        match self {
            StepStatus::Pending => "pending",
            StepStatus::InProgress => "in-progress",
            StepStatus::Done => "done",
            StepStatus::Blocked => "blocked",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GoalStatus {
    #[default]
    Active,
    Complete,
    Blocked,
    Aborted,
}

impl GoalStatus {
    pub fn label(self) -> &'static str {
        match self {
            GoalStatus::Active => "active",
            GoalStatus::Complete => "complete",
            GoalStatus::Blocked => "blocked",
            GoalStatus::Aborted => "aborted",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, GoalStatus::Complete | GoalStatus::Aborted)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Step {
    pub id: usize,
    pub description: String,
    #[serde(default)]
    pub status: StepStatus,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Goal {
    pub objective: String,
    #[serde(default)]
    pub status: GoalStatus,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub notes: String,
    /// Monotonic id allocator for steps. Skipped from the on-disk JSON and
    /// reconstructed from `steps` on load.
    #[serde(skip)]
    pub next_id: usize,
}

impl Goal {
    pub fn new(objective: &str) -> Self {
        Goal {
            objective: objective.to_string(),
            status: GoalStatus::Active,
            steps: Vec::new(),
            notes: String::new(),
            next_id: 1,
        }
    }

    pub fn add_steps(&mut self, descs: &[String]) -> Vec<usize> {
        let mut ids = Vec::new();
        for d in descs {
            let id = self.next_id;
            self.next_id += 1;
            self.steps.push(Step {
                id,
                description: d.clone(),
                status: StepStatus::Pending,
                note: String::new(),
            });
            ids.push(id);
        }
        ids
    }

    pub fn update_step(&mut self, id: usize, status: StepStatus, note: &str) {
        if let Some(s) = self.steps.iter_mut().find(|s| s.id == id) {
            s.status = status;
            if !note.is_empty() {
                s.note = note.to_string();
            }
        }
    }

    /// The step the agent should pick up next: the first not-yet-done step
    /// (pending or in-progress), falling back to a blocked step so the
    /// framework surfaces what's stuck.
    pub fn next_step(&self) -> Option<&Step> {
        self.steps
            .iter()
            .find(|s| matches!(s.status, StepStatus::Pending | StepStatus::InProgress))
            .or_else(|| self.steps.iter().find(|s| s.status == StepStatus::Blocked))
    }

    pub fn done_count(&self) -> usize {
        self.steps.iter().filter(|s| s.status == StepStatus::Done).count()
    }

    /// Human- and model-readable snapshot of the goal, used both for the
    /// system-prompt injection and for the `update_goal` tool's feedback.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("GOAL: {}\n", self.objective));
        out.push_str(&format!("STATUS: {}\n", self.status.label()));
        if self.steps.is_empty() {
            out.push_str("(no steps defined yet — propose a plan with update_goal add_steps)\n");
        } else {
            out.push_str("STEPS:\n");
            for s in &self.steps {
                out.push_str(&format!(
                    "  [{}] #{} {}{}\n",
                    s.status.label(),
                    s.id,
                    s.description,
                    if s.note.is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", s.note)
                    }
                ));
            }
        }
        if !self.notes.is_empty() {
            out.push_str(&format!("NOTES: {}\n", self.notes));
        }
        out
    }
}

/// Owns a goal file (`<session>.goal.json`) and flushes it on every change so
/// progress is durable across interruptions.
pub struct GoalStore {
    path: PathBuf,
    pub goal: Goal,
}

impl GoalStore {
    pub fn new(log_path: &Path, objective: &str) -> Self {
        let path = log_path.with_extension("goal.json");
        GoalStore { path, goal: Goal::new(objective) }
    }

    /// Load an existing goal file for a resumed session, restoring `next_id`.
    pub fn attach(log_path: Option<&Path>) -> Option<Self> {
        let log_path = log_path?;
        let path = log_path.with_extension("goal.json");
        let raw = fs::read_to_string(&path).ok()?;
        let mut goal: Goal = serde_json::from_str(&raw).ok()?;
        goal.next_id = goal.steps.iter().map(|s| s.id).max().map(|m| m + 1).unwrap_or(1);
        Some(GoalStore { path, goal })
    }

    pub fn save(&self) {
        if let Ok(s) = serde_json::to_string_pretty(&self.goal) {
            let _ = fs::write(&self.path, s);
        }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

/// Parse a step status from the flexible vocabulary the model may use.
pub fn parse_step_status(s: &str) -> Option<StepStatus> {
    match s.to_ascii_lowercase().as_str() {
        "pending" | "todo" | "not started" | "queued" => Some(StepStatus::Pending),
        "in_progress" | "inprogress" | "in-progress" | "started" | "doing" => {
            Some(StepStatus::InProgress)
        }
        "done" | "complete" | "completed" | "finished" => Some(StepStatus::Done),
        "blocked" | "stuck" => Some(StepStatus::Blocked),
        _ => None,
    }
}

/// Parse a goal status from the flexible vocabulary the model may use.
pub fn parse_goal_status(s: &str) -> Option<GoalStatus> {
    match s.to_ascii_lowercase().as_str() {
        "active" | "open" | "in_progress" | "inprogress" | "running" => Some(GoalStatus::Active),
        "complete" | "completed" | "done" | "finished" => Some(GoalStatus::Complete),
        "blocked" | "stuck" => Some(GoalStatus::Blocked),
        "aborted" | "abandoned" | "cancelled" | "canceled" => Some(GoalStatus::Aborted),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, unique temp dir per call so tests never collide on a shared
    /// global path (e.g. a root-owned /tmp/pir_goal_tests left by another run)
    /// and never leak state between runs.
    fn tmp_log(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pir_goal_tests_{}_{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir.join(format!("{name}.jsonl"))
    }

    #[test]
    fn status_parsing_accepts_flexible_vocabulary() {
        assert_eq!(parse_step_status("in_progress"), Some(StepStatus::InProgress));
        assert_eq!(parse_step_status("finished"), Some(StepStatus::Done));
        assert_eq!(parse_step_status("stuck"), Some(StepStatus::Blocked));
        assert_eq!(parse_step_status("nonsense"), None);
        assert_eq!(parse_goal_status("abandoned"), Some(GoalStatus::Aborted));
        assert_eq!(parse_goal_status("running"), Some(GoalStatus::Active));
        assert_eq!(parse_goal_status("???"), None);
    }

    #[test]
    fn next_step_skips_done_and_prefers_pending() {
        let mut g = Goal::new("build it");
        let ids = g.add_steps(&["first".into(), "second".into(), "third".into()]);
        assert_eq!(ids, vec![1, 2, 3]);
        g.update_step(1, StepStatus::Done, "");
        g.update_step(3, StepStatus::Blocked, "");
        assert_eq!(g.next_step().unwrap().id, 2);
        g.update_step(2, StepStatus::Done, "");
        assert_eq!(g.next_step().unwrap().id, 3);
    }

    #[test]
    fn store_persists_and_reattaches_with_next_id() {
        let log = tmp_log("sess.jsonl");
        {
            let mut store = GoalStore::new(&log, "ship the thing");
            store.goal.add_steps(&["step one".into(), "step two".into()]);
            store.goal.update_step(1, StepStatus::Done, "done it");
            store.save();
        }
        let store = GoalStore::attach(Some(&log)).expect("goal should reattach");
        assert_eq!(store.goal.objective, "ship the thing");
        assert_eq!(store.goal.steps.len(), 2);
        assert_eq!(store.goal.steps[0].status, StepStatus::Done);
        assert_eq!(store.goal.next_id, 3);
        let _ = std::fs::remove_file(&log.with_extension("goal.json"));
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn summary_renders_status_and_steps() {
        let mut g = Goal::new("objective");
        g.add_steps(&["do x".into()]);
        let s = g.summary();
        assert!(s.contains("GOAL: objective"));
        assert!(s.contains("STATUS: active"));
        assert!(s.contains("[pending] #1 do x"));
    }
}
