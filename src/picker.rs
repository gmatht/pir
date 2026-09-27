//! Interactive session picker for `pir -r`.
//!
//! When `pir -r` is invoked and there is no session from the current shell,
//! instead of the old line-prompt (`resume one? [idx | y | n]`) we draw a
//! full-screen, two-pane terminal UI:
//!
//!   * left pane  — the list of candidate sessions (newest first), navigable
//!     with the Up/Down arrows (or `j`/`k`); the highlighted row is the
//!     selection;
//!   * right pane — a preview of the highlighted session: its first user
//!     prompt, its last user prompt, and the tail of the model's
//!     last thinking + response. It updates live as you move.
//!
//! Enter / Right resumes the highlighted session; `y` resumes the newest (index
//! 0) for convenience; `n` / Esc / ctrl-c / ctrl-d / `q` abort (start a fresh
//! session). This is a hand-rolled raw-mode renderer (no ratatui dependency —
//! it must work in the default build), drawing into the terminal and restoring
//! it on exit.

use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::session::{read_preview, SessionPreview};
use crate::term;

/// A candidate row shown in the left pane.
#[derive(Clone, Debug)]
pub struct PickItem {
    pub index: usize,
    pub name: String,
    pub shell_pid: u32,
    /// Controlling terminal the session was started from (`pts/7`), or empty
    /// when unknown. Shown in preference to the pid, which is recycled.
    pub tty: String,
    pub from_here: bool,
    pub mtime: SystemTime,
    pub path: std::path::PathBuf,
    pub preview_line: String,
}

/// Outcome of the picker.
pub enum PickResult {
    /// Resume the session at this index (into the sorted candidate list).
    Resume(usize),
    /// Rebuild a whole zellij session instead of resuming one transcript: the
    /// operator picked one of the zellij sessions offered as a trailing row
    /// (see `PickItem::Zellij`).
    RebuildZellij(String),
    /// Don't resume anything — start a fresh session.
    Cancel,
}

/// A row in the left pane: either a pir session to resume, or — when the
/// operator has pir sessions recorded inside zellij — a zellij session that can
/// be rebuilt whole.
pub enum PickRow {
    /// A pir transcript.
    Session(PickItem),
    /// A zellij session to rebuild, with the number of recorded pir panes in it.
    Zellij { session: String, panes: usize },
}

impl PickRow {
    fn as_session(&self) -> Option<&PickItem> {
        match self {
            PickRow::Session(i) => Some(i),
            PickRow::Zellij { .. } => None,
        }
    }
}

/// Set by the SIGWINCH handler so `wait_key` can wake up (and the loop can
/// redraw with the new layout) when the terminal is resized while idling.
///
/// Unix-only: the handler is installed with `libc::signal`, and every reader
/// lives in the `#[cfg(unix)]` picker. Gating the static (rather than the `use`)
/// keeps `AtomicBool`/`Ordering` out of the non-unix build entirely.
#[cfg(unix)]
static RESIZED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_winch(_sig: i32) {
    RESIZED.store(true, Ordering::SeqCst);
}

