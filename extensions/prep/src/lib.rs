//! prep — command-preview / safe-find extension.
//!
//! Off by default; enable with `PIR_PREP=1`. It shadows the builtin `bash`
//! tool with an enhanced version that:
//!
//! * **Tail preview** — when a command pipes through `tail -n N` (or `head
//!   -n N`) and would otherwise discard the earlier output, the tool runs the
//!   command *twice*: once unfiltered (capped) so you can see what the tail is
//!   drawn from, and once as written. The preview is trimmed to a few lines so
//!   it stays cheap. This answers "what is going into `tail`?" without the
//!   model having to re-run by hand.
//! * **Safe find** — rewrites a leading `find /` (full-filesystem search) into
//!   `locate` (which uses an indexed database, so it's fast and doesn't walk
//!   the whole tree). `locate` is preferred; if it's missing the command is
//!   left as-is with a note. A plain `find` scoped to the cwd is untouched.
//!
//! Everything else (`read_file`, `write_file`, `edit_file`, `list_dir`,
//! `job_*`, `update_goal`, `commit`) is still provided by the builtin — this
//! extension only replaces `bash`. Register it *before* the builtin so its
//! `bash` wins; the model never sees two `bash` tools.

use crate::plugin::{Outcome, Registry, ToolBackend, ToolSpec};
use serde_json::json;
use std::path::Path;
use std::process::Command;

const PREVIEW_LINES: usize = 12;

struct Prep {
    enabled: bool,
}

impl Prep {
    fn new() -> Self {
        let enabled = std::env::var_os("PIR_PREP").map(|v| v != "0" && !v.is_empty()).unwrap_or(false);
        Prep { enabled }
    }

    /// Rewrite a leading `find / ...` into `locate ...` (best-effort, fast,
    /// indexed). Returns (rewritten_command, did_rewrite).
    fn safe_find(cmd: &str) -> (String, bool) {
        let trimmed = cmd.trim_start();
        if !trimmed.starts_with("find /") && !trimmed.starts_with("find / ") {
            return (cmd.to_string(), false);
        }
        // Only rewrite when `locate` is available; otherwise leave as-is.
        let has_locate = Command::new("locate")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !has_locate {
            return (cmd.to_string(), false);
        }
        // Translate the common `find / -name PATTERN` form into `locate PATTERN`.
        // Pull a quoted or bare `-name` argument.
        let rest = &trimmed["find ".len()..];
        if let Some(pos) = rest.find("-name") {
            let after = &rest[pos + "-name".len()..];
            let pat = after.trim_start();
            let pat = pat
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .split_whitespace()
                .next()
                .unwrap_or("");
            if !pat.is_empty() {
                let m = if pat.contains('*') {
                    pat.to_string()
                } else {
                    format!("*{pat}*")
                };
                return (format!("locate {m}"), true);
            }
        }
        // `find /` without a -name: fall back to a broad locate of any arg token.
        let first_tok = rest.split_whitespace().find(|t| !t.starts_with('-')).unwrap_or("");
        if !first_tok.is_empty() {
            return (format!("locate *{first_tok}*"), true);
        }
        (cmd.to_string(), false)
    }

    /// Detect `... | tail -n N` / `head -n N` so we can show a preview of the
    /// input to the filter. Returns (filter, n) when found.
    fn tail_filter(cmd: &str) -> Option<(&'static str, usize)> {
        let lower = cmd.to_lowercase();
        // crude but effective: look for the last `| tail -n N` or `| head -n N`
        if let Some(pos) = lower.rfind("| tail") {
            let n = parse_n(&cmd[pos..], "tail");
            return Some(("tail", n));
        }
        if let Some(pos) = lower.rfind("| head") {
            let n = parse_n(&cmd[pos..], "head");
            return Some(("head", n));
        }
        None
    }
}

/// Parse the `-n N` count after a `tail`/`head` token within a suffix string.
fn parse_n(suffix: &str, which: &str) -> usize {
    let idx = suffix.find(which).unwrap_or(0) + which.len();
    let rest = &suffix[idx..];
    // Accept `-n N`, `-nN`, or just ` N`.
    let after = rest.trim_start().trim_start_matches("-n").trim_start();
    after
        .split_whitespace()
        .next()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(10)
}

/// Run a command and return its combined output (stdout+stderr) + exit code.
fn run(cmd: &str) -> (String, i32) {
    let out = Command::new("bash").arg("-lc").arg(cmd).output();
    match out {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            let err = String::from_utf8_lossy(&o.stderr).into_owned();
            if !err.trim().is_empty() {
                if !text.trim().is_empty() {
                    text.push('\n');
                }
                text.push_str("[stderr]\n");
                text.push_str(&err);
            }
            let code = o.status.code().unwrap_or(-1);
            (text, code)
        }
        Err(e) => (format!("[pir] spawn error: {e}"), 127),
    }
}

