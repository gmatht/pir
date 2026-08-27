//! Project scaffolding helpers for the `/create` command.
//!
//! - Read the system clipboard (X11/Wayland/macOS best-effort).
//! - Detect whether clipboard text is a *project markdown spec* in the
//!   `unmd2.sh` format (a series of `### path` headers followed by fenced
//!   code blocks).
//! - Extract those blocks into real files on disk (a Rust port of unmd2.sh).

use std::io::Write;
use std::path::Path;
use std::process::Command;

/// Best-effort read of the system clipboard. Tries common CLIs in turn and
/// returns the first non-empty result, or `None` if unavailable.
pub fn read_clipboard() -> Option<String> {
    for cmdline in [
        "xclip -selection clipboard -o",
        "xsel -b -o",
        "wl-paste",
        "pbpaste",
    ] {
        let mut parts = cmdline.split_whitespace();
        let (Some(prog), args) = (parts.next(), parts.collect::<Vec<&str>>()) else {
            continue;
        };
        if let Ok(out) = Command::new(prog).args(&args).output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).into_owned();
                if !s.trim().is_empty() {
                    return Some(s);
                }
            }
        }
    }
    None
}

/// True when `text` looks like a project markdown spec: at least one
/// `### path` file header and at least one ``` code fence.
pub fn looks_like_project_md(text: &str) -> bool {
    let mut has_header = false;
    let mut has_fence = false;
    for line in text.lines() {
        let t = line.trim_start();
        if !has_header && t.starts_with("### ") {
            has_header = true;
        }
        if !has_fence && t.starts_with("```") {
            has_fence = true;
        }
        if has_header && has_fence {
            return true;
        }
    }
    false
}

/// Count the `### path` file headers — i.e. how many files a spec would
/// produce. Used only for the user-facing prompt.
pub fn count_md_files(text: &str) -> usize {
    text.lines()
        .filter(|l| l.trim_start().starts_with("### "))
        .count()
}

/// Write one extracted block to `dir/rel`. Creates parent directories.
fn write_block(dir: &Path, rel: &str, content: &str) -> Result<(), String> {
    if rel.is_empty() || content.is_empty() {
        return Ok(());
    }
    let target = dir.join(rel.trim().trim_start_matches('/'));
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let mut f = std::fs::File::create(&target)
        .map_err(|e| format!("create {}: {e}", target.display()))?;
    f.write_all(content.as_bytes())
        .map_err(|e| format!("write {}: {e}", target.display()))?;
    println!("  ✅ {}", target.display());
    Ok(())
}

/// Rust port of `unmd2.sh`: parse `### path` headers + ``` fenced code blocks
/// and write each block to `dir/<path>`. Returns the number of files written.
pub fn scaffold_from_md(dir: &Path, text: &str) -> Result<usize, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;

    let mut current_file = String::new();
    let mut content = String::new();
    let mut in_block = false;
    let mut written = 0usize;

    let mut flush = |file: &str, body: &str| -> Result<usize, String> {
        if !file.is_empty() && !body.is_empty() {
            write_block(dir, file, body)?;
            Ok(1)
        } else {
            Ok(0)
        }
    };

    for raw in text.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("### ") {
            // A new file header: flush any open block first (malformed input).
            if in_block {
                written += flush(&current_file, &content)?;
                in_block = false;
                content.clear();
            }
            current_file = rest.trim().to_string();
            continue;
        }
        if trimmed.starts_with("```") {
            if in_block {
                written += flush(&current_file, &content)?;
                in_block = false;
                content.clear();
                current_file.clear();
            } else {
                in_block = true;
                content.clear();
            }
            continue;
        }
        if in_block {
            if content.is_empty() {
                content = line.to_string();
            } else {
                content.push('\n');
                content.push_str(line);
            }
        }
    }
    // Trailing/truncated block.
    if in_block {
        written += flush(&current_file, &content)?;
    }

    Ok(written)
}
