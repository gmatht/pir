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

// ---------------------------------------------------------------------------
// Git guard: keep LLM-driven commits from bloating the repo with huge/binary
// files. Implemented as a `.git/hooks/pre-commit` (so it also guards *manual*
// `git commit`, not just the autocommit extension) plus a `/fix` command that
// makes the whole `.git` setup sane for LLM use. jj does NOT run git hooks, so
// under jj we skip the git hook and tell `/fix` to configure jj instead.
// ---------------------------------------------------------------------------

/// Which VCS a repo uses.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Vcs {
    Git,
    Jj,
}

/// Max bytes a single file may be when committing (overridable via
/// `PIR_COMMIT_MAX_BYTES`). 1 MiB keeps models/small binaries out by default.
pub fn commit_max_bytes() -> u64 {
    std::env::var("PIR_COMMIT_MAX_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(1_048_576)
}

/// Detect the active VCS for `repo` (the work-tree root).
pub fn detect_vcs(repo: &Path) -> Vcs {
    // Explicit override wins.
    if std::env::var("PIR_VCS").map(|v| v == "jj").unwrap_or(false) {
        return Vcs::Jj;
    }
    // A `.jj/` directory means this is a jj repo wrapping git.
    if repo.join(".jj").exists() {
        return Vcs::Jj;
    }
    // `jj` thinks the cwd is a repo (covers `jj init --git-repo .`).
    if Command::new("jj")
        .args(["root"])
        .current_dir(repo)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return Vcs::Jj;
    }
    Vcs::Git
}

