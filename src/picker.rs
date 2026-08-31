//! Interactive session picker for `pir -r`.
//!
//! When `pir -r` is invoked and there is no session from the current shell,
//! instead of the old line-prompt (`resume one? [idx | y | n]`) we draw a
//! full-screen, two-pane terminal UI:
//!
//!   * left pane  — the list of candidate sessions (newest first), navigable
//!                  with the Up/Down arrows (or `j`/`k`); the highlighted row is
//!                  the selection;
//!   * right pane — a preview of the highlighted session: its first user
//!                  prompt, its last user prompt, and the tail of the model's
//!                  last thinking + response. It updates live as you move.
//!
//! Enter / Right resumes the highlighted session; `y` resumes the newest (index
//! 0) for convenience; `f`/`F` marks the highlighted session finished (so it
//! drops out of `/unfinished` and the picker itself) without resuming it;
//! `n` / Esc / ctrl-c / ctrl-d / `q` abort (start a fresh session).
//!
//! The picker runs on the **alternate screen** (via the shared `modal`
//! infrastructure) so it restores the normal screen's scrollback on exit, and
//! uses crossterm's event handling for arrows/resize instead of hand-rolled
//! `libc::poll` + SIGWINCH + escape parsing.

use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::time::SystemTime;

use crate::modal::{self, Key};
use crate::session::{read_preview, SessionPreview};
use crate::term;

/// A candidate row shown in the left pane.
pub struct PickItem {
    pub index: usize,
    pub name: String,
    pub shell_pid: u32,
    pub from_here: bool,
    pub mtime: SystemTime,
    pub path: std::path::PathBuf,
    pub preview_line: String,
    /// Reader-friendly conversation name from the background "light" model
    /// (e.g. `cerebras/gemma4`). Empty until generated (or when unavailable /
    /// throttled). The picker shows it when present, else the preview line.
    pub title: String,
    /// Coarse outcome of the last finished turn, classified by the light model
    /// into a short token (`complete` / `waiting` / `retry` / `blocked` /
    /// `error`). Empty until generated. Shown at the top of the preview so the
    /// user can tell at a glance whether a thread still needs them.
    pub verdict: String,
}

/// Outcome of the picker.
pub enum PickResult {
    /// Resume the session at this index (into the sorted candidate list).
    Resume(usize),
    /// Mark the session at this index finished (drop it out of `/unfinished`
    /// and the picker itself) without resuming it. Used by the `f`/`F` key.
    Finish(usize),
    /// Don't resume anything — start a fresh session.
    Cancel,
}

/// Run the interactive picker over `items` (already sorted newest-first, index
/// 0 = newest). Returns `PickResult::Cancel` when stdin is not a terminal or
/// the user bails out. Runs on the alternate screen; restores the normal screen
/// (with scrollback) on return.
pub fn pick_session(items: &[PickItem]) -> PickResult {
    if items.is_empty() || !io::stdin().is_terminal() {
        return PickResult::Cancel;
    }
    let mut selected = 0usize;
    // Previews are a per-file scan; cache them as the user visits rows.
    let mut preview_cache: HashMap<usize, SessionPreview> = HashMap::new();

    // Enter the alternate screen + raw mode (RAII: restores on drop, even on
    // panic). Returns None if not a tty — we already checked, so unwrap.
    let _modal = modal::Modal::enter().expect("stdin is a tty, so modal should enter");

    let result = loop {
        let preview = preview_cache
            .entry(selected)
            .or_insert_with(|| read_preview(&items[selected].path));
        draw(items, selected, preview);

        match modal::read_key() {
            Some(Key::Up) | Some(Key::Char('k')) => {
                if selected > 0 {
                    selected -= 1;
                }
            }
            Some(Key::Down) | Some(Key::Char('j')) => {
                if selected + 1 < items.len() {
                    selected += 1;
                }
            }
            Some(Key::Char('g')) => selected = 0,
            Some(Key::Char('G')) => selected = items.len().saturating_sub(1),
            Some(Key::PageUp) => selected = selected.saturating_sub(page_step(items.len())),
            Some(Key::PageDown) => selected = (selected + page_step(items.len())).min(items.len() - 1),
            Some(Key::Enter) | Some(Key::Right) => break PickResult::Resume(selected),
            Some(Key::Char('y')) => break PickResult::Resume(0), // newest, like the old `y=latest`
            Some(Key::Char('f')) | Some(Key::Char('F')) => break PickResult::Finish(selected), // mark finished, don't resume
            Some(Key::Char('n')) | Some(Key::Char('q')) | Some(Key::Esc) | Some(Key::CtrlC) | Some(Key::CtrlD) => {
                break PickResult::Cancel;
            }
            // keys without a picker action: ignore (Left = "back out" is
            // ambiguous with vim `h`, so keep it a no-op rather than quitting).
            Some(Key::Left) => {}
            // Terminal resized while idling: fall through so the loop redraws
            // with the new layout.
            Some(Key::Resize) => {}
            Some(Key::Char(c)) if c.is_ascii_digit() => {
                // Jump to the 1-based index if it exists.
                let n = (c as u32 as usize).saturating_sub(1);
                if n < items.len() {
                    selected = n;
                }
            }
            // Any other character: ignore (the picker only acts on the single
            // keys mapped above; free text is not an action).
            Some(Key::Char(_)) => {}
            // Other keys (Home/End/Tab/ctrl-n/ctrl-m/Other): ignore.
            Some(_) => {}
            // EOF / read error: cancel.
            None => break PickResult::Cancel,
        }
    };
    result
}