/// Keep only the first `n` lines (for the preview), trimmed.
fn head_lines(text: &str, n: usize) -> String {
    text.lines().take(n).collect::<Vec<_>>().join("\n")
}

impl ToolBackend for Prep {
    fn name(&self) -> &'static str {
        "prep"
    }

    fn specs(&self) -> Vec<ToolSpec> {
        if !self.enabled {
            return Vec::new();
        }
        vec![ToolSpec {
            name: "bash",
            description:
                "Run a shell command (enhanced: prep). Tail/head preview: when the command pipes \
                 through `tail -n N` / `head -n N`, it also shows a short preview of the input \
                 feeding that filter so you can see what is being discarded. Safe find: a leading \
                 `find /` (whole-filesystem) is rewritten to fast indexed `locate`. Otherwise \
                 identical to the builtin bash. Returns stdout+stderr (truncated) and exit code.",
            schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The shell command to execute" }
                },
                "required": ["command"]
            }),
        }]
    }

    fn run(&mut self, name: &str, input: &serde_json::Value) -> Outcome {
        if name != "bash" {
            return Outcome::err(format!("unknown tool '{name}'"));
        }
        let cmd = match input.get("command").and_then(|v| v.as_str()) {
            Some(c) if !c.trim().is_empty() => c.to_string(),
            _ => return Outcome::err("bash: missing 'command'".into()),
        };

        // 1) Safe find rewrite.
        let (cmd, rewrote) = Self::safe_find(&cmd);
        let mut note = String::new();
        if rewrote {
            note.push_str(&format!("[prep] rewrote `find /` -> `{cmd}` (locate is indexed/fast)\n"));
        }

        // 2) Run the (possibly rewritten) command.
        let (mut text, code) = run(&cmd);

        // 3) Tail/head preview: if the command filters through tail/head, also
        //    run a preview of the unfiltered stream so we can see context.
        if let Some((which, n)) = Self::tail_filter(&cmd) {
            // Build a preview command: drop the trailing `| tail/head ...`.
            let base = strip_filter(&cmd);
            let (preview, _) = run(&base);
            let shown = head_lines(&preview, PREVIEW_LINES);
            note.push_str(&format!(
                "[prep] {which} -n {n} preview (first {PREVIEW_LINES} lines of what feeds the filter):\n{shown}\n"
            ));
        }

        if !note.is_empty() {
            text = format!("{note}\n{text}");
        }
        if code != 0 {
            text.push_str(&format!("\n[exit code {code}]"));
        }
        crate::plugin::truncate(&mut text, 30_000);
        Outcome::ok(text)
    }
}

/// Remove a trailing `| tail ...` / `| head ...` pipe stage from a command so
/// the preview runs the unfiltered stream.
fn strip_filter(cmd: &str) -> String {
    let lower = cmd.to_lowercase();
    let cut = lower.rfind("| tail").or_else(|| lower.rfind("| head"));
    match cut {
        Some(i) => {
            // Walk back to the pipe '|' (the rfind already points at '|').
            cmd[..i].trim_end().to_string()
        }
        None => cmd.to_string(),
    }
}

pub fn register(reg: &mut Registry) {
    reg.add(Box::new(Prep::new()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_find_slash_to_locate() {
        // Only meaningful when `locate` exists; skip otherwise.
        if Command::new("locate").arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
            // A bare pattern gets `*` wildcards wrapped; one that already
            // contains `*` is used verbatim.
            let (c, ok) = Prep::safe_find("find / -name '*.rs'");
            assert!(ok, "find / should be rewritten");
            assert!(c.starts_with("locate"), "got {c}");
            assert_eq!(c, "locate *.rs", "pattern with * is used verbatim");

            let (c2, ok2) = Prep::safe_find("find / -name Cargo.toml");
            assert!(ok2, "find / should be rewritten");
            assert_eq!(c2, "locate *Cargo.toml*", "bare pattern gets wildcards");
        }
    }

    #[test]
    fn leaves_scoped_find_alone() {
        let (c, ok) = Prep::safe_find("find . -name Cargo.toml");
        assert!(!ok, "scoped find must not be rewritten");
        assert_eq!(c, "find . -name Cargo.toml");
    }

    #[test]
    fn parse_n_reads_count() {
        assert_eq!(parse_n("| tail -n 25", "tail"), 25);
        assert_eq!(parse_n("| head -n 5", "head"), 5);
        assert_eq!(parse_n("| tail", "tail"), 10);
    }

    #[test]
    fn strip_filter_drops_tail_stage() {
        assert_eq!(strip_filter("cargo build 2>&1 | tail -n 40"), "cargo build 2>&1");
        assert_eq!(strip_filter("ls | head -n 3"), "ls");
    }

    #[test]
    fn off_by_default_registers_no_tools() {
        std::env::remove_var("PIR_PREP");
        assert!(!Prep::new().enabled);
        assert!(Prep::new().specs().is_empty());
    }
}