/// True when `repo` is a git work tree.
pub fn is_git_repo(repo: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(repo)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The pre-commit hook source. Respects `git commit --no-verify` (git simply
/// does not invoke the hook), enforces a per-file byte ceiling, and refuses
/// files git classifies as binary. Threshold comes from `PIR_COMMIT_MAX_BYTES`.
fn guard_hook_script() -> String {
    const TEMPLATE: &str = r#"#!/bin/sh
# pir git guard — refuse to commit files that are too large or binary.
# Bypass for one commit with: git commit --no-verify
# Configure threshold with PIR_COMMIT_MAX_BYTES (bytes).
set -u
MAX=__MAX__
bad=0
while IFS= read -r f; do
  [ -z "$f" ] && continue
  if git cat-file -e ":$f" 2>/dev/null; then
    sz=$(git cat-file -s ":$f" 2>/dev/null || echo 0)
    case "$sz" in (*[!0-9]*) sz=0;; esac
    if [ "$sz" -gt "$MAX" ]; then
      echo "pir guard: '$f' is $sz bytes (> $MAX); commit aborted" >&2
      bad=1
    fi
    if git diff --cached --numstat -- "$f" 2>/dev/null | awk 'NF>=3 && $1=="-" && $2=="-"{f=1} END{exit !f}'; then
      echo "pir guard: '$f' looks binary; commit aborted" >&2
      bad=1
    fi
  fi
done <<EOF
$(git diff --cached --name-only --diff-filter=ACMR 2>/dev/null)
EOF
if [ "$bad" -ne 0 ]; then
  echo "pir guard: commit blocked. Use 'git commit --no-verify' to override once, or raise PIR_COMMIT_MAX_BYTES." >&2
  exit 1
fi
"#;
    TEMPLATE.replace("__MAX__", &commit_max_bytes().to_string())
}

/// Install the git guard pre-commit hook into `repo/.git/hooks/pre-commit`.
/// Idempotent: if a hook already exists and is identical it is left alone; if
/// a *different* hook exists we don't clobber it (return Ok(false) so the
/// caller can warn). Returns Ok(true) when a hook was written.
pub fn install_git_guard_hook(repo: &Path) -> Result<bool, String> {
    let hooks_dir = repo.join(".git").join("hooks");
    std::fs::create_dir_all(&hooks_dir).map_err(|e| format!("mkdir hooks: {e}"))?;
    let hook = hooks_dir.join("pre-commit");
    let script = guard_hook_script();
    if hook.exists() {
        let existing = std::fs::read_to_string(&hook).unwrap_or_default();
        if existing == script {
            return Ok(true); // already in place
        }
        return Ok(false); // a different hook is present; don't overwrite
    }
    std::fs::write(&hook, &script).map_err(|e| format!("write hook: {e}"))?;
    let _ = Command::new("chmod")
        .args(["+x", hook.to_string_lossy().as_ref()])
        .status();
    Ok(true)
}

/// True when `repo` is git and has no pir guard hook installed (so the startup
/// warning / `/fix` suggestion applies). Always false under jj (git hooks are
/// irrelevant there).
pub fn missing_git_guard(repo: &Path) -> bool {
    if detect_vcs(repo) == Vcs::Jj {
        return false;
    }
    if !is_git_repo(repo) {
        return false;
    }
    !repo.join(".git").join("hooks").join("pre-commit").exists()
}

/// Make the `.git` setup sane for LLM use. Under git: install the guard hook,
/// set `core.fsckObjects=true` (catch corrupt/unsafe objects) and
/// `core.quotepath=false` (readable non-ASCII paths in warnings), and add a
/// `.gitattributes` marking common binary extensions so `git` reports them as
/// binary. Under jj: git hooks don't run, so configure a jj `commit` hook
/// instead and tell the user git-hook tooling is bypassed.
pub fn fix_git_setup(repo: &Path) -> String {
    if detect_vcs(repo) == Vcs::Jj {
        return fix_jj_setup(repo);
    }
    let mut lines = Vec::new();
    match install_git_guard_hook(repo) {
        Ok(true) => lines.push("✓ installed .git/hooks/pre-commit guard (refuses > {} bytes / binary; bypass with --no-verify)".replace("{}", &commit_max_bytes().to_string())),
        Ok(false) => lines.push("• a pre-commit hook already exists; left it in place (not the pir guard). Run `git commit --no-verify` if needed, or replace it manually.".to_string()),
        Err(e) => lines.push(format!("✗ could not install guard hook: {e}")),
    }
    for (k, v) in [("core.fsckObjects", "true"), ("core.quotepath", "false")] {
        let _ = Command::new("git")
            .args(["config", k, v])
            .current_dir(repo)
            .status();
        lines.push(format!("✓ git config {k}={v}"));
    }
    // .gitattributes: mark obvious binary extensions so `git` treats them as
    // binary (the hook keys off git's binary detection).
    let attrs = repo.join(".gitattributes");
    let mut attr = String::from(
        "# pir: mark common binary extensions so git reports them as binary\n\
         *.bin binary\n*.exe binary\n*.dll binary\n*.so binary\n*.dylib binary\n\
         *.png binary\n*.jpg binary\n*.jpeg binary\n*.gif binary\n*.pdf binary\n\
         *.zip binary\n*.tar binary\n*.gz binary\n*.7z binary\n*.woff binary\n*.woff2 binary\n",
    );
    if attrs.exists() {
        if let Ok(existing) = std::fs::read_to_string(&attrs) {
            if existing.contains("pir: mark common binary") {
                attr = String::new(); // our block already present
            } else {
                attr = format!("\n{}", attr); // append
            }
        }
    }
    if !attr.is_empty() {
        let mut opts = std::fs::OpenOptions::new();
        if attrs.exists() {
            opts.append(true);
        } else {
            opts.create(true).write(true);
        }
        if let Ok(mut f) = opts.open(&attrs) {
            let _ = std::io::Write::write_all(&mut f, attr.as_bytes());
            lines.push("✓ added .gitattributes (binary extensions marked)".to_string());
        }
    }
    lines.join("\n")
}

/// jj doesn't invoke git hooks. Configure a jj `commit` hook (via repo-local
/// `jj config set --repo`) that runs the same size/binary guard. jj exposes the
/// change via `$JJ_REPO_PATH`; we approximate using `git` on the colocated repo.
fn fix_jj_setup(repo: &Path) -> String {
    let max = commit_max_bytes();
    // jj's `commit` hook runs a command; we reuse our guard logic via a small
    // inline script that checks staged files through the colocated git repo.
    let script = format!(
        "r='${{JJ_REPO_PATH:-.}}'; git -C \"$r\" diff --cached --name-only | while read -r f; do \
         s=$(git -C \"$r\" cat-file -s \":$f\" 2>/dev/null||echo 0); \
         if [ \"${{s:-0}}\" -gt {max} ]; then echo \"pir guard (jj): $f is $s bytes (> {max})\" >&2; exit 1; fi; \
         if git -C \"$r\" diff --cached --numstat -- \"$f\" | grep -q '^-\\t-\\t'; then echo \"pir guard (jj): $f looks binary\" >&2; exit 1; fi; \
         done"
    );
    let out = Command::new("jj")
        .args(["config", "set", "--repo", "hooks.commit.command", &script])
        .current_dir(repo)
        .output();
    match out {
        Ok(o) if o.status.success() => {
            format!(
                "✓ jj repo: git hooks don't run under jj, so installed a jj `commit` hook (refuses > {} bytes / binary).\n  Bypass for one change with: jj commit --no-commit-working-copy (or omit the hook).",
                max
            )
        }
        _ => format!(
            "✗ jj repo: could not set jj commit hook (is `jj` configured?). Under jj, git's .git/hooks/pre-commit is ignored, so a guard must be a jj hook."
        ),
    }
}
