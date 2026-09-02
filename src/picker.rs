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
//! 0) for convenience; `n` / Esc / ctrl-c / ctrl-d / `q` abort (start a fresh
//! session). This is a hand-rolled raw-mode renderer (no ratatui dependency —
//! it must work in the default build), drawing into the terminal and restoring
//! it on exit.

use std::io::{self, Write};
#[cfg(unix)]
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use crate::session::SessionPreview;
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
}

/// Outcome of the picker.
pub enum PickResult {
    /// Resume the session at this index (into the sorted candidate list).
    Resume(usize),
    /// Don't resume anything — start a fresh session.
    Cancel,
}

/// Set by the SIGWINCH handler so `wait_key` can wake up (and the loop can
/// redraw with the new layout) when the terminal is resized while idling.
static RESIZED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_sig: i32) {
    RESIZED.store(true, Ordering::SeqCst);
}

/// Run the interactive picker over `items` (already sorted newest-first, index
/// 0 = newest). Returns `PickResult::Cancel` when stdin is not a terminal or
/// the user bails out. Blocks in raw mode for the duration; restores the
/// terminal on return.
#[cfg(unix)]
pub fn pick_session(items: &[PickItem]) -> PickResult {
    if items.is_empty() || !io::stdin().is_terminal() {
        return PickResult::Cancel;
    }
    let mut selected = 0usize;
    // Previews are a per-file scan; cache them as the user visits rows.
    let mut preview_cache: HashMap<usize, SessionPreview> = HashMap::new();

    // Enter raw mode (separate save slot from the REPL's running-turn raw) and
    // hide the cursor while we draw.
    term::raw::enable_raw_picker();
    RESIZED.store(false, Ordering::SeqCst);
    // Wake the (blocking) key wait when the terminal is resized so the
    // layout is redrawn; restore the default (ignore) on exit.
    unsafe { libc::signal(libc::SIGWINCH, on_winch as *const () as libc::sighandler_t) };
    let _ = io::stdout().write_all(b"\x1b[?25l");
    let _ = io::stdout().flush();

    // How many rows we drew so we can erase exactly that block later.
    let mut drawn_rows: usize = 0;

    let result = loop {
        let preview = preview_cache
            .entry(selected)
            .or_insert_with(|| read_preview(&items[selected].path));
        draw(items, selected, preview, &mut drawn_rows);

        match wait_key() {
            Key::Up | Key::Char('k') => {
                if selected > 0 {
                    selected -= 1;
                }
            }
            Key::Down | Key::Char('j') => {
                if selected + 1 < items.len() {
                    selected += 1;
                }
            }
            Key::Char('g') => selected = 0,
            Key::Char('G') => selected = items.len().saturating_sub(1),
            Key::PageUp => selected = selected.saturating_sub(page_step(items.len())),
            Key::PageDown => selected = (selected + page_step(items.len())).min(items.len() - 1),
            Key::Enter | Key::Right => break PickResult::Resume(selected),
            Key::Char('y') => break PickResult::Resume(0), // newest, like the old `y=latest`
            Key::Char('n') | Key::Char('q') | Key::Esc | Key::CtrlC | Key::CtrlD => {
                break PickResult::Cancel;
            }
            // keys without a picker action: ignore (Left = "back out" is
            // ambiguous with vim `h`, so keep it a no-op rather than quitting).
            Key::Left => {}
            // Terminal resized while idling: fall through so the loop redraws
            // with the new layout.
            Key::Resize => {}
            Key::Char(c) if c.is_ascii_digit() => {
                // Jump to the 1-based index if it exists.
                let n = (c as u32 as usize).saturating_sub(1);
                if n < items.len() {
                    selected = n;
                }
            }
            // Any other character: ignore (the picker only acts on the single
            // keys mapped above; free text is not an action).
            Key::Char(_) => {}
            // Spurious wake (e.g. EINTR without a resize flag): just redraw.
            Key::None => {}
        }
    };

    // Erase the drawn block and restore the cursor.
    let _ = io::stdout().write_all(format!("\r\x1b[{}A\x1b[J\x1b[?25h", drawn_rows).as_bytes());
    let _ = io::stdout().flush();
    term::raw::disable_raw_picker();
    unsafe { libc::signal(libc::SIGWINCH, libc::SIG_DFL) };
    result
}

fn page_step(n: usize) -> usize {
    n.min(5)
}

