//! Auto-approve / auto-deny rules for the `/quarantine` review.
//!
//! Rule lines: `ACTION OPS: REGEX` — ACTION ∈ {APPROVE, DENY}, OPS ∈
//! {CREATE, MODIFY, DELETE} (comma-separated; ADD/DEL/REMOVE synonyms). The
//! regex (full `regex` crate) is matched against the real target path the write
//! would land on. First DENY wins, else first APPROVE. The default file is
//! `~/.pi/agent/quarantine-rules`, created with a starting cache policy
//! (caches die with the worktree/container; deleting cache is fine — rebuild).

use regex::Regex;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewOp {
    Create,
    Modify,
    Delete,
}
impl ReviewOp {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewOp::Create => "CREATE",
            ReviewOp::Modify => "MODIFY",
            ReviewOp::Delete => "DELETE",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleVerdict {
    Approve,
    Deny,
}

#[derive(Debug, Clone)]
pub struct Rule {
    action: RuleVerdict,
    ops: Vec<ReviewOp>,
    re: Regex,
}

const DEFAULT_RULES: &str = "\
# /quarantine auto-applies/denies staged writes by these rules.
# Lines: ACTION OPS: REGEX   (ACTION = APPROVE|DENY; OPS = CREATE,MODIFY,DELETE; ADD/DEL synonyms)
# The regex is matched against the real target path the write would land on.
# Starting policy — caches die with the worktree/container; deleting cache is fine (rebuild):
DENY CREATE,MODIFY: .*/[.]cache/.*
APPROVE DELETE: .*/[.]cache/.*
";

pub fn rules_path() -> std::path::PathBuf {
    crate::config::pi_dir().join("agent").join("quarantine-rules")
}

pub fn load_rules() -> Vec<Rule> {
    let path = rules_path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => {
            let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
            let _ = std::fs::write(&path, DEFAULT_RULES);
            DEFAULT_RULES.to_string()
        }
    };
    let mut rules = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(idx) = line.find(':') else { continue };
        let (head, re_str) = line.split_at(idx);
        let re_str = re_str[1..].trim();
        if re_str.is_empty() {
            continue;
        }
        let mut it = head.split_whitespace();
        let Some(action) = it.next() else { continue };
        let action = match action.to_ascii_lowercase().as_str() {
            "approve" | "allow" | "yes" => RuleVerdict::Approve,
            "deny" | "disallow" | "no" => RuleVerdict::Deny,
            _ => continue,
        };
        let ops = it
            .next()
            .unwrap_or("")
            .split(',')
            .filter_map(|o| match o.trim().to_ascii_lowercase().as_str() {
                "create" | "add" => Some(ReviewOp::Create),
                "modify" | "mod" => Some(ReviewOp::Modify),
                "delete" | "del" | "remove" => Some(ReviewOp::Delete),
                _ => None,
            })
            .collect::<Vec<_>>();
        if ops.is_empty() {
            continue;
        }
        let Ok(re) = Regex::new(re_str) else { continue };
        rules.push(Rule { action, ops, re });
    }
    rules
}

/// Auto-verdict for a staged write, if any rule matches (first DENY wins, else
/// first APPROVE).
pub fn evaluate(op: ReviewOp, real_path: &str) -> Option<RuleVerdict> {
    let mut approve = None;
    for r in &load_rules() {
        if r.ops.contains(&op) && r.re.is_match(real_path) {
            match r.action {
                RuleVerdict::Deny => return Some(RuleVerdict::Deny),
                RuleVerdict::Approve => approve = Some(RuleVerdict::Approve),
            }
        }
    }
    approve
}

/// Add a rule derived from the `/quarantine apply r <idx> <regex>` flow: the
/// user edits a filename into a regex; we confirm the ORIGINAL path still
/// matches before saving (so a typo doesn't silently stop matching).
pub fn add_rule(action: RuleVerdict, op: ReviewOp, regex: &str, sample_path: &str) -> std::io::Result<()> {
    let re = Regex::new(regex)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("bad regex: {e}")))?;
    if !re.is_match(sample_path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("your regex no longer matches the original path '{sample_path}'; not saved"),
        ));
    }
    let path = rules_path();
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    use std::io::Write;
    let act = match action {
        RuleVerdict::Approve => "APPROVE",
        RuleVerdict::Deny => "DENY",
    };
    writeln!(file, "{act} {}: {regex}   # from /quarantine r (matches {sample_path})", op.as_str())?;
    Ok(())
}