/// Background preview prefetcher.
///
/// The picker paints synchronously and `read_preview` parses a whole transcript
/// (tens of ms for a multi-MB session), so scanning a row on demand stalls the
/// UI for exactly as long as that scan takes. This worker fills the cache ahead
/// of the user instead: it scans rows in a priority order that follows the
/// selection, so the preview is already warm by the time a row is highlighted.
///
/// Priority is a *hint*, not a contract — the drawing path still computes any
/// missing preview synchronously, so a wrong guess costs nothing but the work
/// already done.
///
/// Unix-only: it builds its notification pipe with `libc::pipe` and is driven by
/// the `#[cfg(unix)]` picker loop, so nothing outside that path can reach it.
#[cfg(unix)]
struct Prefetcher {
    /// Row index -> finished preview. Shared with the UI thread.
    cache: Arc<Mutex<HashMap<usize, SessionPreview>>>,
    /// Rows the worker should scan, nearest-to-selection first. The UI replaces
    /// this on every selection change; the worker re-reads it after each row so
    /// a new selection takes effect promptly.
    queue: Arc<Mutex<Vec<usize>>>,
    /// Set on drop so the worker stops instead of scanning every remaining
    /// transcript after the user has already resumed a session.
    stop: Arc<AtomicBool>,
    /// Write end of the notification pipe. The worker writes one byte per
    /// published preview, waking the UI's `poll` immediately.
    ///
    /// A self-pipe rather than a timed poll: the picker already parks in
    /// `poll(stdin, -1)` for zero idle CPU, and a polling timeout would trade
    /// that away (a 50ms tick measures ~0.24% of a core) to learn about
    /// something we can simply be told. `None` if the pipe could not be created,
    /// in which case the UI falls back to a bounded poll timeout.
    notify_tx: Option<Arc<std::fs::File>>,
    /// Read end of that pipe, polled alongside stdin.
    notify_rx: Option<Arc<std::fs::File>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl Prefetcher {
    /// Spawn a worker over `paths`, starting at `selected`.
    fn spawn(paths: Vec<std::path::PathBuf>, selected: usize) -> Prefetcher {
        let cache: Arc<Mutex<HashMap<usize, SessionPreview>>> = Arc::new(Mutex::new(HashMap::new()));
        let queue: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(order_from(&paths, selected)));
        let stop = Arc::new(AtomicBool::new(false));

        // Notification pipe: worker writes, UI reads. Non-blocking on the write
        // side so a saturated pipe (many previews, no reads) can never wedge the
        // worker — a dropped notification only costs one late repaint, and the
        // next one wakes the UI anyway.
        let notify_tx;
        let notify_rx;
        #[cfg(unix)]
        {
            use std::os::unix::io::FromRawFd;
            let mut fds = [0i32; 2];
            if unsafe { libc::pipe(fds.as_mut_ptr()) } == 0 {
                unsafe {
                    let flags = libc::fcntl(fds[1], libc::F_GETFL);
                    let _ = libc::fcntl(fds[1], libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
                notify_tx = Some(Arc::new(unsafe { std::fs::File::from_raw_fd(fds[1]) }));
                notify_rx = Some(Arc::new(unsafe { std::fs::File::from_raw_fd(fds[0]) }));
            } else {
                notify_tx = None;
                notify_rx = None;
            }
        }
        #[cfg(not(unix))]
        {
            notify_tx = None;
            notify_rx = None;
        }

        let handle = {
            let cache = Arc::clone(&cache);
            let queue = Arc::clone(&queue);
            let stop = Arc::clone(&stop);
            let notify = notify_tx.clone();
            let paths = Arc::new(paths);
            std::thread::Builder::new()
                .name("pir-preview-prefetch".to_string())
                .spawn(move || {
                    use std::io::Write;
                    while !stop.load(Ordering::SeqCst) {
                        // Take the next row the UI cares about. Popping here (vs
                        // iterating a snapshot) is what lets a fresh selection
                        // preempt the remainder of the old one.
                        let next = queue.lock().ok().and_then(|mut q| q.pop());
                        let Some(idx) = next else { break };
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        // Skip if the UI already produced it (raced us).
                        let already = cache.lock().map(|c| c.contains_key(&idx)).unwrap_or(true);
                        if already {
                            continue;
                        }
                        let Some(path) = paths.get(idx) else { continue };
                        let preview = read_preview(path);
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        if let Ok(mut c) = cache.lock() {
                            c.insert(idx, preview);
                        }
                        // Wake the UI: it is parked in poll(stdin) and would
                        // otherwise not repaint until the next keystroke.
                        if let Some(n) = &notify
                            && let Ok(mut f) = n.try_clone()
                        {
                            let _ = f.write_all(b"p");
                        }
                    }
                })
                .ok()
        };
        Prefetcher { cache, queue, stop, notify_tx, notify_rx, handle }
    }

    /// Re-target the worker at a new selection: scan `selected` first, then the
    /// rows just below it, then the rows just above. Replaces any pending work
    /// so a held-down arrow key does not queue up every row it passed through.
    fn retarget(&self, paths_len: usize, selected: usize) {
        if let Ok(mut q) = self.queue.lock() {
            *q = order_from_len(paths_len, selected);
        }
    }

    /// A finished preview for `idx`, if the worker (or a previous synchronous
    /// scan) has one. Does not block.
    fn get(&self, idx: usize) -> Option<SessionPreview> {
        self.cache.lock().ok()?.get(&idx).cloned()
    }

    /// Publish a preview computed on the UI thread so the worker skips it.
    fn put(&self, idx: usize, preview: SessionPreview) {
        if let Ok(mut c) = self.cache.lock() {
            c.insert(idx, preview);
        }
    }
}

#[cfg(unix)]
impl Drop for Prefetcher {
    fn drop(&mut self) {
        // Abandon any remaining rows: the user has picked (or cancelled), so
        // scanning the rest of a 25-session backlog would only burn I/O.
        self.stop.store(true, Ordering::SeqCst);
        // Empty the queue too, so a worker blocked on `lock()` sees the stop
        // flag at its next check rather than draining the backlog first.
        if let Ok(mut q) = self.queue.lock() {
            q.clear();
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Scan order for `selected`: the highlighted row first, then outward (down
/// before up, matching where an arrow key is most likely to go next).
#[cfg(unix)]
fn order_from(paths: &[std::path::PathBuf], selected: usize) -> Vec<usize> {
    order_from_len(paths.len(), selected)
}

#[cfg(unix)]
fn order_from_len(len: usize, selected: usize) -> Vec<usize> {
    // Build the desired *scan* order explicitly, then reverse it: the queue is
    // consumed with `pop()` (last element first). Priority is
    // selected, +1, -1, +2, -2, … so the row the user is looking at is scanned
    // first and the likely next move (Down) precedes the unlikely one (Up).
    let mut scan: Vec<usize> = Vec::with_capacity(len + 1);
    scan.push(selected);
    let mut d = 1usize;
    while selected + d < len || d <= selected {
        if selected + d < len {
            scan.push(selected + d); // below first (Down is the common move)
        }
        if d <= selected {
            scan.push(selected - d);
        }
        d += 1;
    }
    scan.reverse();
    scan
}

/// Run the interactive picker over `items` (already sorted newest-first, index
/// 0 = newest), optionally offering whole-zellij-session rebuilds as trailing
/// rows. Returns `PickResult::Cancel` when stdin is not a terminal or the user
/// bails out. Blocks in raw mode for the duration; restores the terminal on
/// return.
#[cfg(unix)]
pub fn pick_session(items: &[PickItem]) -> PickResult {
    pick_session_with_zellij(items, &[])
}

/// [`pick_session`], plus `zellij_sessions` (`(session, recorded pir panes)`)
/// offered as extra rows after the transcripts. Selecting one yields
/// [`PickResult::RebuildZellij`] rather than a resume.
#[cfg(unix)]
pub fn pick_session_with_zellij(
    items: &[PickItem],
    zellij_sessions: &[(String, usize)],
) -> PickResult {
    if items.is_empty() || !io::stdin().is_terminal() {
        return PickResult::Cancel;
    }
    // Transcripts first (newest first, as the caller sorted them), then any
    // rebuildable zellij sessions.
    let mut rows: Vec<PickRow> = items.iter().cloned().map(PickRow::Session).collect();
    rows.extend(zellij_sessions.iter().map(|(session, panes)| PickRow::Zellij {
        session: session.clone(),
        panes: *panes,
    }));
    if rows.is_empty() {
        return PickResult::Cancel;
    }
    let mut selected = 0usize;
    // Previews are a per-file scan; the worker fills this ahead of the user and
    // the draw path falls back to a synchronous scan for anything still cold.
    let paths: Vec<std::path::PathBuf> =
        rows.iter().filter_map(PickRow::as_session).map(|i| i.path.clone()).collect();
    let prefetch = Prefetcher::spawn(paths, selected);

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
        // Prefer a prefetched preview; fall back to computing it here so the
        // first paint after launch (or a fast scroll past the worker) is never
        // blank. Only a cold cache pays the scan. A zellij row has no
        // transcript, so it takes a static preview instead.
        let preview = match rows[selected].as_session() {
            Some(item) => match prefetch.get(selected) {
                Some(p) => p,
                None => {
                    let p = read_preview(&item.path);
                    prefetch.put(selected, p.clone());
                    p
                }
            },
            None => zellij_preview(&rows[selected]),
        };
        draw(&rows, selected, &preview, &mut drawn_rows);

        // Re-target the worker at wherever the selection now is, so its next
        // scans are the rows the user is about to look at rather than whatever
        // was queued when the picker opened.
        prefetch.retarget(rows.len(), selected);

        match wait_key(prefetch.notify_rx.as_deref()) {
            Key::Up | Key::Char('k') => {
                selected = selected.saturating_sub(1);
            }
            Key::Down | Key::Char('j') => {
                if selected + 1 < rows.len() {
                    selected += 1;
                }
            }
            Key::Char('g') => selected = 0,
            Key::Char('G') => selected = rows.len().saturating_sub(1),
            Key::PageUp => selected = selected.saturating_sub(page_step(rows.len())),
            Key::PageDown => selected = (selected + page_step(rows.len())).min(rows.len() - 1),
            // Enter/Right commit the highlighted row: a transcript resumes, a
            // zellij row rebuilds the whole session.
            Key::Enter | Key::Right => break match &rows[selected] {
                PickRow::Zellij { session, .. } => PickResult::RebuildZellij(session.clone()),
                PickRow::Session(_) => PickResult::Resume(selected),
            },
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
            // A prefetched preview landed: fall through and redraw with it.
            Key::Repaint => {}
            Key::Char(c) if c.is_ascii_digit() => {
                // Jump to the 1-based index if it exists.
                let n = (c as u32 as usize).saturating_sub(1);
                if n < rows.len() {
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

/// The preview shown for a zellij row: there is no transcript, so explain what
/// selecting it will do rather than showing stale/empty session text.
fn zellij_preview(row: &PickRow) -> SessionPreview {
    let PickRow::Zellij { session, panes } = row else {
        return SessionPreview {
            turns: 0,
            first_prompt: String::new(),
            last_prompt: String::new(),
            last_thinking: String::new(),
            last_output: String::new(),
        };
    };
    SessionPreview {
        turns: 0,
        first_prompt: format!("rebuild zellij session '{}'", session),
        last_prompt: format!(
            "{panes} pir pane(s) recorded in this session will be restored to their own conversation"
        ),
        last_thinking: String::new(),
        last_output: String::new(),
    }
}

/// Render the two panes for `selected`. The layout is recomputed on each draw
/// from the current terminal size so it works at any width/height. We draw the
/// whole block from the cursor, anchoring back to the top line afterward so the
/// next draw overwrites in place..
fn draw(items: &[PickRow], selected: usize, preview: &SessionPreview, drawn_rows: &mut usize) {
    let w = term::terminal_width().max(40);
    let h = term::terminal_height().max(12);
    let list_w = (w / 2).clamp(20, 48);
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

    // Left pane: the session list, plus any zellij rebuild rows.
    let mut rows = 0usize;
    for (idx, item) in items.iter().enumerate().take(end).skip(start) {
        let marker = if idx == selected { "▸" } else { " " };
        let aw = list_w.saturating_sub(18).max(4);
        // A zellij row reads as a distinct kind of action, not a transcript:
        // mark it so `enter` is not mistaken for "resume this transcript".
        let row: String = match item {
            PickRow::Zellij { session, panes } => {
                let mut label = format!("@{}  (rebuild session)", session);
                if term::visible_len(&label) > aw {
                    label = term::clip(&label, aw);
                }
                let marker_s = if idx == selected { term::yellow(marker) } else { marker.to_string() };
                let label_s = if idx == selected {
                    term::yellow(&label)
                } else {
                    term::dim(&label)
                };
                format!(
                    "{} {:<2} {:<labelw$} {} {}",
                    marker_s,
                    idx,
                    label_s,
                    term::dim(&format!("[{panes} pane(s)]")),
                    String::new(),
                    labelw = aw,
                )
            }
            PickRow::Session(it) => {
                let mut label = it.name.replace("pir-", "").replace(".jsonl", "");
                if term::visible_len(&label) > aw {
                    label = term::clip(&label, aw);
                }
                // Prefer the tty: it is the stable identity. Fall back to the pid for
                // sessions recorded before the tty was tracked, or detached launches.
                let tag = if it.tty.is_empty() {
                    format!("[sh{}]", it.shell_pid)
                } else {
                    format!("[{}]", it.tty)
                };
                let tag_s = if it.from_here {
                    term::cyan(&tag)
                } else {
                    term::dim(&tag)
                };
                let age = rel_time(it.mtime);
                if idx == selected {
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
                }
            }
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

    // 1-based; header is row 1.
    for (prow, line) in (2usize..).zip(pcol) {
        if prow > h {
            break;
        }
        buf.extend_from_slice(format!("\x1b[{};{}H", prow, list_w + 1).as_bytes());
        buf.extend_from_slice(term::clip(&line, prev_w).as_bytes());
        buf.extend_from_slice(b"\x1b[K");
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
    /// Not a keypress: the background prefetcher published a preview and the
    /// pane should be redrawn. Handled as a no-op by the picker loop (which
    /// redraws unconditionally each iteration).
    Repaint,
    Char(char),
    None,
}

/// Block until a keypress is available, the terminal is resized, or the
/// prefetcher has a preview ready, translating arrow keys / control sequences
/// via the shared CSI-aware `term::raw::translate_picker`. This is what lets
/// the picker idle instead of spinning: `poll()` parks the process with **no
/// timeout** until stdin is readable, SIGWINCH interrupts it (EINTR), or the
/// worker writes to its notification pipe — so no CPU is burned and no repaint
/// happens until there is something to show.
#[cfg(unix)]
fn wait_key(notify_rx: Option<&std::fs::File>) -> Key {
    use std::os::unix::io::AsRawFd as _;
    let fd = io::stdin().as_raw_fd();
    if RESIZED.swap(false, Ordering::SeqCst) {
        // A resize may have landed while we were processing the last key.
        return Key::Resize;
    }
    loop {
        // Park on stdin AND the prefetcher's notification pipe. With no timeout
        // at all, the picker keeps its zero-idle-CPU property: it wakes only for
        // a real keypress, a resize, or a freshly prefetched preview.
        let mut pfd = [
            libc::pollfd { fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd {
                fd: notify_rx.map(|f| f.as_raw_fd()).unwrap_or(-1),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let r = unsafe { libc::poll(pfd.as_mut_ptr(), 2, -1) };
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
        // A prefetched preview landed before (or instead of) a keypress: drain
        // the notification byte and repaint.
        if pfd[1].revents & libc::POLLIN != 0 {
            if let Some(f) = notify_rx {
                let mut b = [0u8; 64];
                let n = unsafe {
                    libc::read(
                        f.as_raw_fd(),
                        b.as_mut_ptr() as *mut libc::c_void,
                        b.len(),
                    )
                };
                let _ = n; // drain; contents are irrelevant
                // If the user typed at the same moment, prefer the keystroke
                // (fall through) — but only redraw now if stdin is quiet.
                if pfd[0].revents & libc::POLLIN == 0 {
                    return Key::Repaint;
                }
            } else {
                return Key::Repaint;
            }
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
        // Windows byte-path outcomes (unreachable via wait_input, which
        // accumulates into its buffer): map the keypresses the picker
        // understands, ignore the rest.
        #[cfg(not(unix))]
        RawInput::Char(c) => Key::Char(*c),
        #[cfg(not(unix))]
        RawInput::Enter => Key::Enter,
        #[cfg(not(unix))]
        RawInput::Tab
        | RawInput::Up
        | RawInput::Down
        | RawInput::Left
        | RawInput::Right
        | RawInput::Resize
        | RawInput::Other(_) => Key::None,
        #[cfg(not(unix))]
        RawInput::Paste(s) => s
            .chars()
            .find(|c| !c.is_whitespace())
            .map(Key::Char)
            .unwrap_or(Key::None),
    }
}

/// Non-unix stub: the raw-mode picker is unix-only, so this just reports "no
/// choice" and lets the caller fall back to its own prompt. It must exist for
/// the same reason as [`pick_session`] — `main` calls the zellij-aware variant
/// unconditionally, so a unix-only definition would not compile for Windows.
#[cfg(not(unix))]
pub fn pick_session_with_zellij(
    _items: &[PickItem],
    _zellij_sessions: &[(String, usize)],
) -> PickResult {
    PickResult::Cancel
}

#[cfg(not(unix))]
pub fn pick_session(_items: &[PickItem]) -> PickResult { PickResult::Resume(0) }

#[cfg(test)]
mod row_tests {
    use super::*;

    fn item(name: &str) -> PickItem {
        PickItem {
            index: 0,
            name: name.to_string(),
            shell_pid: 1,
            tty: "pts/0".into(),
            from_here: false,
            mtime: std::time::SystemTime::now(),
            path: std::path::PathBuf::from(name),
            preview_line: String::new(),
        }
    }

    fn rows(n_sessions: usize, zellij: &[(&str, usize)]) -> Vec<PickRow> {
        let mut rows: Vec<PickRow> = (0..n_sessions)
            .map(|i| PickRow::Session(item(&format!("pir-{i}.jsonl"))))
            .collect();
        rows.extend(zellij.iter().map(|(s, p)| PickRow::Zellij {
            session: s.to_string(),
            panes: *p,
        }));
        rows
    }

    /// Transcripts come first (newest first), zellij rebuilds trail: a
    /// transcript index must stay stable no matter how many rebuild rows are
    /// offered, or `Resume(idx)` would point at the wrong session.
    #[test]
    fn zellij_rows_are_appended_after_transcripts() {
        let r = rows(2, &[("alpha", 3), ("beta", 1)]);
        assert_eq!(r.len(), 4);
        assert!(r[0].as_session().is_some());
        assert!(r[1].as_session().is_some());
        assert!(r[2].as_session().is_none());
        assert!(r[3].as_session().is_none());
        match &r[2] {
            PickRow::Zellij { session, panes } => {
                assert_eq!(session, "alpha");
                assert_eq!(*panes, 3);
            }
            _ => panic!("expected a zellij row"),
        }
    }

    /// The preview pane for a zellij row must describe the rebuild, not show an
    /// empty transcript — otherwise the operator cannot tell what Enter will do.
    #[test]
    fn zellij_row_preview_explains_the_rebuild() {
        let r = rows(0, &[("vitreous-hill", 7)]);
        let p = zellij_preview(&r[0]);
        assert!(p.first_prompt.contains("vitreous-hill"), "got: {}", p.first_prompt);
        assert!(p.last_prompt.contains('7'), "pane count must be shown: {}", p.last_prompt);
        // A session row must not produce this text.
        let s = rows(1, &[]);
        let sp = zellij_preview(&s[0]);
        assert!(!sp.first_prompt.contains("rebuild"), "a transcript row must not claim a rebuild");
    }

    /// Selecting a zellij row must yield `RebuildZellij(name)`, never a
    /// `Resume(idx)` that would resume some transcript instead.
    #[test]
    fn zellij_rows_are_never_resumable_by_index() {
        let r = rows(2, &[("alpha", 3)]);
        let zellij_positions: Vec<usize> = r
            .iter()
            .enumerate()
            .filter(|(_, row)| matches!(row, PickRow::Zellij { .. }))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(zellij_positions, vec![2]);
        // Resuming is only ever valid for a real session row.
        for i in zellij_positions {
            assert!(r[i].as_session().is_none(), "row {i} must not be resumable");
        }
    }
}

#[cfg(test)]
mod prefetch_tests {
    use super::*;

    /// The queue is consumed with `pop()` (last element first), so the
    /// *highlighted* row must be pushed last to be scanned first.
    #[test]
    fn order_scans_selection_first() {
        let o = order_from_len(10, 0);
        assert_eq!(*o.last().unwrap(), 0, "highlighted row must be scanned first");

        let o = order_from_len(10, 5);
        assert_eq!(*o.last().unwrap(), 5);

        let o = order_from_len(10, 9);
        assert_eq!(*o.last().unwrap(), 9);
    }

    #[test]
    fn order_covers_every_row_exactly_once() {
        for len in 1..12usize {
            for sel in 0..len {
                let o = order_from_len(len, sel);
                let mut sorted = o.clone();
                sorted.sort_unstable();
                let expect: Vec<usize> = (0..len).collect();
                assert_eq!(sorted, expect, "len={len} sel={sel} must cover all rows once");
            }
        }
    }

    /// After the highlighted row, the next candidate should be the row *below*
    /// it (where Down goes), not the one above.
    #[test]
    fn order_prefers_down_then_up() {
        // Pop order: last pushed first. Build the pop sequence explicitly.
        let o = order_from_len(10, 4);
        let pop_order: Vec<usize> = o.iter().rev().cloned().collect();
        assert_eq!(pop_order[0], 4, "highlighted first");
        assert_eq!(pop_order[1], 5, "then the row below (Down)");
        assert_eq!(pop_order[2], 3, "then the row above (Up)");
    }

    #[test]
    fn order_handles_selection_at_both_ends() {
        // Selection at 0: no rows above, so it walks downward only.
        let o = order_from_len(4, 0);
        let pops: Vec<usize> = o.iter().rev().cloned().collect();
        assert_eq!(pops, vec![0, 1, 2, 3]);

        // Selection at the last row: walks upward only.
        let o = order_from_len(4, 3);
        let pops: Vec<usize> = o.iter().rev().cloned().collect();
        assert_eq!(pops, vec![3, 2, 1, 0]);
    }

    /// A single-row list must not produce duplicates or panic.
    #[test]
    fn order_single_row() {
        assert_eq!(order_from_len(1, 0), vec![0]);
        assert_eq!(order_from_len(0, 0), vec![0]);
    }
}

#[cfg(all(test, unix))]
mod prefetch_integration_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn big_session(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        // ~2 MB: enough that a cold scan is measurable but the test stays fast.
        let mut s = String::new();
        let filler = "z".repeat(4096);
        for i in 0..40 {
            s.push_str(&format!(
                "{{\"role\":\"user\",\"blocks\":[{{\"type\":\"text\",\"text\":\"prompt {i}\"}}]}}\n"
            ));
            s.push_str(&format!(
                "{{\"role\":\"assistant\",\"blocks\":[{{\"type\":\"tool_use\",\"id\":\"t{i}\",\"name\":\"bash\",\"input\":{{}}}}]}}\n"
            ));
            s.push_str(&format!(
                "{{\"role\":\"user\",\"blocks\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"t{i}\",\"content\":\"{filler}\"}}]}}\n"
            ));
            s.push_str(&format!(
                "{{\"role\":\"assistant\",\"blocks\":[{{\"type\":\"text\",\"text\":\"answer {i}\"}}]}}\n"
            ));
        }
        let p = dir.join(name);
        std::fs::write(&p, s).unwrap();
        p
    }

    /// The worker must populate the cache without the UI ever scanning.
    #[test]
    fn prefetcher_fills_cache_in_background() {
        let d = std::env::temp_dir().join(format!("pir_pf_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let paths: Vec<_> = (0..4).map(|i| big_session(&d, &format!("s{i}.jsonl"))).collect();

        let pf = Prefetcher::spawn(paths.clone(), 2);
        // Wait for the highlighted row (2) to land, as the picker would.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut got = false;
        while Instant::now() < deadline {
            if pf.get(2).is_some() {
                got = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(got, "prefetcher never produced the highlighted row's preview");
        let p = pf.get(2).unwrap();
        assert_eq!(p.turns, 40);
        assert_eq!(p.first_prompt, "prompt 0");
        assert_eq!(p.last_prompt, "prompt 39");
        drop(pf);
        for p in &paths {
            let _ = std::fs::remove_file(p);
        }
        let _ = std::fs::remove_dir(&d);
    }

    /// Once warm, a lookup must be essentially free — that is the whole point:
    /// the picker's arrow-key repaint no longer waits on a parse.
    #[test]
    fn cached_lookup_is_fast_and_correct() {
        let d = std::env::temp_dir().join(format!("pir_pf2_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let path = big_session(&d, "one.jsonl");

        let cold = Instant::now();
        let direct = read_preview(&path);
        let cold_ms = cold.elapsed().as_secs_f64() * 1000.0;

        let pf = Prefetcher::spawn(vec![path.clone()], 0);
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline && pf.get(0).is_none() {
            std::thread::sleep(Duration::from_millis(5));
        }
        let warm = Instant::now();
        let cached = pf.get(0).expect("prefetched");
        let warm_ms = warm.elapsed().as_secs_f64() * 1000.0;

        // Identical content...
        assert_eq!(cached.turns, direct.turns);
        assert_eq!(cached.first_prompt, direct.first_prompt);
        assert_eq!(cached.last_prompt, direct.last_prompt);
        assert_eq!(cached.last_output, direct.last_output);
        // ...and obtained without re-reading the file.
        println!("cold scan {cold_ms:.1}ms vs cached lookup {warm_ms:.4}ms");
        assert!(
            warm_ms < 1.0,
            "cached lookup should be sub-millisecond, took {warm_ms:.3}ms"
        );
        drop(pf);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&d);
    }

    /// Dropping the picker must stop the worker promptly rather than let it
    /// grind through the rest of a large backlog.
    #[test]
    fn drop_stops_the_worker() {
        let d = std::env::temp_dir().join(format!("pir_pf3_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let paths: Vec<_> = (0..8).map(|i| big_session(&d, &format!("m{i}.jsonl"))).collect();
        let pf = Prefetcher::spawn(paths.clone(), 0);
        let t = Instant::now();
        drop(pf); // must join without waiting for 8 full scans
        let took = t.elapsed();
        assert!(took < Duration::from_secs(10), "drop took {took:?}");
        for p in &paths {
            let _ = std::fs::remove_file(p);
        }
        let _ = std::fs::remove_dir(&d);
    }
}


#[cfg(all(test, unix))]
mod prefetch_notify_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// The worker must wake a blocked `poll` on its notification pipe: that is
    /// what makes a preview appear without waiting for a keystroke, and what
    /// lets the picker keep `poll(.., -1)` (zero idle CPU) instead of a polling
    /// timeout.
    #[test]
    fn worker_wakes_a_blocked_poll() {
        use std::os::unix::io::AsRawFd as _;
        let d = std::env::temp_dir().join(format!("pir_notify_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        // A session big enough that the scan cannot finish before we park.
        let mut s = String::new();
        let filler = "q".repeat(8192);
        for i in 0..300 {
            s.push_str(&format!(
                "{{\"role\":\"assistant\",\"blocks\":[{{\"type\":\"text\",\"text\":\"a{i}\"}}]}}\n"
            ));
            s.push_str(&format!(
                "{{\"role\":\"user\",\"blocks\":[{{\"type\":\"tool_result\",\"content\":\"{filler}\"}}]}}\n"
            ));
        }
        let p = d.join("big.jsonl");
        std::fs::write(&p, s).unwrap();

        let pf = Prefetcher::spawn(vec![p.clone()], 0);
        let rx = pf.notify_rx.as_ref().expect("notification pipe");

        // Park exactly as wait_key does; the worker must wake us.
        let mut pfd = [libc::pollfd { fd: rx.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
        let t = Instant::now();
        let r = unsafe { libc::poll(pfd.as_mut_ptr(), 1, 10_000) }; // 10s ceiling
        let waited = t.elapsed();
        assert_eq!(r, 1, "expected the worker to make the pipe readable");
        assert!(pfd[0].revents & libc::POLLIN != 0);
        assert!(waited < Duration::from_secs(10), "must wake via the pipe, not the timeout");
        println!("worker woke the poll after {waited:?}");

        drop(pf);
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir(&d);
    }

    /// With the pipe in place the picker must not need a polling timeout, so an
    /// idle picker parks indefinitely. Assert the pipe exists (the mechanism
    /// the zero-idle-CPU property now depends on).
    #[test]
    fn notification_pipe_is_created() {
        let pf = Prefetcher::spawn(Vec::new(), 0);
        assert!(pf.notify_tx.is_some(), "write end must exist");
        assert!(pf.notify_rx.is_some(), "read end must exist");
        drop(pf);
    }
}

#[cfg(all(test, unix))]
mod prefetch_hygiene_tests {
    use super::*;

    /// Repeated spawn/drop must not leak file descriptors (each prefetcher
    /// creates a pipe) nor accumulate threads.
    ///
    /// `/proc/self/fd` and `/proc/self/task` are *process-global*, so a raw
    /// before/after delta also counts every other test running in parallel — a
    /// test that happens to open a file or spawn a thread during the window makes
    /// this fail (observed ~1 run in 5; `scripts/deploy.sh` already documents the
    /// suite as contention-bound).
    ///
    /// So we measure our own contribution with a **control batch**: a no-op
    /// allocating the same resource shape (one pipe, one short-lived thread).
    /// The batches are run *interleaved* — 10 cycles of one, then 10 of the
    /// other, repeated — so drift in the shared counters affects both equally.
    /// A leak raises the prefetcher batch's peak above the control's; parallel
    /// activity moves both together.
    #[test]
    fn spawn_drop_does_not_leak_fds_or_threads() {
        let count = |path: &str| std::fs::read_dir(path).map(|d| d.count()).unwrap_or(0);

        /// One pipe plus one short-lived thread: the shape a `Prefetcher`
        /// allocates. Used as the control batch.
        fn control_cycle() {
            let (r, w) = std::io::pipe().expect("pipe");
            let h = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(1));
                drop(r);
            });
            drop(w);
            let _ = h.join();
        }

        // Warm up so one-time allocations are not counted as a leak.
        for _ in 0..3 {
            drop(Prefetcher::spawn(Vec::new(), 0));
        }

        let mut control_peak = 0usize;
        let mut prefetch_peak = 0usize;
        let mut thread_peak = 0usize;
        for _ in 0..20 {
            for _ in 0..10 {
                control_cycle();
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
            control_peak = control_peak.max(count("/proc/self/fd"));

            for _ in 0..10 {
                drop(Prefetcher::spawn(Vec::new(), 0));
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
            prefetch_peak = prefetch_peak.max(count("/proc/self/fd"));
            thread_peak = thread_peak.max(count("/proc/self/task"));
        }

        // A leak of one fd per cycle across 200 cycles would dwarf this slack.
        assert!(
            prefetch_peak <= control_peak + 8,
            "fd leak: control batch peaked at {control_peak} open, prefetcher batch peaked at \
             {prefetch_peak} over 200 spawn/drop cycles"
        );
        assert!(
            thread_peak < 96,
            "thread count {thread_peak} during 200 prefetcher cycles looks like accumulation"
        );
    }

    /// An empty backlog (or a fully-cached one) must not spin: the worker exits
    /// when the queue drains, rather than busy-looping on an empty Vec.
    #[test]
    fn worker_exits_when_queue_is_empty() {
        let pf = Prefetcher::spawn(Vec::new(), 0);
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Cache stays empty and get() still answers promptly.
        assert!(pf.get(0).is_none());
        drop(pf);
    }
}