/// Render the two panes for `selected`. The layout is recomputed on each draw
/// from the current terminal size so it works at any width/height. We draw the
/// whole block from the cursor, anchoring back to the top line afterward so the
/// next draw overwrites in place..
fn draw(items: &[PickItem], selected: usize, preview: &SessionPreview, drawn_rows: &mut usize) {
    let w = term::terminal_width().max(40);
    let h = term::terminal_height().max(12);
    let list_w = (w / 2).min(48).max(20);
    let prev_w = (w.saturating_sub(list_w + 1)).max(20);

    let mut buf: Vec<u8> = Vec::new();

    // Anchor to the top of the block: move up `drawn_rows` from the previous
    // tick (0 on the first), then erase the whole block downward.
    if *drawn_rows > 0 {
        buf.extend_from_slice(format!("\r\x1b[{}A", *drawn_rows).as_bytes());
    }
    buf.extend_from_slice(b"\x1b[J");

    // Header line.
    buf.extend_from_slice(
        term::bold(
            "resume a session  ↑/↓ (or j/k) move · enter resume · y=latest · n=skip\n",
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
    *drawn_rows = h; // the whole terminal is ours for the duration
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

/// A single keypress (or control key) read in raw mode.
#[derive(Debug)]
enum Key {
    Up,
    Down,
    Right,
    Left,
    Enter,
    Esc,
    CtrlC,
    CtrlD,
    PageUp,
    PageDown,
    Resize,
    Char(char),
    None,
}

/// Block until a keypress is available (or the terminal is resized),
/// translating arrow keys / control sequences via the shared CSI-aware
/// `term::raw::translate_picker`. This is what lets the picker idle instead of
/// spinning: `poll()` parks the process with zero wakeups until stdin is
/// readable or SIGWINCH interrupts it (EINTR), so no CPU is burned and no
/// repaint happens until there is something to show.
#[cfg(unix)]
fn wait_key() -> Key {
    let fd = io::stdin().as_raw_fd();
    if RESIZED.swap(false, Ordering::SeqCst) {
        // A resize may have landed while we were processing the last key.
        return Key::Resize;
    }
    loop {
        let mut pfd = [libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        }];
        let r = unsafe { libc::poll(pfd.as_mut_ptr(), 1, -1) };
        if r < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                // EINTR: either SIGWINCH (flag set) or a stray signal.
                return if RESIZED.swap(false, Ordering::SeqCst) {
                    Key::Resize
                } else {
                    Key::None
                };
            }
            // Real poll error: fall back to a redraw-and-retry cycle.
            return Key::None;
        }
        if r == 0 {
            continue; // can't happen with an infinite timeout
        }
        break; // stdin is readable
    }

    let mut tmp = [0u8; 64];
    let r = unsafe { libc::read(fd, tmp.as_mut_ptr() as *mut libc::c_void, tmp.len()) };
    if r <= 0 {
        if r == 0 {
            return Key::CtrlD; // stdin closed
        }
        // Read interrupted (EINTR, e.g. a resize during read): retry loop.
        return Key::None;
    }
    let bytes = &tmp[..r as usize];
    // The shared `translate` swallows CSI sequences (arrows, Home/End, F-keys)
    // and reports them as `RawInput::None`, so we detect the movement keys we
    // care about *here* from the raw bytes before falling back to it. Handles
    // both the standard `ESC [ <letter>` form and the VT100 `ESC O <letter>`
    // application-cursor form, plus the page-up/page-down `ESC [ 5~`/`6~`.
    if bytes.len() >= 3 && (bytes[0] == 0x1b) && (bytes[1] == 0x5b || bytes[1] == 0x4f) {
        return match bytes[2] {
            b'A' => Key::Up,
            b'B' => Key::Down,
            b'C' => Key::Right,
            b'D' => Key::Left,
            _ => Key::None,
        };
    }
    if bytes.len() >= 4 && bytes[0] == 0x1b && bytes[1] == 0x5b && bytes[3] == b'~' {
        return match bytes[2] {
            b'5' => Key::PageUp,
            b'6' => Key::PageDown,
            _ => Key::None,
        };
    }
    let mut buf = String::new();
    let ta: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let res = term::raw::translate_picker(&mut buf, &ta, bytes);
    translate_result(&res, &buf)
}

/// Map a `term::raw::RawInput` outcome (plus the typed line if any) into our
/// `Key` enum. The picker ignores free text except for the single keys
/// `y`/`n`/`q`/`g`/`G` and digits, so a finished line is reported via its first
/// non-whitespace character.
fn translate_result(res: &term::raw::RawInput, buf: &str) -> Key {
    use term::raw::RawInput;
    match res {
        RawInput::Line(s) => {
            let s = s.trim();
            if s.is_empty() {
                Key::Enter
            } else if let Some(c) = s.chars().next() {
                Key::Char(c)
            } else {
                Key::None
            }
        }
        RawInput::Interrupt => Key::CtrlC,
        RawInput::Cancel => Key::Esc,
        RawInput::Eof => Key::CtrlD,
        RawInput::Suspend => Key::CtrlC, // treat ctrl-z like cancel for the picker
        RawInput::Quit => Key::CtrlD,    // ctrl-q: leave the picker (quit the app from the REPL)
        RawInput::None => {
            let _ = buf;
            Key::None
        }
        _ => Key::None,
    }
}

#[cfg(not(unix))]
pub fn pick_session(_items: &[PickItem]) -> PickResult { PickResult::Resume(0) }
