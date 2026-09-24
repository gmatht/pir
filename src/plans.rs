//! Plan documents and plan mode (Feature 5).
//!
//! Plan mode is a *workflow* posture: the agent explores read-only, writes a
//! plan, and waits for approval before touching anything. That ordering is what
//! makes a large change reviewable — the operator sees the approach before the
//! diff exists, which is far cheaper to correct.
//!
//! This module owns the on-disk side: where plans live and how they are named
//! and listed.
//!
//! ```text
//!   ~/.pi/agent/plans/<slug>.md      one file per plan
//!   ~/.pi/agent/plans/<slug>.goal.json   (not used here — goals are per-session)
//! ```
//!
//! Plans are *not* per-session: a plan written in one session is meant to be
//! picked up in the next (`/plan list`), and the operator may well hand-edit it
//! between the two. The session's own plan-mode flag lives next to the session
//! log (see the `.plan` sidecar handled by the agent), so mode and document have
//! independent lifetimes.

use std::path::{Path, PathBuf};

/// Directory that holds every plan document.
pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("PIR_PLANS_DIR") {
        let p = PathBuf::from(d);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    crate::config::pi_dir().join("agent").join("plans")
}

/// Normalize a plan name into a filename slug. Empty input yields `"plan"`.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if (c.is_whitespace() || c == '-' || c == '_' || c == '/' || c == '.') && !out.is_empty() && !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let s = out.trim_matches('-').to_string();
    if s.is_empty() { "plan".to_string() } else { s }
}

/// Where a plan named `name` lives. The name is slugged, so a caller can pass
/// anything (including a title with spaces and punctuation) without escaping it.
pub fn path_for(name: &str) -> PathBuf {
    dir().join(format!("{}.md", slug(name)))
}

/// Write `content` as the plan named `name`, creating the plans directory if
/// needed. Returns the path written.
pub fn write(name: &str, content: &str) -> Result<PathBuf, String> {
    let dir = dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create plans dir {}: {e}", dir.display()))?;
    let path = path_for(name);
    let body = if content.trim().is_empty() {
        format!("# Plan: {name}\n\n(empty)\n")
    } else {
        content.to_string()
    };
    std::fs::write(&path, body).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

/// Read a plan by name. `Err` distinguishes "no such plan" from an I/O failure,
/// because the model needs to know whether to write it or fix permissions.
pub fn read(name: &str) -> Result<String, String> {
    let path = path_for(name);
    std::fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("no plan named '{name}' ({} does not exist)", path.display())
        } else {
            format!("cannot read {}: {e}", path.display())
        }
    })
}

/// Existing plans, newest first, as `(slug, path, modified)`.
pub fn list() -> Vec<(String, PathBuf, std::time::SystemTime)> {
    let mut out: Vec<(String, PathBuf, std::time::SystemTime)> = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir()) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = p.file_stem().map(|s| s.to_string_lossy().to_string()) else { continue };
        let modified = e
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        out.push((stem, p, modified));
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.2));
    out
}

/// One-line-per-plan listing for the REPL, or a hint when there are none.
pub fn listing() -> String {
    let plans = list();
    if plans.is_empty() {
        return format!(
            "(no plans yet — the agent writes one with the plan_write tool in /plan mode; dir: {})",
            dir().display()
        );
    }
    let mut s = format!("{} plan(s) in {}:\n", plans.len(), dir().display());
    for (name, path, _) in &plans {
        let title = first_heading(path).unwrap_or_else(|| name.clone());
        s.push_str(&format!("  {name}  —  {title}\n"));
    }
    s.trim_end().to_string()
}

/// The first markdown heading of a plan, used as a display title.
fn first_heading(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    for line in raw.lines() {
        let l = line.trim();
        if let Some(h) = l.strip_prefix("# ") {
            return Some(h.trim().to_string());
        }
        if !l.is_empty() && !l.starts_with('#') {
            return Some(l.to_string());
        }
    }
    None
}

/// A stable, human-recognisable default name for an unnamed plan: the session's
/// timestamp plus the first few words of the objective.
pub fn default_name(objective: &str) -> String {
    let stamp = crate::term::timestamp_compact();
    let words: Vec<&str> = objective.split_whitespace().take(6).collect();
    format!("{} {}", stamp, words.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test works in a throwaway plans dir so the operator's real
    /// `~/.pi/agent/plans` is never touched.
    fn with_dir<T>(tag: &str, f: impl FnOnce(&Path) -> T) -> T {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let d = std::env::temp_dir().join(format!("pir_plans_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let old = std::env::var_os("PIR_PLANS_DIR");
        // SAFETY: test-local env mutation under the shared env lock.
        unsafe { std::env::set_var("PIR_PLANS_DIR", &d); }
        let out = f(&d);
        match old {
            Some(v) => unsafe { std::env::set_var("PIR_PLANS_DIR", v) },
            None => unsafe { std::env::remove_var("PIR_PLANS_DIR") },
        }
        let _ = std::fs::remove_dir_all(&d);
        out
    }

    #[test]
    fn slug_is_safe_and_never_empty() {
        assert_eq!(slug("Add Retry Logic!"), "add-retry-logic");
        assert_eq!(slug("../../etc/passwd"), "etc-passwd");
        assert_eq!(slug("!!!"), "plan");
        assert_eq!(slug(""), "plan");
    }

    #[test]
    fn write_read_round_trip_and_missing_is_reported_clearly() {
        with_dir("rw", |_| {
            let p = write("Add retry logic", "# Plan: retries\n\n1. do the thing\n").unwrap();
            assert!(p.ends_with("add-retry-logic.md"), "got {}", p.display());
            let back = read("Add retry logic").unwrap();
            assert!(back.contains("do the thing"));
            // Slug-equivalent names resolve to the same document.
            assert!(read("add_retry_logic").unwrap().contains("do the thing"));
            let err = read("nope").unwrap_err();
            assert!(err.contains("no plan named"), "got: {err}");
        });
    }

    #[test]
    fn empty_content_still_writes_a_valid_document() {
        with_dir("empty", |_| {
            let p = write("blank", "   ").unwrap();
            let back = std::fs::read_to_string(p).unwrap();
            assert!(back.contains("# Plan: blank"), "got: {back}");
        });
    }

    #[test]
    fn list_is_newest_first_and_reports_titles() {
        with_dir("list", |_| {
            assert!(listing().contains("no plans yet"));
            write("older", "# Plan: older one\nbody\n").unwrap();
            // Ensure a distinct mtime ordering.
            std::thread::sleep(std::time::Duration::from_millis(1100));
            write("newer", "# Plan: newer one\nbody\n").unwrap();
            let plans = list();
            assert_eq!(plans.len(), 2);
            assert_eq!(plans[0].0, "newer", "newest must be first: {plans:?}");
            let l = listing();
            assert!(l.contains("newer") && l.contains("older"));
            assert!(l.contains("newer one"), "titles must be shown: {l}");
        });
    }

    #[test]
    fn default_name_uses_timestamp_and_objective_words() {
        let n = default_name("add exponential backoff to the retry loop because flaky tests");
        assert!(n.contains("add exponential backoff"), "got: {n}");
        assert!(n.split_whitespace().count() <= 7, "must stay short: {n}");
        assert_eq!(slug(&n), n.replace(' ', "-"), "slug of the default name is stable");
    }
}