fn page_step(n: usize) -> usize {
    n.min(5)
}

/// Render the two panes for `selected`. The layout is recomputed on each draw
/// from the current terminal size so it works at any width/height. We draw the
/// whole block from the cursor, anchoring back to the top line afterward so the
/// next draw overwrites in place.
fn draw(items: &[PickItem], selected: usize, preview: &SessionPreview) {
    let w = term::terminal_width().max(40);
    let h = term::terminal_height().max(12);
    let list_w = (w / 2).min(48).max(20);
    let prev_w = (w.saturating_sub(list_w + 1)).max(20);

    let mut buf: Vec<u8> = Vec::new();

    // Clear the alternate screen and anchor to top-left.
    buf.extend_from_slice(b"\x1b[2J\x1b[1;1H");

    // Header line.
    buf.extend_from_slice(
        term::bold(
            "resume a session  ↑/↓ (or j/k) move · enter resume · y=latest · f=mark finished · n=skip\n",
        )
        .as_bytes(),
    );

    // How many list rows we can show (leave room for the header).
    let max_list = h.saturating_sub(2).max(1);
    let start = selected
        .saturating_sub(max_list / 2)
        .min(items.len().saturating_sub(max_list));
    let end = (start + max_list).min(items.len());

    // Left pane: the session list.
    let mut rows = 0usize;
    for idx in start..end {
        let it = &items[idx];
        let marker = if idx == selected { "▸" } else { " " };
        let mut label = it.name.replace("pir-", "").replace(".jsonl", "");
        let aw = list_w.saturating_sub(18).max(4);
        if term::visible_len(&label) > aw {
            label = term::clip(&label, aw);
        }
        let tag = format!("[sh{}]", it.shell_pid);
        let tag_s = if it.from_here {
            term::cyan(&tag)
        } else {
            term::dim(&tag)
        };
        let age = rel_time(it.mtime);
        let row: String = if idx == selected {
            format!(
                "{} {:<2} {:<labelw$} {} {}",
                term::cyan(marker),
                idx,
                label,
                tag_s,
                term::dim(&age),
                labelw = aw,
            )
        } else {
            format!(
                "{} {:<2} {:<labelw$} {} {}",
                marker,
                idx,
                term::dim(&label),
                tag_s,
                term::dim(&age),
                labelw = aw,
            )
        };
        buf.extend_from_slice(row.as_bytes());
        // Pad with spaces to list_w (row may contain ANSI, so pad by spaces).
        buf.extend_from_slice(" ".repeat(list_w.saturating_sub(term::visible_len(&row))).as_bytes());
        buf.extend_from_slice(b"\n");
        rows += 1;
    }
    while rows < max_list {
        buf.extend_from_slice(" ".repeat(list_w).as_bytes());
        buf.extend_from_slice(b"\n");
        rows += 1;
    }

    // Right pane: the preview, overlaid at column list_w+1, rows 2.. .
    let mut pcol: Vec<String> = Vec::new();
    pcol.push(term::bold("preview").to_string());
    // Show the generated conversation title at the top of the preview when the
    // background "light" model has produced one (empty otherwise); this is the
    // same title `list_sessions` prints, kept in sync so both views agree.
    if !items[selected].title.is_empty() {
        pcol.push(term::green(&items[selected].title));
    }    // Show the turn-outcome verdict (classified by the same light model) on
    // its own line when present, so the user can tell at a glance whether a
    // thread still needs them. Same coloring convention as `list_sessions`.
    let v = crate::titler::verdict_label(&items[selected].verdict);
    if !v.is_empty() {
        let v_s = if v == "complete" {
            term::green(v)
        } else if v == "waiting for input" || v == "needs retry" {
            term::yellow(v)
        } else {
            term::red(v)
        };
        pcol.push(format!("  [{}]", v_s));
    }
    if preview.turns == 0 {
        pcol.push(term::dim("(empty session — no prompts yet)").to_string());
    } else {
        pcol.push(format!("{} {} turn(s)", term::dim("·"), preview.turns));
        if !preview.first_prompt.is_empty() {
            pcol.push(format!(
                "{} first: {}",
                term::dim("·"),
                term::dim(&term::clip(&first_line(&preview.first_prompt), prev_w.saturating_sub(8)))
            ));
        }
        if !preview.last_prompt.is_empty() && preview.last_prompt != preview.first_prompt {
            pcol.push(format!(
                "{} last:  {}",
                term::dim("·"),
                term::dim(&term::clip(&first_line(&preview.last_prompt), prev_w.saturating_sub(8)))
            ));
        }
        if !preview.last_thinking.is_empty() {
            pcol.push(term::dim("thinking (tail):").to_string());
            for l in wrap_lines(tail_lines(&preview.last_thinking, 6), prev_w.saturating_sub(2)) {
                pcol.push(term::dim(&l));
            }
        }
        if !preview.last_output.is_empty() {
            pcol.push(term::dim("response (tail):").to_string());
            for l in wrap_lines(tail_lines(&preview.last_output, 12), prev_w.saturating_sub(2)) {
                pcol.push(l);
            }
        }
    }

    let mut prow = 2usize; // 1-based; header is row 1
    for line in pcol {
        if prow > h {
            break;
        }
        buf.extend_from_slice(format!("\x1b[{};{}H", prow, list_w + 1).as_bytes());
        buf.extend_from_slice(term::clip(&line, prev_w).as_bytes());
        buf.extend_from_slice(b"\x1b[K");
        prow += 1;
    }

    // Move cursor back to top-left so the next tick can erase the block.
    buf.extend_from_slice(b"\x1b[1;1H");

    let _ = io::stdout().write_all(&buf);
    let _ = io::stdout().flush();
}

/// First non-empty line of `s`.
fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default()
        .to_string()
}

/// Last `n` lines of `s` (trimming each), as a `Vec<String>`.
fn tail_lines(s: &str, n: usize) -> Vec<String> {
    let lines: Vec<String> = s.lines().map(|l| l.trim().to_string()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].to_vec()
}

/// Hard-wrap each input line to at most `width` visible chars (no mid-word
/// split when possible) so wide thinking/response text doesn't overflow the
/// preview pane.
fn wrap_lines(lines: Vec<String>, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for l in lines {
        let l = l.trim_start();
        if width == 0 {
            out.push(l.to_string());
            continue;
        }
        let mut cur = String::new();
        for word in l.split(' ') {
            if cur.is_empty() {
                cur = word.to_string();
            } else if term::visible_len(&cur) + 1 + term::visible_len(word) <= width {
                cur.push(' ');
                cur.push_str(word);
            } else {
                out.push(std::mem::take(&mut cur));
                cur = word.to_string();
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

fn rel_time(t: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < 3600 {
        return format!("{}m", secs / 60);
    }
    if secs < 86_400 {
        return format!("{}h", secs / 3600);
    }
    format!("{}d", secs / 86_400)
}

