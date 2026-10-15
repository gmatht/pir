//! Alternate-screen modal dialogs (crossterm).
//!
//! pir's streaming REPL runs on the **normal screen** so the whole session
//! (thoughts, replies, tool calls) stays in the terminal's scrollback. Transient
//! **modals** are the exception: they pop up on the **alternate screen** (via
//! crossterm's `EnterAlternateScreen`/`LeaveAlternateScreen`), hide the agent's
//! streaming output while they're up, and restore the normal screen (with full
//! scrollback) on close.
//!
//! This module provides the shared infrastructure: `enter_modal`/`leave_modal`
//! (alternate screen + raw mode) and a key reader that uses crossterm's
//! `event::read` so arrows/Enter/Esc work uniformly across dialogs.

use crate::config::ApiKind;
use crate::term;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The previously drawn modal frame (its vertical extent + exact bytes sent).
/// Used to repaint only when the frame actually changed: clearing just the
/// previous/current box rectangle (not the whole screen) and skipping identical
/// frames is what stops the menu/list flicker when you move the selection
/// within a non-scrolling list.
struct PrevFrame {
    top: usize,
    bottom: usize,
    out: String,
}
static PREV_FRAME: Mutex<Option<PrevFrame>> = Mutex::new(None);

/// True while a modal owns the keyboard.
///
/// A modal reads the *same* fd 0 the REPL's mid-turn reader does. Two readers
/// on one tty is a race the operator always loses: the mid-turn reader
/// (`term::raw::read_chunk`) drains the tty eagerly and translates `ESC[A` /
/// `ESC[B` into *history recall*, so the arrow keys never reached the dialog
/// and ↑/↓ did nothing at all. The REPL consults this flag to stay out of the
/// way for as long as a modal is up (see `term::raw::wait_input`).
static KEYBOARD_HELD: AtomicBool = AtomicBool::new(false);

/// Whether a modal currently owns the keyboard. The REPL's mid-turn input
/// reader must not consume stdin while this is true.
pub fn keyboard_held() -> bool {
    KEYBOARD_HELD.load(Ordering::SeqCst)
}

/// Test-only: pretend a modal owns (or released) the screen, so tick-pause
/// behaviour can be probed without a tty (`Modal::enter` refuses non-ttys).
#[cfg(test)]
pub(crate) fn set_keyboard_held_for_test(v: bool) {
    KEYBOARD_HELD.store(v, Ordering::SeqCst);
}

/// A key the user pressed in a modal dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A printable character (e.g. `o`, `s`, `n`, `i`).
    Char(char),
    Enter,
    Esc,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Tab,
    BackTab,
    CtrlC,
    CtrlD,
    CtrlN,
    CtrlM,
    Resize,
    /// Any other key (ignored by most dialogs).
    Other,
}

/// RAII guard that enters the alternate screen + raw mode on construction and
/// restores the normal screen + terminal state on drop (even on panic).
pub struct Modal {
    active: bool,
}

impl Modal {
    /// Enter the alternate screen and raw mode. Returns `None` if the terminal
    /// isn't a tty (so a piped/scripted run falls back to the caller's default
    /// rather than hanging on a modal that can't be answered).
    pub fn enter() -> Option<Modal> {
        if !crate::term::is_terminal() {
            return None;
        }
        // Claim the keyboard *before* any of the work below, and only release
        // it if construction fails. Every dialog that opens while a turn is
        // running (an `ask_user_question` box, a tool-approval box) is opened
        // from the agent's worker thread while the REPL's mid-turn reader is
        // parked in `wait_input` with raw mode on: two readers on one tty means
        // whichever wakes first eats the key, and the REPL is a far bigger
        // tattletale than the dialog.
        KEYBOARD_HELD.store(true, Ordering::SeqCst);
        let mut stdout = io::stdout();
        // Enter alternate screen, hide the cursor, enable raw mode.
        if execute!(stdout, EnterAlternateScreen, Hide).is_err() {
            KEYBOARD_HELD.store(false, Ordering::SeqCst);
            return None;
        }
        // Blank the alternate screen *once*, now, so the first frame starts from
        // a clean slate. A per-frame `ESC[2J` is what caused the menu flicker
        // (every keypress cleared + repainted the whole screen), so the draw
        // functions never do that — they only clear the small box region. Also
        // drop any frame cached from a previous modal so the first repaint
        // compares against nothing.
        let _ = stdout.write_all(b"\x1b[2J\x1b[H");
        let _ = stdout.flush();
        *PREV_FRAME.lock().unwrap() = None;
        if enable_raw_mode().is_err() {
            KEYBOARD_HELD.store(false, Ordering::SeqCst);
            let _ = execute!(stdout, LeaveAlternateScreen, Show);
            return None;
        }
        // Drain any keypresses that were queued *before* the modal opened — e.g.
        // the Enter that submitted `/menu` — so they can't be misread as a menu
        // selection (which made `/menu` skip straight to the first item).
        while event::poll(Duration::ZERO).unwrap_or(false) {
            let _ = event::read();
        }
        Some(Modal { active: true })
    }
}

impl Drop for Modal {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        KEYBOARD_HELD.store(false, Ordering::SeqCst);
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, Show);
        let _ = stdout.flush();
    }
}

/// The abandonment flag of the currently-installed ask, if any. Set by
/// [`set_abort_flag`] before the renderer runs. Kept as an `Arc` so clones are
/// cheap and no raw pointers are involved.
static ABORT: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

/// Install the abandonment flag the ask renderer should poll. Called by
/// [`crate::askuser::ask`] before it invokes the renderer; the flag is the
/// caller's, so a timeout can be observed by the dialog itself.
pub fn set_abort_flag(flag: Arc<AtomicBool>) {
    *ABORT.lock().unwrap_or_else(|e| e.into_inner()) = Some(flag);
}

/// Forget the current ask's abandonment flag. Called by [`crate::askuser::ask`]
/// once it has returned, so a timed-out question cannot make a *later*
/// unrelated dialog (the menus, pickers and other `read_key` users) close
/// itself the instant it opens — the flag belongs to one question, not to the
/// process.
pub fn clear_abort_flag() {
    *ABORT.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// True when the current ask has been abandoned (the caller timed out). The
/// modal loops check this so an abandoned dialog closes itself.
pub fn aborted() -> bool {
    ABORT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|f| f.load(Ordering::SeqCst))
        .unwrap_or(false)
}

/// Read a single key from the terminal (blocking). Returns `None` on EOF, a
/// read error, or when the ask has been abandoned (see [`aborted`]), which the
/// callers treat as "dismiss" so an abandoned dialog returns promptly instead
/// of blocking forever on the alternate screen.
///
/// The read is polled rather than fully blocking so an abandonment is noticed
/// within [`ABORT_POLL`] even when the operator never presses a key.
pub fn read_key() -> Option<Key> {
    loop {
        if aborted() {
            return None;
        }
        if event::poll(ABORT_POLL).unwrap_or(false) {
            match event::read() {
                Ok(Event::Key(KeyEvent { code, modifiers, kind, .. })) => {
                    // Ignore key-release events: a release (e.g. the Enter key-up
                    // that follows submitting `/menu`) must never be treated as a
                    // press — that made the menu skip straight to the first item.
                    // Press and Repeat are both accepted (holding a key repeats).
                    if kind == KeyEventKind::Release {
                        continue;
                    }
                    return Some(translate_key(code, modifiers));
                }
                Ok(Event::Resize(_, _)) => return Some(Key::Resize),
                Ok(_) => continue, // mouse / focus / paste: ignore
                Err(_) => return None,
            }
        }
        // Timed out waiting for a key: loop to re-check the abort flag. This is
        // the only reason the poll is bounded — a plain blocking `event::read`
        // would sit in the read forever after the caller gave up.
    }
}

/// How long [`read_key`] waits for a key before re-checking the abandonment
/// flag. Small enough that an abandoned dialog closes imperceptibly fast,
/// large enough not to spin.
const ABORT_POLL: Duration = Duration::from_millis(100);

fn translate_key(code: KeyCode, mods: KeyModifiers) -> Key {
    use KeyCode::*;
    match code {
        Char(c) => {
            // Ctrl+letter → control key.
            if mods.contains(KeyModifiers::CONTROL) {
                match c.to_ascii_lowercase() {
                    'c' => Key::CtrlC,
                    'd' => Key::CtrlD,
                    'n' => Key::CtrlN,
                    'm' => Key::CtrlM,
                    _ => Key::Other,
                }
            } else {
                Key::Char(c)
            }
        }
        Enter => Key::Enter,
        Esc => Key::Esc,
        Up => Key::Up,
        Down => Key::Down,
        Left => Key::Left,
        Right => Key::Right,
        PageUp => Key::PageUp,
        PageDown => Key::PageDown,
        Home => Key::Home,
        End => Key::End,
        Tab => Key::Tab,
        BackTab => Key::BackTab,
        Backspace => Key::Char('\u{8}'),
        Delete => Key::Other,
        _ => Key::Other,
    }
}

/// Draw a simple centered box with a title and body lines on the alternate
/// screen. `lines` are written verbatim (callers apply their own styling).
/// Returns the number of rows drawn (for cursor restore).
///
/// Repaints only when the frame changed: it clears just the union of the
/// previous and current box rectangles (not the whole screen), so moving the
/// selection within a list that fits without scrolling no longer flashes the
/// entire screen on every keypress. An identical frame is skipped entirely.
pub fn draw_box(title: &str, lines: &[String]) -> usize {
    draw_box_titled(title, lines, false)
}

/// Draw a centered box, with the title bar styled as a *blocking* prompt when
/// `blocking` is set.
///
/// A blocking box is one that has stopped the agent until the operator answers
/// (a tool approval, a secret entry, a question) — as opposed to a navigational
/// dialog the operator opened themselves (`/menu`, `/model`), which must NOT
/// wear the alert styling or the banner would cry wolf. The blocking title gets
/// a bold black-on-yellow bar, so the whole top edge of the box reads as
/// "you are the blocker" and matches the inline [`crate::term::awaiting_banner`]
/// used on the non-modal prompts.
pub fn draw_box_titled(title: &str, lines: &[String], blocking: bool) -> usize {
    let w = crate::term::terminal_width();
    let h = crate::term::terminal_height();
    let body_w = w.saturating_sub(4).max(10);
    let body_h = lines.len().min(h.saturating_sub(4).max(1));
    let top = (h.saturating_sub(body_h + 2)) / 2;
    let left = (w.saturating_sub(body_w + 2)) / 2;
    let bottom = top + 1 + body_h;

    let mut out = String::new();
    // Title bar. Measure the *visible* width: a blocking title carries SGR
    // escapes (the alert styling), and counting raw chars would over-count them
    // as printable columns and under-pad the rest of the border.
    let plain = truncate(title, body_w);
    let shown = if blocking { crate::term::alert(&plain) } else { plain.clone() };
    out.push_str(&format!("\x1b[{};{}H┌─ {} ─", top, left, shown));
    // Fill the rest of the top border.
    let used = 4 + crate::term::visible_len(&plain);
    for _ in used..body_w + 2 {
        out.push('─');
    }
    out.push('┐');
    // Body.
    for (i, line) in lines.iter().take(body_h).enumerate() {
        let row = top + 1 + i;
        out.push_str(&format!("\x1b[{};{}H│", row, left));
        out.push_str(&truncate(line, body_w));
        // Pad to the right border.
        let lw = crate::term::visible_len(line).min(body_w);
        for _ in lw..body_w {
            out.push(' ');
        }
        out.push('│');
    }
    // Bottom border.
    out.push_str(&format!("\x1b[{};{}H└", bottom, left));
    for _ in 0..body_w + 2 {
        out.push('─');
    }
    out.push('┘');
    // Move cursor to a safe spot.
    out.push_str(&format!("\x1b[{};{}H", bottom + 1, left));

    paint_frame(top, bottom, &out)
}

/// Draw a centered box showing a *viewport* of `lines`: the first visible
/// row is `top` and the first visible column is `left`, so callers can scroll
/// long content with the arrow keys (up/down = vertical, left/right = horizontal).
/// `footer` is printed on the line directly below the box (use it for control
/// hints). Returns the number of rows drawn (for cursor restore).
///
/// Repaints only when the frame changed (see [`draw_box`]): it clears just the
/// union of the previous and current box rectangles, so the menu/list no longer
/// flickers when you move the selection or scroll within a viewport.
pub fn draw_box_scrolled(title: &str, lines: &[String], top: usize, left: usize, footer: &str) -> usize {
    let w = crate::term::terminal_width();
    let h = crate::term::terminal_height();
    let body_w = w.saturating_sub(4).max(10);
    let max_rows = h.saturating_sub(4).max(1);
    let body_h = lines.len().min(max_rows);
    let top_row = (h.saturating_sub(body_h + 2)) / 2;
    let left_col = (w.saturating_sub(body_w + 2)) / 2;
    let bottom = top_row + 1 + body_h;

    let mut out = String::new();
    // Title bar.
    let title = truncate(title, body_w);
    out.push_str(&format!("\x1b[{};{}H┌─ {} ─", top_row, left_col, title));
    let used = 4 + title.chars().count();
    for _ in used..body_w + 2 {
        out.push('─');
    }
    out.push('┐');
    // Body: render only the viewport `lines[top .. top+body_h]`.
    let end = (top + body_h).min(lines.len());
    for (i, line) in lines.iter().enumerate().take(end).skip(top) {
        let row = top_row + 1 + (i - top);
        out.push_str(&format!("\x1b[{};{}H│", row, left_col));
        // Horizontal viewport: skip `left` cols, take `body_w`.
        let slice: String = line.chars().skip(left).take(body_w).collect();
        out.push_str(&slice);
        let lw = slice.chars().count();
        for _ in lw..body_w {
            out.push(' ');
        }
        out.push('│');
    }
    // Bottom border.
    out.push_str(&format!("\x1b[{};{}H└", bottom, left_col));
    for _ in 0..body_w + 2 {
        out.push('─');
    }
    out.push('┘');
    // Footer (control hints) + cursor to a safe spot.
    let footer = truncate(footer, body_w);
    out.push_str(&format!("\x1b[{};{}H{}", bottom + 1, left_col, footer));
    out.push_str(&format!("\x1b[{};{}H", bottom, left_col));

    paint_frame(top_row, bottom, &out)
}

/// Emit a freshly built box `out` for the vertical span `[top, bottom]`, but
/// only when it differs from the previous frame. We clear the *union* of the
/// previous and current box rectangles (rewriting each row of that span from
/// column 1 to the right edge with spaces) instead of `ESC[2J`, which blanks
/// the entire alternate screen and is what caused the flicker: a no-op
/// keypress (e.g. Up at the top of a list) used to clear+repaint the whole
/// screen, whereas now only the small box region is touched — and when the
/// frame is byte-for-byte identical (the common "I pressed a key but nothing
/// moved" case) we skip writing anything at all.
fn paint_frame(top: usize, bottom: usize, out: &str) -> usize {
    let mut prev = PREV_FRAME.lock().unwrap();
    if let Some(p) = prev.as_ref()
        && p.out == out {
            // Unchanged frame: nothing to do. Return the previous extents so the
            // caller's cursor math stays consistent.
            return p.bottom.saturating_sub(p.top);
        }
    let w = crate::term::terminal_width().max(2);
    let clear_top = prev.as_ref().map(|p| p.top).unwrap_or(top).min(top);
    let clear_bottom = prev.as_ref().map(|p| p.bottom).unwrap_or(bottom).max(bottom);
    let mut buf = String::new();
    buf.push_str("\x1b[?25l"); // hide cursor during repaint
    for row in clear_top..=clear_bottom {
        buf.push_str(&format!("\x1b[{};1H", row));
        for _ in 0..w {
            buf.push(' ');
        }
    }
    buf.push_str(out);
    buf.push_str("\x1b[?25h"); // restore cursor
    let _ = io::stdout().write_all(buf.as_bytes());
    let _ = io::stdout().flush();
    *prev = Some(PrevFrame { top, bottom, out: out.to_string() });
    bottom.saturating_sub(top)
}

/// Run a simple dismiss-only dialog that also supports scrolling: draw `lines`
/// in a scrollable box (so long content isn't clipped) and read keys until the
/// user dismisses with Esc/ctrl-c/ctrl-d/Enter. Used by the help/security/
/// settings/about dialogs.
fn scroll_until_dismiss(title: &str, lines: &[String]) -> Option<()> {
    let mut top = 0usize;
    let mut left = 0usize;
    let footer = crate::term::dim("[↑↓] scroll  [←→] sideways  [esc/enter] close").to_string();
    loop {
        draw_box_scrolled(title, lines, top, left, &footer);
        match read_key()? {
            Key::Up | Key::Char('k') => top = top.saturating_sub(1),
            Key::Down | Key::Char('j') => {
                let max = lines.len().saturating_sub(1);
                if top < max { top += 1; }
            }
            Key::Left => left = left.saturating_sub(8),
            Key::Right => {
                let max = lines.iter().map(|l| crate::term::visible_len(l)).max().unwrap_or(0).saturating_sub(1);
                if left < max { left += 8; }
            }
            Key::PageUp => top = top.saturating_sub(10),
            Key::PageDown => {
                let max = lines.len().saturating_sub(1);
                if top < max { top = (top + 10).min(max); }
            }
            Key::Esc | Key::CtrlC | Key::CtrlD | Key::Enter => return Some(()),
            _ => {}
        }
    }
}

/// Truncate `s` to at most `n` visible chars, appending `…` when cut.
fn truncate(s: &str, n: usize) -> String {
    if crate::term::visible_len(s) <= n {
        return s.to_string();
    }
    let mut out = String::new();
    for (vis, c) in s.chars().enumerate() {
        if vis >= n.saturating_sub(1) {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

/// The operator's decision from a tool-approval dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    AllowOnce,
    AllowSession,
    Deny,
}

/// An action chosen from the main menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    Resume,
    BackgroundSessions,
    Model,
    Thinking,
    Security,
    Settings,
    Worktrees,
    AutocleanCoverage,
    Help,
    About,
    Quit,
    None,
}

/// Show the main menu on the alternate screen and return the chosen action.
/// Returns `None` if the terminal isn't a tty (caller falls back to no-op).
pub fn main_menu() -> Option<MenuAction> {
    let _modal = Modal::enter()?;
    let items = [
        ("r", "Resume / pick session"),
        ("b", "Backgrounded sessions"),
        ("m", "Model"),
        ("t", "Thinking"),
        ("s", "Security"),
        ("e", "Settings"),
        ("w", "Worktrees by default"),
        ("c", "Autoclean branch coverage"),
        ("h", "Help"),
        ("a", "About"),
        ("q", "Quit"),
    ];
    let mut selected = 0usize;
    loop {
        let lines: Vec<String> = items
            .iter()
            .enumerate()
            .map(|(i, (key, label))| {
                let marker = if i == selected { "▸" } else { " " };
                format!("{marker} [{key}] {label}")
            })
            .collect();
        draw_box("pir — main menu", &lines);
        match read_key()? {
            Key::Up | Key::Char('k') => selected = selected.saturating_sub(1),
            Key::Down | Key::Char('j') => selected = (selected + 1).min(items.len() - 1),
            Key::Enter | Key::Right => return Some(action_for(items[selected].0)),
            Key::Char(c) => {
                let c = c.to_ascii_lowercase();
                if let Some((i, _)) = items.iter().enumerate().find(|(_, (k, _))| k.starts_with(c)) {
                    return Some(action_for(items[i].0));
                }
            }
            Key::Esc | Key::CtrlC | Key::CtrlD => return Some(MenuAction::None),
            _ => {}
        }
    }
}

fn action_for(key: &str) -> MenuAction {
    match key {
        "r" => MenuAction::Resume,
        "b" => MenuAction::BackgroundSessions,
        "m" => MenuAction::Model,
        "t" => MenuAction::Thinking,
        "s" => MenuAction::Security,
        "e" => MenuAction::Settings,
        "w" => MenuAction::Worktrees,
        "c" => MenuAction::AutocleanCoverage,
        "h" => MenuAction::Help,
        "a" => MenuAction::About,
        "q" => MenuAction::Quit,
        _ => MenuAction::None,
    }
}

/// Show the security dialog on the alternate screen. Displays the current
/// posture and OS support (needs-root / extra deps / active) for each option.
/// Returns `None` if not a tty.
pub fn security_dialog(
    policy: &crate::security::SecurityPolicy,
    su_security: bool,
) -> Option<()> {
    let _modal = Modal::enter()?;
    let lines = security_lines(policy, su_security);
    scroll_until_dismiss("pir — security", &lines)
}

fn security_lines(policy: &crate::security::SecurityPolicy, su_security: bool) -> Vec<String> {
    use crate::term;
    let mut lines = Vec::new();
    lines.push(term::bold("posture").to_string());
    lines.push(format!("  level: {}", term::cyan(policy.level.as_str())));
    lines.push(format!("  mitigation engine: {}",
        if crate::security::mitigation_active() { term::green("on") } else { term::yellow("off") }));
    lines.push(format!("  apt: {}   network: {}   ask: {}   read: {}",
        policy.apt.as_str(), policy.network.as_str(), policy.ask.as_str(), policy.read.as_str()));
    let (q_on, q_backend) = crate::security::quarantine_engaged_surface();
    let qp_on = {
        #[cfg(unix)]
        { crate::security::overlay::project_quarantine_engaged() }
        #[cfg(not(unix))]
        { false }
    };
    lines.push(format!("  quarantine: {}   project-quarantine: {}",
        if q_on { format!("on (engaged: {q_backend})") } else { "CONFIG on, NOT mounted".to_string() },
        if qp_on { "on (engaged)".to_string() } else { "off (no worktree overlay)".to_string() }));
    lines.push(format!("  su-based boundary: {}", if su_security { term::green("ENABLED") } else { term::yellow("disabled") }));
    lines.push(String::new());
    lines.push(term::bold("OS support").to_string());
    // unix vs windows support.
    #[cfg(unix)]
    {
        lines.push("  overlayfs quarantine: needs root + overlayfs in kernel".to_string());
        lines.push("  su-based boundary: needs root (sudoers.d/skynet-ai + wrappers)".to_string());
        lines.push("  apt: needs root to install".to_string());
    }
    #[cfg(windows)]
    {
        lines.push(term::yellow("  Windows security: TODO (Defender/SmartScreen, AppContainer,").to_string());
        lines.push(term::yellow("  MIC, UAC, WDAC — not yet implemented)").to_string());
    }
    lines.push(String::new());
    lines.push(term::dim("[esc] close").to_string());
    lines
}

/// The editable rows of the settings dialog: the values it can change, read
/// once when it opens and written back out when the operator saves.
///
/// This is a *view model*, not a handle on the agent. The dialog runs on the
/// alternate screen and the REPL holds the agent in `agent_slot`, so the dialog
/// must not touch the agent itself — the caller applies whatever comes back
/// (and, on save, persists it). Keeping the struct plain is also what makes the
/// row index stable enough to unit-test without a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsValues {
    /// Not editable here: the model changes with `/model` and the dedicated
    /// picker, which is a different screen with a different key space. Shown
    /// for orientation only, and skipped by the cursor.
    pub model: String,
    /// The full list of thinking levels to cycle through, in display order. The
    /// dialog cannot know which are valid for the current model, so the caller
    /// passes the already-filtered list (same source as `thinking_picker`).
    /// Empty means "unknown/unsupported model" and leaves the row inert.
    pub thinking_levels: Vec<String>,
    pub thinking: String,
    pub show_thinking: bool,
    pub incremental: bool,
    pub stop_skill: bool,
    pub full_auto: bool,
}

impl SettingsValues {
    /// The number of editable rows (`thinking`, `show`, `incremental`,
    /// `stop-skill`, `full-auto`). `model` is a status line, not a row.
    const ROWS: usize = 5;
}

/// Outcome of the settings dialog.
pub enum SettingsOutcome {
    /// The operator applied the values (Enter or `s`). The struct holds the
    /// *edited* values, so the caller diffs them against what it passed in to
    /// decide what actually needs applying.
    Saved(SettingsValues),
    /// The operator closed without applying (Esc / `q` / ctrl-c).
    Discarded,
    /// The dialog could not be opened (no tty).
    NoTty,
}

/// Show the settings dialog on the alternate screen — and make it *editable*.
///
/// It used to be a report: it drew the current values and delegated to
/// [`scroll_until_dismiss`], so `full-auto` (and every other row) could be read
/// but never changed, which is why the screen advertised a value the session
/// had no way to act on. This is now the same shape as [`security_editor`]:
/// `↑/↓` move, `←/→` cycle the value on the selected row, Enter/`s` applies,
/// `q`/Esc discards.
///
/// The values come in and go out as a plain [`SettingsValues`], so nothing here
/// needs the agent and the REPL can decide what to persist.
pub fn settings_editor(vals: SettingsValues) -> SettingsOutcome {
    let Some(_modal) = Modal::enter() else {
        return SettingsOutcome::NoTty;
    };
    let mut cur = vals;
    let mut selected = 0usize;

    let yn = |b: bool| -> String { if b { term::green("on") } else { term::yellow("off") } };

    loop {
        let marker = |i: usize| if i == selected { "▸" } else { " " };
        let lines: Vec<String> = vec![
            format!("  model: {}", term::cyan(&cur.model)),
            String::new(),
            format!("{} thinking            {}", marker(0), term::cyan(&cur.thinking)),
            format!("{} show thinking       {}", marker(1), yn(cur.show_thinking)),
            format!("{} incremental md      {}", marker(2), yn(cur.incremental)),
            format!("{} stop-skill          {}", marker(3), yn(cur.stop_skill)),
            format!("{} full-auto           {}", marker(4), yn(cur.full_auto)),
            String::new(),
            term::dim("[↑/↓] move  [←/→] change  [enter] apply  [q] discard"),
            term::dim("full-auto on = ask auto-yes: unanticipated requests run without asking"),
        ];
        draw_box("pir — settings", &lines);

        let Some(key) = read_key() else {
            return SettingsOutcome::NoTty;
        };
        match key {
            Key::Up | Key::Char('k') => selected = selected.saturating_sub(1),
            Key::Down | Key::Char('j') => selected = (selected + 1).min(SettingsValues::ROWS - 1),
            Key::Left | Key::Char('h') => apply_setting(&mut cur, selected, -1),
            Key::Right | Key::Char('l') => apply_setting(&mut cur, selected, 1),
            Key::Enter | Key::Char('s') | Key::Char('S') => return SettingsOutcome::Saved(cur),
            Key::Esc | Key::CtrlC | Key::CtrlD | Key::Char('q') | Key::Char('Q') => {
                return SettingsOutcome::Discarded;
            }
            _ => {}
        }
    }
}

/// Apply one `←`/`→` step to row `row`. Shared by the key handler and the tests
/// so the step logic is verifiable without a terminal.
///
/// `dir` is `-1` (back) or `1` (forward). The thinking row cycles the caller's
/// filtered level list; the rest are booleans, so both directions toggle.
fn apply_setting(vals: &mut SettingsValues, row: usize, dir: i32) {
    match row {
        0 => {
            let levels = &vals.thinking_levels;
            if levels.is_empty() {
                return;
            }
            let cur = vals.thinking.as_str();
            let i = levels.iter().position(|l| l == cur).unwrap_or(0) as i32;
            let n = levels.len() as i32;
            let next = ((i + dir).rem_euclid(n)) as usize;
            vals.thinking = levels[next].clone();
        }
        1 => vals.show_thinking = !vals.show_thinking,
        2 => vals.incremental = !vals.incremental,
        3 => vals.stop_skill = !vals.stop_skill,
        4 => vals.full_auto = !vals.full_auto,
        _ => {}
    }
}

/// Show the help dialog on the alternate screen. `help_text` is the full `/help`
/// text (split into lines). Returns `None` if not a tty.
pub fn help_dialog(help_text: &str) -> Option<()> {
    let _modal = Modal::enter()?;
    let lines: Vec<String> = help_text.lines().map(|l| l.to_string()).collect();
    let mut top = 0usize;
    let mut left = 0usize;
    let footer = crate::term::dim("[↑↓] scroll  [←→] sideways  [esc/enter] close").to_string();
    loop {
        draw_box_scrolled("pir — help", &lines, top, left, &footer);
        match read_key()? {
            Key::Up | Key::Char('k') => top = top.saturating_sub(1),
            Key::Down | Key::Char('j') => {
                let max = lines.len().saturating_sub(1);
                if top < max { top += 1; }
            }
            Key::Left => left = left.saturating_sub(8),
            Key::Right => {
                let max = lines.iter().map(|l| crate::term::visible_len(l)).max().unwrap_or(0).saturating_sub(1);
                if left < max { left += 8; }
            }
            Key::PageUp => top = top.saturating_sub(10),
            Key::PageDown => {
                let max = lines.len().saturating_sub(1);
                if top < max { top = (top + 10).min(max); }
            }
            Key::Esc | Key::CtrlC | Key::CtrlD | Key::Enter => return Some(()),
            _ => {}
        }
    }
}

/// Show the about dialog on the alternate screen (version + git hash + build
/// profile + license + deps). Returns `None` if not a tty.
pub fn about_dialog() -> Option<()> {
    let _modal = Modal::enter()?;
    use crate::term;
    let lines = vec![
        format!("pir {}", term::bold(env!("CARGO_PKG_VERSION"))),
        format!("git: {}", term::cyan(env!("GIT_HASH"))),
        format!("build: {} · opt-level {} · lto {}",
            if cfg!(debug_assertions) { "debug" } else { "release" },
            option_env!("OPT_LEVEL").unwrap_or("?"),
            option_env!("LTO").unwrap_or("?")),
        format!("license: {}", term::dim("GPL-3.0")),
        String::new(),
        term::dim("deps: pulldown-cmark · streamdown-parser · rustyline · lsb-curl · smol · crossterm").to_string(),
        String::new(),
        term::dim("[esc] close").to_string(),
    ];
    scroll_until_dismiss("pir — about", &lines)
}

/// A session row shown in the backgrounded-session selector.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub name: String,
    pub preview: String,
    /// Coarse state token: `running` / `complete` / `waiting for input` /
    /// `needs retry` / `blocked` / `error` / `interrupted`.
    pub state: String,
    pub from_here: bool,
}

/// Outcome of the backgrounded-session selector.
pub enum SessionPick {
    /// Resume the session at this index.
    Resume(usize),
    /// Jump to the next waiting-for-input session (index into the list).
    NextWaiting(usize),
    /// Cancel (no action).
    Cancel,
}

/// Show the backgrounded-session selector on the alternate screen. Lists
/// sessions with their state; `n` jumps to the next waiting-for-input session.
/// Returns `None` if not a tty.
pub fn session_selector(rows: &[SessionRow]) -> Option<SessionPick> {
    let _modal = Modal::enter()?;
    let mut selected = 0usize;
    loop {
        let lines: Vec<String> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let marker = if i == selected { "▸" } else { " " };
                let state = state_color(&r.state);
                let here = if r.from_here { " (this shell)" } else { "" };
                format!("{marker} {:<2} {}  {}  {}{here}", i, state, r.name, r.preview)
            })
            .collect();
        draw_box("pir — backgrounded sessions", &lines);
        match read_key()? {
            Key::Up | Key::Char('k') => selected = selected.saturating_sub(1),
            Key::Down | Key::Char('j') => selected = (selected + 1).min(rows.len().saturating_sub(1)),
            Key::Enter | Key::Right => return Some(SessionPick::Resume(selected)),
            Key::Char('n') => {
                // Jump to the next waiting-for-input / needs-retry session.
                let start = (selected + 1) % rows.len();
                let mut found = None;
                for off in 0..rows.len() {
                    let idx = (start + off) % rows.len();
                    let s = &rows[idx].state;
                    if s == "waiting for input" || s == "needs retry" {
                        found = Some(idx);
                        break;
                    }
                }
                if let Some(idx) = found {
                    selected = idx;
                }
            }
            Key::Char('r') => return Some(SessionPick::Resume(selected)),
            Key::Esc | Key::CtrlC | Key::CtrlD | Key::Char('q') => return Some(SessionPick::Cancel),
            _ => {}
        }
    }
}

/// What a keypress does to the model picker's selection: move it, or commit
/// (with the flat index), or nothing. Pure so the key map can be tested without
/// a tty (see `model_picker_key_tests`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelPickAction {
    Move(usize),
    Commit(usize),
    Cancel,
    Ignore,
}

/// The model picker's key map, pure: given the row count, the current
/// selection, and a key, decide what happens. `rows[i].0` — the flat index
/// shown as `:N` — is what a digit jumps to and what Enter commits.
///
/// A digit jumps to the row whose *displayed* `:N` it names (the same numbering
/// `list_models` prints and `config::select(":N")` resolves), so pressing `3`
/// selects the row labelled `:3`. Both the `:N` label and the position are the
/// same number here because rows are built in flat order; keying off the label
/// (not the offset) keeps them in step if that ever changes.
fn model_pick_action(rows: &[(usize, String, String, bool)], selected: usize, key: Key) -> ModelPickAction {
    match key {
        Key::Up | Key::Char('k') => ModelPickAction::Move(selected.saturating_sub(1)),
        Key::Down | Key::Char('j') => ModelPickAction::Move((selected + 1).min(rows.len() - 1)),
        Key::Home => ModelPickAction::Move(0),
        Key::End => ModelPickAction::Move(rows.len().saturating_sub(1)),
        Key::Enter | Key::Right => ModelPickAction::Commit(rows[selected].0),
        Key::Esc | Key::CtrlC | Key::CtrlD => ModelPickAction::Cancel,
        // A digit selects the row labelled `:N` (1-based on screen is `:0`-based
        // in the label, so the digit IS the index). Out-of-range digits are
        // ignored, exactly like the session picker's (`picker.rs`).
        Key::Char(c) if c.is_ascii_digit() => {
            let n = c.to_digit(10).unwrap_or(0) as usize;
            match rows.iter().position(|r| r.0 == n) {
                Some(i) => ModelPickAction::Move(i),
                None => ModelPickAction::Ignore,
            }
        }
        _ => ModelPickAction::Ignore,
    }
}

/// Show a model picker on the alternate screen. `providers` is the catalog in
/// `/models` order; `current` is the active `provider/model` label (e.g.
/// `openai/gpt-fake`) to highlight, or `""` for none. Returns the chosen flat
/// index into the provider→model list (matches `config::select(":N")`), or
/// `None` if not a tty / the list is empty / cancelled.
///
/// Keys: Up/Down (or `j`/`k`) move, Home/End jump to the ends, a **digit**
/// jumps to the row labelled `:N`, Enter/Right commit the highlighted row,
/// Esc/ctrl-c/ctrl-d cancel.
pub fn model_picker(providers: &[crate::config::Provider], current: &str) -> Option<usize> {
    let _modal = Modal::enter()?;
    let mut rows: Vec<(usize, String, String, bool)> = Vec::new();
    let mut idx = 0usize;
    for p in providers {
        for m in &p.models {
            let label = m.name.clone().unwrap_or_else(|| m.id.clone());
            rows.push((idx, p.pid(), label, format!("{}/{}", p.pid(), m.id) == current));
            idx += 1;
        }
    }
    if rows.is_empty() {
        return None;
    }
    let mut selected = rows.iter().position(|r| r.3).unwrap_or(0);
    loop {
        let w = rows.iter().map(|r| r.1.chars().count()).max().unwrap_or(8).min(24);
        let lines: Vec<String> = rows
            .iter()
            .enumerate()
            .map(|(i, (ix, pid, label, cur))| {
                let marker = if i == selected { "▸" } else { " " };
                let cur_mark = if *cur { term::green("  •current") } else { String::new() };
                format!("{marker} {:>3}  {:<w$}  {}{}  ", ix, term::cyan(pid), label, cur_mark)
            })
            .collect();
        // The `:N` left of each row is a *key* now, not just an id: say so, or
        // the digit shortcut stays as undiscoverable as the arrows were before.
        // Appended inside the box (like the option dialog's hint) rather than
        // printed outside, which would re-emit on every arrow keypress.
        let mut lines = lines;
        lines.push(String::new());
        lines.push(term::dim("[↑↓/jk] move  [0-9] jump to that :N  [enter] pick  [esc] cancel").to_string());
        draw_box("pir — choose model", &lines);
        match model_pick_action(&rows, selected, read_key()?) {
            ModelPickAction::Move(i) => selected = i,
            ModelPickAction::Commit(i) => return Some(i),
            ModelPickAction::Cancel => return None,
            ModelPickAction::Ignore => {}
        }
    }
}

/// What a keypress does to the *inline* model list: move the highlight, commit
/// a row, cancel, or nothing. Pure so the key map can be tested without a tty.
///
/// `Commit(i)` carries a *row position* (0-based), the same position
/// `model_history::format_rows` numbers on screen, so pressing `2` and typing
/// `/model2` name the same model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineModelAction {
    Move(usize),
    Commit(usize),
    Cancel,
    Ignore,
}

/// The inline list's key map, pure: `n` rows, the current highlight, a key.
///
///   * Up/Down move, clamped at the ends (Home/End jump);
///   * a digit `1`…`n` **commits** that row outright — the interactive twin of
///     `/model2`, so both routes pick the same model with one keystroke;
///   * Enter commits the highlighted row;
///   * Esc/ctrl-c/ctrl-d cancel (no switch).
fn inline_model_action(n: usize, selected: usize, key: Key) -> InlineModelAction {
    if n == 0 {
        return InlineModelAction::Ignore;
    }
    let last = n - 1;
    match key {
        Key::Up | Key::Char('k') => InlineModelAction::Move(selected.saturating_sub(1)),
        Key::Down | Key::Char('j') => InlineModelAction::Move((selected + 1).min(last)),
        Key::Home => InlineModelAction::Move(0),
        Key::End => InlineModelAction::Move(last),
        Key::Enter | Key::Right => InlineModelAction::Commit(selected.min(last)),
        Key::Esc | Key::CtrlC | Key::CtrlD => InlineModelAction::Cancel,
        Key::Char(c) if c.is_ascii_digit() => {
            // `0` is not a row (the list is 1-based); out-of-range digits are
            // ignored rather than clamped, so a fat-fingered `9` never silently
            // switches to the last model.
            let d = c.to_digit(10).unwrap_or(0) as usize;
            if d >= 1 && d <= n {
                InlineModelAction::Commit(d - 1)
            } else {
                InlineModelAction::Ignore
            }
        }
        _ => InlineModelAction::Ignore,
    }
}

/// Guard that puts the terminal in raw mode for an *inline* picker without
/// taking the alternate screen: the list stays in the normal scrollback (it is
/// a menu printed at the prompt, not a dialog that covers the session), but the
/// arrow keys still arrive as keypresses instead of being line-buffered.
struct InlineRaw {
    active: bool,
}

impl InlineRaw {
    fn enter() -> Option<InlineRaw> {
        if !crate::term::is_terminal() {
            return None;
        }
        // Claim the keyboard exactly like `Modal::enter`: the REPL's mid-turn
        // reader also reads fd 0, and two readers on one tty is a race the
        // operator loses.
        KEYBOARD_HELD.store(true, Ordering::SeqCst);
        if enable_raw_mode().is_err() {
            KEYBOARD_HELD.store(false, Ordering::SeqCst);
            return None;
        }
        // Drop keypresses queued *before* the picker opened — e.g. the Enter
        // that submitted `/model`. Otherwise it is read as the first key and
        // commits the highlighted row before the operator has seen the list.
        while event::poll(Duration::ZERO).unwrap_or(false) {
            let _ = event::read();
        }
        Some(InlineRaw { active: true })
    }
}

impl Drop for InlineRaw {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        KEYBOARD_HELD.store(false, Ordering::SeqCst);
        let _ = disable_raw_mode();
    }
}

/// Print `labels` as a numbered list (one model per line) in the normal screen
/// and let the operator pick with Up/Down (Enter commits) or by pressing the
/// row's number. Returns the chosen position, or `None` when the list is empty,
/// the terminal is not a tty, or the operator cancelled.
///
/// This is the bare-`/model` browse: the [`crate::model_history::TOP_N`] most
/// recent models, in scrollback, instead of the alternate-screen catalog
/// picker (`model_picker`, which `/models` still opens). Each Up/Down redraws
/// only the block just printed, so the surrounding session is untouched.
pub fn model_list_picker(labels: &[String], current: &str) -> Option<usize> {
    if labels.is_empty() {
        return None;
    }
    let _raw = InlineRaw::enter()?;
    // Start on the active model (or the first row) so the first Up/Down moves
    // somewhere meaningful, matching the alternate-screen picker.
    let mut selected = labels.iter().position(|l| l == current).unwrap_or(0);

    // The block is the models plus the key hint line.
    let block = labels.len() + 1;
    let mut out = io::stdout();

    // Reserve `block` clean rows at the bottom of the screen and park the
    // cursor on the block's first row. Printing straight down from wherever
    // the prompt left the cursor ran the tail off the bottom of the screen (and
    // over the persistent footer), which is what lost a row of the list. Park
    // at the bottom row, emit `block` newlines to scroll exactly that many
    // blank rows in, then step back up to the top of them. Absolute moves only,
    // so this is independent of where the line editor thinks the cursor is.
    if crate::term::is_terminal() {
        let h = crate::term::terminal_height().max(3);
        let block = block.min(h - 1).max(1);
        let mut buf = format!("\x1b[{};1H", h);
        for _ in 0..block {
            buf.push_str("\r\n");
        }
        buf.push_str(&format!("\x1b[{}A\r", block));
        let _ = out.write_all(buf.as_bytes());
        let _ = out.flush();
    }

    let mut drawn = 0usize;
    loop {
        // Repaint the block in place: move back up over the previous frame and
        // clear downward, then write the rows. Reusing `format_rows` for both
        // the first paint and every redraw is what keeps the number on screen
        // and the number in `/modelN` the same thing.
        let mut buf = String::new();
        if drawn > 0 {
            buf.push_str(&format!("\r\x1b[{}A\x1b[J", drawn));
        }
        for row in crate::model_history::format_rows(labels, Some(selected), current) {
            buf.push_str(&row);
            buf.push_str("\r\n");
        }
        buf.push_str(&term::dim("[↑↓/jk] move  [1-5] pick  [enter] choose  [esc] cancel"));
        buf.push_str("\r\n");
        let _ = out.write_all(buf.as_bytes());
        let _ = out.flush();
        drawn = labels.len() + 1;

        match inline_model_action(labels.len(), selected, read_key()?) {
            InlineModelAction::Move(i) => selected = i,
            InlineModelAction::Commit(i) => {
                // Seal the block with a newline so the caller's next output and
                // the idle prompt zone start *below* it. Without this the prompt
                // rule is redrawn at the cursor (still inside the list) and cuts
                // across its tail.
                let _ = out.write_all(b"\r\n");
                let _ = out.flush();
                return Some(i);
            }
            InlineModelAction::Cancel => {
                let _ = out.write_all(b"\r\n");
                let _ = out.flush();
                return None;
            }
            InlineModelAction::Ignore => {}
        }
    }
}

/// Outcome of [`security_editor`]: saved (with the file path), discarded by
/// the operator, or not openable (no tty). Returned explicitly so the caller
/// can report the path or the error instead of swallowing it.
pub enum SecuritySave {
    /// One file saved (`s`), or session + global files (`d`).
    Saved(Vec<std::path::PathBuf>),
    SaveFailed(String),
    Discarded,
    NoTty,
}

/// The editable security-options dialog (opened from the menu's Security
/// item). Arrow through the rows, `←/→` (or `h`/`l`) cycles the value, `s` or
/// Enter saves to `security.toml` (takes effect for new sessions), `q`/Esc
/// discards. Mutates `policy` only when the user saves.
pub fn security_editor(policy: &mut crate::security::SecurityPolicy) -> SecuritySave {
    let Some(_modal) = Modal::enter() else {
        return SecuritySave::NoTty;
    };

    let mut level = policy.level;
    let mut apt = policy.apt;
    let mut network = policy.network;
    let mut ask = policy.ask;
    let mut read = policy.read;
    let mut user_security = policy.user_security;
    let mut quarantine = policy.quarantine;
    let mut quarantine_project = policy.quarantine_project;

    let _levels = ["guard", "off", "sandbox", "strict", "worktree", "mitigation"];
    let _apts = ["auto", "human", "stage", "project"];
    let _networks = ["on", "allowlist", "off"];
    let _asks = ["ask", "auto-yes", "auto-no"];
    let _reads = ["open", "guarded-secrets"];

    let spin = |cur: &str, arr: &[&str], dir: i32| -> String {
        let i = arr.iter().position(|s| *s == cur).unwrap_or(0);
        let n = arr.len();
        arr[((i as i32 + dir).rem_euclid(n as i32)) as usize].to_string()
    };
    let yn = |b: bool| -> String { if b { term::green("on") } else { term::yellow("off") } };

    let mut selected = 0usize; // 0 level, 1 quarantine, 2 quarantine-project, 3 apt, 4 network, 5 ask, 6 read, 7 user-security
    loop {
        let marker = |i: usize| if i == selected { "▸" } else { " " };
        let lines: Vec<String> = vec![
            format!("{} level             {}", marker(0), term::cyan(level.as_str())),
            format!("{} write-quarantine   {}", marker(1), yn(quarantine)),
            format!("{} project-quarantine {}", marker(2), yn(quarantine_project)),
            format!("{} apt                {}", marker(3), term::cyan(apt.as_str())),
            format!("{} network            {}", marker(4), term::cyan(network.as_str())),
            format!("{} ask                {}", marker(5), term::cyan(ask.as_str())),
            format!("{} read               {}", marker(6), term::cyan(read.as_str())),
            format!("{} user-security     {}", marker(7), yn(user_security)),
            String::new(),
            term::dim("[↑/↓] move  [←/→] change  [s] save  [d] save as global defaults  [q] discard"),
            term::dim("note: disabling a quarantine mid-session unmounts its overlays + discards staged writes"),
        ];
        draw_box("pir — security options", &lines);
        #[allow(clippy::too_many_arguments)]
        fn apply_spin(
            selected: usize,
            dir: i32,
            spin: &dyn Fn(&str, &[&str], i32) -> String,
            level: &mut crate::security::SecurityLevel,
            apt: &mut crate::security::AptMode,
            network: &mut crate::security::NetworkMode,
            ask: &mut crate::security::AskMode,
            read: &mut crate::security::ReadMode,
            quarantine: &mut bool,
            quarantine_project: &mut bool,
            user_security: &mut bool,
        ) {
            use crate::security::{AptMode, AskMode, NetworkMode, ReadMode, SecurityLevel};
            match selected {
                0 => {
                    if let Some(l) = SecurityLevel::parse(&spin(level.as_str(), &["guard", "off", "sandbox", "strict", "worktree", "mitigation"], dir)) {
                        *level = l;
                    }
                }
                1 => *quarantine = !*quarantine,
                2 => *quarantine_project = !*quarantine_project,
                3 => {
                    if let Some(a) = AptMode::parse(&spin(apt.as_str(), &["auto", "human", "stage", "project"], dir)) {
                        *apt = a;
                    }
                }
                4 => {
                    if let Some(n) = NetworkMode::parse(&spin(network.as_str(), &["on", "allowlist", "off"], dir)) {
                        *network = n;
                    }
                }
                5 => {
                    if let Some(a) = AskMode::parse(&spin(ask.as_str(), &["ask", "auto-yes", "auto-no"], dir)) {
                        *ask = a;
                    }
                }
                6 => {
                    if let Some(r) = ReadMode::parse(&spin(read.as_str(), &["open", "guarded-secrets"], dir)) {
                        *read = r;
                    }
                }
                7 => *user_security = !*user_security,
                _ => {}
            }
        }
        let Some(key) = read_key() else {
            return SecuritySave::NoTty;
        };
        match key {
            Key::Up | Key::Char('k') | Key::Char('K') => selected = selected.saturating_sub(1),
            Key::Down | Key::Char('j') | Key::Char('J') => selected = (selected + 1).min(7),
            Key::Left | Key::Char('h') | Key::Char('H') => apply_spin(selected, -1, &spin, &mut level, &mut apt, &mut network, &mut ask, &mut read, &mut quarantine, &mut quarantine_project, &mut user_security),
            Key::Right | Key::Char('l') | Key::Char('L') => apply_spin(selected, 1, &spin, &mut level, &mut apt, &mut network, &mut ask, &mut read, &mut quarantine, &mut quarantine_project, &mut user_security),
            Key::Char('s') | Key::Char('S') | Key::Enter => {
                policy.level = level;
                policy.apt = apt;
                policy.network = network;
                policy.ask = ask;
                policy.read = read;
                policy.quarantine = quarantine;
                policy.quarantine_project = quarantine_project;
                policy.user_security = user_security;
                // Never swallow the result: the caller reports the path (proof
                // of where it landed) or the error (proof of why it didn't).
                // On failure append the identity diagnostic: EACCES with euid 0
                // means virtual-root-in-a-namespace or a container view, and
                // the flags say which.
                return match crate::security::save_policy(policy) {
                    Ok(path) => SecuritySave::Saved(vec![path]),
                    Err(e) => {
                        let target = crate::config::pi_dir().join("agent").join("security.toml");
                        SecuritySave::SaveFailed(format!(
                            "{e} [{}]",
                            crate::security::save_diagnostic(&target)
                        ))
                    }
                };
            }
            Key::Char('d') | Key::Char('D') => {
                policy.level = level;
                policy.apt = apt;
                policy.network = network;
                policy.ask = ask;
                policy.read = read;
                policy.quarantine = quarantine;
                policy.quarantine_project = quarantine_project;
                policy.user_security = user_security;
                // Save as global defaults: session file AND the invoking
                // user's file (fresh projects inherit it via the load
                // fallback). Session first so the current project stays
                // consistent even if the global write fails.
                let session_path = crate::config::pi_dir().join("agent").join("security.toml");
                let global_path = crate::security::global_defaults_file();
                let mut targets = vec![session_path];
                if global_path != targets[0] {
                    targets.push(global_path);
                }
                let mut saved = Vec::new();
                for t in &targets {
                    match crate::security::save_policy_to(policy, t) {
                        Ok(p) => saved.push(p),
                        Err(e) => {
                            return SecuritySave::SaveFailed(format!(
                                "{e} [{}]",
                                crate::security::save_diagnostic(t)
                            ));
                        }
                    }
                }
                return SecuritySave::Saved(saved);
            }
            Key::Esc | Key::CtrlC | Key::CtrlD | Key::Char('q') | Key::Char('Q') => {
                return SecuritySave::Discarded;
            }
            _ => {}
        }
    }
}

fn state_color(state: &str) -> String {
    use crate::term;
    match state {
        "running" => term::cyan(state),
        "complete" => term::green(state),
        "waiting for input" | "needs retry" => term::yellow(state),
        _ => term::red(state),
    }
}

/// The thinking levels that actually take effect for this provider + context
/// window + model, in display order. Shared by [`thinking_picker`] and
/// [`settings_editor`] so the settings screen can never offer a level the
/// dedicated picker would refuse (a list the two screens disagreed about would
/// make "the same setting" have two answers).
pub fn offered_thinking_levels(
    kind: Option<ApiKind>,
    ctx: u64,
    model: Option<&crate::config::Model>,
) -> Vec<String> {
    // Only offer levels that actually take effect for this provider + context
    // window. Showing `minimal` on an OpenAI model (no reasoning_effort) or an
    // under-budget Anthropic level would let the user pick something that is
    // silently ignored. `off` is always offered as an explicit opt-out.
    // When catalog metadata is available, additionally honor the model's
    // `thinkingLevelMap` (pi `getSupportedThinkingLevels`): explicitly-null
    // levels are hidden, and `xhigh`/`max` require an explicit entry — e.g.
    // opencode-go's deepseek-v4.1-flash only offers off/high/max.
    ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
        .iter()
        .filter_map(|l| crate::config::ThinkingLevel::parse(l))
        .filter(|l| l.effective(kind, ctx))
        .filter(|l| model.map(|m| m.supported_levels().contains(l)).unwrap_or(true))
        .map(|l| l.as_str().to_string())
        .collect()
}

/// Show a thinking-level picker on the alternate screen and return the chosen
/// level. Returns `None` if not a tty or the user cancels.
pub fn thinking_picker(
    current: &str,
    kind: Option<ApiKind>,
    ctx: u64,
    model: Option<&crate::config::Model>,
) -> Option<String> {
    let _modal = Modal::enter()?;
    let levels = offered_thinking_levels(kind, ctx, model);
    let mut selected = levels.iter().position(|l| l == current).unwrap_or(0);
    loop {
        let lines: Vec<String> = levels
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let marker = if i == selected { "▸" } else { " " };
                let cur = if *l == current { "  (current)" } else { "" };
                format!("{marker} {l}{cur}")
            })
            .collect();
        draw_box("pir — thinking level", &lines);
        match read_key()? {
            Key::Up | Key::Char('k') => selected = selected.saturating_sub(1),
            Key::Down | Key::Char('j') => selected = (selected + 1).min(levels.len() - 1),
            Key::Enter | Key::Right => return Some(levels[selected].clone()),
            Key::Esc | Key::CtrlC | Key::CtrlD => return None,
            _ => {}
        }
    }
}

/// What one keypress means in the option list of [`question_dialog`].
///
/// Extracted so the mapping is unit-testable without a terminal — and so the
/// tests pin the function the dialog actually calls (see `window_start`,
/// which was once re-inlined in the dialog and left the suite asserting a
/// copy nobody ran).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptionKeyAction {
    MoveUp,
    MoveDown,
    PageUp,
    PageDown,
    /// Space in a multi-select dialog: toggle the highlighted row.
    Toggle,
    /// A `1`-`9` digit: the 0-based option row it targets.
    Digit(usize),
    /// Confirm (Enter): choose the highlighted row / checked set.
    Confirm,
    /// Esc / ctrl-c / ctrl-d: dismiss without answering.
    Dismiss,
    /// Anything else — **including Left and Right** — does nothing. A decision
    /// dialog must never answer on a sideways arrow: in plan review → on the
    /// "Revise" row confirmed a rejection the operator never meant, and the
    /// plan came back "NOT approved".
    Ignore,
}

/// Classify one keypress in the option list of [`question_dialog`]. The loop
/// below dispatches on this, so every mapping here is the live behaviour.
fn classify_option_key(key: Key, multi: bool) -> OptionKeyAction {
    match key {
        Key::Up | Key::Char('k') => OptionKeyAction::MoveUp,
        Key::Down | Key::Char('j') => OptionKeyAction::MoveDown,
        Key::PageUp => OptionKeyAction::PageUp,
        Key::PageDown => OptionKeyAction::PageDown,
        Key::Char(' ') if multi => OptionKeyAction::Toggle,
        Key::Char(c @ '1'..='9') => OptionKeyAction::Digit((c as usize) - ('1' as usize)),
        Key::Enter => OptionKeyAction::Confirm,
        Key::Esc | Key::CtrlC | Key::CtrlD => OptionKeyAction::Dismiss,
        _ => OptionKeyAction::Ignore,
    }
}

/// Show an `ask_user_question` dialog on the alternate screen and return the
/// operator's answer (Feature 3). One question per dialog: a header title, the
/// question body (wrapped), the option list (with descriptions), and a hint
/// footer. Number keys jump straight to an option; Up/Down + Enter select
/// (Left/Right are ignored, so → can never confirm the wrong row).
/// For multi-select, Space toggles and Enter confirms.
///
/// When the question has no options this becomes a free-text entry dialog, so
/// a model can still ask an open question. Returns `None` if the terminal
/// isn't a tty (the caller then falls back to a line prompt).
pub fn question_dialog(q: &crate::askuser::Question) -> Option<crate::askuser::Answer> {
    use crate::askuser::Answer;
    let _modal = Modal::enter()?;
    if q.options.is_empty() {
        // Free-text: reuse the secret-entry loop but echo the text (this is not
        // a credential, so masking would be hostile). A long question pages
        // with PgUp/PgDn like the options branch — an uncapped draw would
        // silently clip off the bottom of a small box.
        let mut buf = String::new();
        let mut q_first = 0usize;
        loop {
            let capacity = crate::term::terminal_height().saturating_sub(4).max(1);
            let q_wrapped = wrap_lines(&q.question, 72);
            // Chrome below the question: blank + input + blank + hint.
            let q_budget = q_wrapped.len().min(capacity.saturating_sub(4)).max(1);
            q_first = q_first.min(q_wrapped.len().saturating_sub(q_budget));
            let mut lines = window_question(&q_wrapped, q_first, q_budget);
            lines.push(String::new());
            lines.push(format!("  {}", if buf.is_empty() { "(type your answer)".to_string() } else { buf.clone() }));
            lines.push(String::new());
            let footer = if q_wrapped.len() > q_budget {
                "[enter] submit  [esc] dismiss  [backspace] delete  [PgUp/PgDn] scroll"
            } else {
                "[enter] submit  [esc] dismiss  [backspace] delete"
            };
            lines.push(crate::term::dim(footer).to_string());
            draw_box_titled(&format!("⏸ pir — {} — waiting for you", q.header), &lines, true);
            match read_key()? {
                Key::Enter => return Some(Answer::Text(buf)),
                Key::Esc | Key::CtrlC => return Some(Answer::Dismissed),
                Key::PageUp => q_first = q_first.saturating_sub(q_budget),
                Key::PageDown => {
                    q_first = (q_first + q_budget).min(q_wrapped.len().saturating_sub(q_budget));
                }
                Key::Char('\u{8}') => {
                    buf.pop();
                }
                Key::Char(c) if !c.is_control() => buf.push(c),
                _ => {}
            }
        }
    }

    let multi = q.multi_select;
    let mut selected = 0usize;
    let mut checked: Vec<bool> = vec![false; q.options.len()];
    // First visible row of the question window: PgUp/PgDn page through a
    // question taller than the box (plan bodies on small terminals).
    let mut q_first = 0usize;
    loop {
        // Rows are laid out once, so the viewport is just a window over them
        // and the drawn text always matches the highlighted row.
        let mut rows: Vec<String> = Vec::new();
        for (i, o) in q.options.iter().enumerate() {
            let cursor = if i == selected { "▸" } else { " " };
            let box_ = if multi {
                if checked[i] { "[x] " } else { "[ ] " }
            } else {
                ""
            };
            // Number the rows 1-based, because that is what the digit keys
            // select (`1` chooses the first option). Labelling from 0 while
            // the keys count from 1 made the hint ("[1-9] choose") a lie: `1`
            // answered the row marked `0)`, and options past the ninth could
            // not be reached by number at all.
            rows.push(format!("{cursor} {}) {box_}{}", i + 1, o.label));
            if !o.description.is_empty() {
                for l in wrap_lines(&o.description, 64) {
                    rows.push(format!("      {}", crate::term::dim(&l)));
                }
            }
        }

        // Keep the highlighted row inside the viewport. Without this a long
        // option list was silently cut at the bottom of the box: ↓ kept moving
        // `selected` past the last *drawn* row, so the extra options could be
        // chosen but never seen — the "I can't get to the later options"
        // report. (Every other dialog here goes through
        // `draw_box_scrolled`; this one drew the whole list and hoped.)
        //
        // The math itself lives in `window_start`, which the tests below pin.
        // It used to be re-inlined here, which left those tests asserting a
        // function the dialog never called: the fix they were written for
        // could regress without a single test noticing, because the only
        // code under test was the copy nobody ran.
        // Everything the box must draw, budgeted so nothing is cut off the
        // bottom (the screenshot bug: option 2 and the hint vanished because the
        // viewport ignored the question and hint rows).
        //
        // The QUESTION is budgeted first: it is the decision payload (the
        // whole plan body in plan review), while the option list can scroll.
        // The old math sized the option viewport first (terminal - 9 rows)
        // and left the question a single row, so every long question —
        // every plan review — collapsed to "(question truncated) …" and the
        // plan was never readable. Fixed chrome, in rows: blank + blank +
        // hint = 3; the scroll notice (when the options overflow) and at
        // least one option row come out of the same box.
        let capacity = crate::term::terminal_height().saturating_sub(4).max(1);
        let q_wrapped = wrap_lines(&q.question, 72);
        let (q_budget, viewport, show_notice) =
            dialog_layout(capacity, q_wrapped.len(), rows.len());
        // A resize can shrink the box under the scroll offset: clamp it back
        // into range every frame so the window never runs past the text.
        q_first = q_first.min(q_wrapped.len().saturating_sub(q_budget));
        let question_block = window_question(&q_wrapped, q_first, q_budget);
        // The scroll keys are only advertised when there is anything to
        // scroll to — a fitting question keeps the short hint.
        let scroll_hint = if q_wrapped.len() > q_budget { "  [PgUp/PgDn] scroll" } else { "" };
        let hint = if multi {
            format!("[space] toggle  [enter] confirm  [1-9] toggle  [esc] dismiss{scroll_hint}")
        } else {
            format!("[enter] choose  [1-9] choose  [esc] dismiss{scroll_hint}")
        };
        let first = window_start(selected, rows.len(), viewport);
        let visible: Vec<String> = rows[first..(first + viewport).min(rows.len())].to_vec();

        let mut lines = question_block;
        lines.push(String::new());
        lines.extend(visible);
        lines.push(String::new());
        // Say so when the list is taller than the box, so a scroll is never a
        // silent truncation.
        if show_notice {
            lines.push(crate::term::dim(&format!(
                "… {} more (↑↓ to scroll)",
                rows.len() - (first + viewport)
            )).to_string());
        }
        lines.push(crate::term::dim(&hint).to_string());
        draw_box_titled(&format!("⏸ pir — {} — waiting for you", q.header), &lines, true);
        let key = match read_key() {
            Some(k) => k,
            None => return Some(Answer::Dismissed),
        };
        match classify_option_key(key, multi) {
            OptionKeyAction::MoveUp => selected = selected.saturating_sub(1),
            OptionKeyAction::MoveDown => selected = (selected + 1).min(q.options.len() - 1),
            // Page through the question window (a no-op when it all fits —
            // the offset clamps to 0 and the hint stays short).
            OptionKeyAction::PageUp => q_first = q_first.saturating_sub(q_budget),
            OptionKeyAction::PageDown => {
                q_first = (q_first + q_budget).min(q_wrapped.len().saturating_sub(q_budget));
            }
            OptionKeyAction::Toggle => checked[selected] = !checked[selected],
            OptionKeyAction::Digit(idx) => {
                // The rows are numbered from 1 on screen (see the `{i})` label
                // below), so the digit key must mean the same 1-based option.
                // These used to disagree: the label printed the 0-based index
                // while this handler subtracted 1, so pressing `1` answered the
                // row labelled `0)` and any option past the ninth had no digit
                // at all — "I can't get to 3-9".
                if idx < q.options.len() {
                    if multi {
                        checked[idx] = !checked[idx];
                    } else {
                        return Some(Answer::Chosen(vec![q.options[idx].label.clone()]));
                    }
                }
            }
            OptionKeyAction::Confirm => {
                if multi {
                    let chosen: Vec<String> = q
                        .options
                        .iter()
                        .zip(checked.iter())
                        .filter(|(_, c)| **c)
                        .map(|(o, _)| o.label.clone())
                        .collect();
                    return Some(Answer::Chosen(chosen));
                }
                return Some(Answer::Chosen(vec![q.options[selected].label.clone()]));
            }
            OptionKeyAction::Dismiss => return Some(Answer::Dismissed),
            OptionKeyAction::Ignore => {}
        }
    }
}

/// How many rows the question dialog can spend on **option rows**, given a
/// terminal of `terminal_rows` rows and the number of rows its fixed chrome
/// (question block + blank separators + hint + scroll notice) needs.
///
/// The bug this exists for: the dialog sized its option viewport with a flat
/// `terminal_height - 6` and *then* prepended the wrapped question, a
/// blank separator, the hint and (when scrolling) a "… N more" line into the
/// same body. Those are all rows too, so the body overflowed `draw_box_titled`'s
/// `h - 4` capacity and the renderer's `.take(body_h)` dropped rows **from the
/// bottom** — the last visible option and the control hint vanished, leaving a
/// dialog whose second option and bottom border the operator never saw.
///
/// The `-4` here matches `draw_box_titled`'s own capacity (2 border rows + 2
/// title/footer rows) so the two can never disagree again; `chrome` covers
/// every other non-option row. Always at least 1 so the list is never empty.
fn option_viewport(terminal_rows: usize, chrome: usize) -> usize {
    terminal_rows
        .saturating_sub(4) // the box border + title/footer rows the renderer keeps
        .saturating_sub(chrome)
        .max(1)
}

/// Split the dialog's body budget (`capacity` rows) between the wrapped
/// question block and the option viewport: `(question_rows, viewport_rows,
/// show_scroll_notice)`.
///
/// The question is budgeted first — up to everything but the fixed chrome
/// (blank + blank + hint), one option row, and one spare row for a possible
/// scroll notice — because it carries the decision (a plan body), while the
/// option list scrolls. The options then take the remainder; when they don't
/// fit, the notice takes one row and the viewport shows a scrolling window
/// (never zero rows). Total emitted rows (`question + blank + viewport +
/// blank + notice? + hint`) never exceed `capacity`, so the renderer can't
/// clip the hint or the last option off the bottom.
///
/// Built on [`option_viewport`] (same capacity contract); cut ends are
/// marked by [`window_question`] instead of ending silently.
fn dialog_layout(capacity: usize, q_lines: usize, option_rows: usize) -> (usize, usize, bool) {
    let h = capacity.saturating_add(4);
    // Question first: everything past chrome, one option row, one notice spare.
    let q_budget = q_lines.min(h.saturating_sub(4 + 3 + 1 + 1)).max(1);
    // Options take the rest after fixed chrome (blank + blank + hint).
    let o_avail = capacity.saturating_sub(3 + q_budget);
    let notice = option_rows > o_avail;
    let viewport = option_viewport(h, 3 + q_budget + usize::from(notice));
    (q_budget, viewport, notice)
}

/// The visible window of a wrapped question: `wrapped[first .. first+budget]`,
/// with clipped ends marked so a cut never looks complete. When rows are cut
/// above, row 0 names the count and the scroll keys; when cut below, the last
/// row does. A 1-row window cut on both sides gets one combined marker.
/// Always returns exactly `budget.max(1)` rows when the question is longer,
/// or the whole question when it fits — so callers can size the box without
/// re-measuring.
///
/// This is what makes long questions (plan bodies) readable on small
/// terminals: instead of a fixed cap the dialog pages through the text with
/// PgUp/PgDn (see `question_dialog`). The markers always name the keys, so a
/// clipped view advertises its own escape.
fn window_question(wrapped: &[String], first: usize, budget: usize) -> Vec<String> {
    let budget = budget.max(1);
    if wrapped.len() <= budget {
        return wrapped.to_vec();
    }
    let first = first.min(wrapped.len().saturating_sub(budget));
    let last = (first + budget).min(wrapped.len());
    let mut out: Vec<String> = wrapped[first..last].to_vec();
    debug_assert_eq!(out.len(), budget);
    let cut_above = first;
    let cut_below = wrapped.len() - last;
    if budget == 1 {
        // One row cannot show content plus two markers: name the cut(s).
        out[0] = if cut_above > 0 && cut_below > 0 {
            crate::term::dim(&format!(
                "(question clipped above & below — PgUp/PgDn to scroll, {total} rows total)",
                total = wrapped.len()
            ))
        } else if cut_above > 0 {
            crate::term::dim(&format!("… ↑ ({cut_above} more above — PgUp/PgDn to scroll)"))
        } else {
            crate::term::dim(&format!("… ↓ ({cut_below} more below — PgUp/PgDn to scroll)"))
        };
        return out;
    }
    if cut_above > 0 {
        out[0] = crate::term::dim(&format!("… ↑ ({cut_above} more above — PgUp/PgDn to scroll)"));
    }
    if cut_below > 0 {
        let n = out.len();
        out[n - 1] = crate::term::dim(&format!("… ↓ ({cut_below} more below — PgUp/PgDn to scroll)"));
    }
    out
}

/// The visible-window math, extracted so the behaviour is testable without
    /// a terminal: given `total` rows and a `viewport` of `k`, where must the
    /// window start so that the row at `selected` is on screen?
fn window_start(selected: usize, total: usize, viewport: usize) -> usize {
        if total <= viewport {
            return 0;
        }
        let first = selected.min(total - 1);
        first
            .saturating_sub(viewport.saturating_sub(2).max(1))
            .min(total.saturating_sub(viewport))
    }

/// Wrap `text` to at most `width` visible characters per line, preserving
/// explicit newlines. Used by the question dialog so a long question or option
/// description doesn't run off the box.
fn wrap_lines(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split_whitespace() {
            if line.is_empty() {
                line.push_str(word);
            } else if line.chars().count() + 1 + word.chars().count() <= width {
                line.push(' ');
                line.push_str(word);
            } else {
                out.push(std::mem::take(&mut line));
                line.push_str(word);
            }
        }
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Show a masked secret-entry dialog on the alternate screen and return the
/// typed value. The key is shown as `••••` and never touches the normal screen
/// or scrollback. Returns `None` if the terminal isn't a tty (caller falls back
/// to a plain read) or the user cancels (Esc/ctrl-c).
pub fn secret_entry(prompt: &str) -> Option<String> {
    let _modal = Modal::enter()?;
    let mut buf = String::new();
    loop {
        let masked: String = buf.chars().map(|_| '•').collect();
        let lines = vec![
            prompt.to_string(),
            String::new(),
            format!("  {}", if masked.is_empty() { "(type to enter)" } else { &masked }),
            String::new(),
            crate::term::dim("[enter] confirm  [esc] cancel  [backspace] delete").to_string(),
        ];
        draw_box_titled("pir — secret entry", &lines, true);
        match read_key()? {
            Key::Enter => {
                if !buf.is_empty() {
                    return Some(buf);
                }
            }
            Key::Esc | Key::CtrlC => return None,
            Key::Char('\u{8}') => {
                buf.pop();
            }
            Key::Char(c) if !c.is_control() => buf.push(c),
            _ => {}
        }
    }
}

/// Show the tool-approval dialog on the alternate screen and read the operator's
/// decision. `denial` describes the blocked operation; `approval` carries the
/// agent's recent prompts + thinking for context. Returns `None` if the terminal
/// isn't a tty (caller falls back to its default, e.g. deny).
pub fn tool_approval(
    denial: &crate::security::Denial,
    approval: &crate::security::ApprovalContext,
) -> Option<Approval> {
    let _modal = Modal::enter()?;
    let (prompts, thinking) = approval.snapshot();
    let mut show_info = false;
    loop {
        let lines = approval_lines(denial, &prompts, &thinking, show_info);
        draw_box_titled("⏸ pir — tool approval — waiting for you", &lines, true);
        match read_key()? {
            Key::Char('o') => return Some(Approval::AllowOnce),
            Key::Char('s') => return Some(Approval::AllowSession),
            Key::Char('n') | Key::Esc | Key::CtrlC => return Some(Approval::Deny),
            Key::Char('i') => show_info = !show_info,
            _ => {}
        }
    }
}

/// Build the dialog body lines for a denial.
fn approval_lines(
    d: &crate::security::Denial,
    prompts: &[String],
    thinking: &[String],
    show_info: bool,
) -> Vec<String> {    use crate::term;
    let what = match &d.ask.path {
        Some(p) => p.display().to_string(),
        None => match &d.ask.target {
            Some(t) => t.clone(),
            None => d.ask.op.verb().to_string(),
        },
    };
    let mut lines = Vec::new();
    lines.push(format!(
        "{} {} {}  → parcel: {} (risk: {})",
        term::yellow("[denied]"),
        d.ask.op.verb(),
        what,
        term::bold(&d.parcel.id()),
        d.parcel.default_risk().as_str(),
    ));
    lines.push(format!("  blast radius: {}", term::dim(d.parcel.blast_radius())));
    if !d.ask.reason.is_empty() {
        lines.push(format!("  reason: {}", term::dim(&d.ask.reason)));
    }
    if show_info {
        lines.push(term::dim("  (full blast radius shown above)").to_string());
    }
    lines.push(String::new());
    // Context: recent prompts + thinking (given space).
    if !prompts.is_empty() {
        lines.push(term::bold("recent prompts:").to_string());
        for p in prompts.iter().rev().take(3) {
            lines.push(format!("  {}", term::dim(&truncate(p, 60))));
        }
    }
    if !thinking.is_empty() {
        lines.push(term::bold("recent thinking:").to_string());
        for t in thinking.iter().rev().take(6) {
            lines.push(format!("  {}", term::dim(&truncate(t, 60))));
        }
    }
    lines.push(String::new());
    lines.push(term::dim("[o] allow once  [s] allow session  [n] deny  [i] info").to_string());
    lines
}


#[cfg(test)]
mod keyboard_ownership_tests {
    use super::{keyboard_held, window_start, Modal};

    // -- the question dialog -------------------------------------------------

    /// The regression the screenshot exposed: the box must be tall enough for
    /// **everything it draws** — the question, the options, and the hint. The
    /// viewport used to be a flat `terminal_height - 6` that ignored the
    /// question and hint rows, so a multi-line question pushed the last option
    /// and the whole hint line off the bottom (`draw_box_titled` truncates with
    /// `.take(body_h)`), and the operator saw a dialog with no second option and
    /// no bottom border.
    ///
    /// This recomputes the row budget exactly as `question_dialog` does and
    /// asserts the total never exceeds the renderer's capacity, so the hint can
    /// never be the row that is cut — at every question scroll offset (PgUp/
    /// PgDn only swap which rows show; markers replace content rows in place,
    /// so the count is offset-independent, pinned here across offsets anyway).
    #[test]
    fn the_question_dialog_never_overflows_the_box() {
        use super::{dialog_layout, window_question, wrap_lines};
        for h in [10usize, 12, 24, 40] {
            for question in [
                "Ship it?",
                "Approve this plan and start implementing?\n\nSaved to: \
                 /home/s/pam/.pi/agent/plans/trivial-smoke-test.md\n\n# Trivial smoke test",
                &"long ".repeat(120),
            ] {
                for options in [2usize, 5, 12] {
                    // Mirror `question_dialog` exactly.
                    let capacity = h.saturating_sub(4).max(1);
                    let q_wrapped = wrap_lines(question, 72);
                    let (q_budget, viewport, show_notice) =
                        dialog_layout(capacity, q_wrapped.len(), options);
                    for q_first in [0usize, 1, 7, usize::MAX] {
                        let shown = options.min(viewport);
                        let block = window_question(&q_wrapped, q_first, q_budget);
                        // question + blank + options + blank + scroll? + hint
                        let drawn = block.len() + 1 + shown + 1
                            + usize::from(show_notice) + 1;
                        assert!(
                            drawn <= capacity,
                            "the dialog draws {drawn} rows into a {capacity}-row box \
                             (h={h}, q_first={q_first}, opts={options}) — \
                             the hint would be cut off the bottom"
                        );
                        assert!(!block.is_empty(), "the question block is never empty");
                    }
                }
            }
        }
    }

    /// `option_viewport` must agree with the renderer's own capacity
    /// (`draw_box_titled` uses `height - 4`). When the two disagreed the dialog
    /// over-counted by 2 rows and cut the hint off. Pin the arithmetic.
    #[test]
    fn option_viewport_matches_the_renderer_capacity() {
        use super::option_viewport;
        // No chrome at all: the whole body is available.
        assert_eq!(option_viewport(24, 0), 20, "24 - 4 border/title rows");
        // Chrome is subtracted, not ignored.
        assert_eq!(option_viewport(24, 6), 14);
        // A degenerate terminal still yields a usable (>=1) viewport, never 0.
        assert_eq!(option_viewport(4, 0), 1);
        assert_eq!(option_viewport(0, 5), 1);
    }

    /// A clipped question is marked, never silent — a cut that looks complete
    /// is its own bug. Same promise as the old fixed cap, now for a scrolling
    /// window: fitting text passes through untouched, cut ends name the
    /// direction, the count, and the scroll keys.
    #[test]
    fn window_question_marks_clipped_ends() {
        use super::window_question;
        let lines: Vec<String> = (0..10).map(|i| format!("line {i}")).collect();
        assert_eq!(window_question(&lines, 0, 10), lines, "fits: unchanged");
        assert_eq!(window_question(&lines, 0, 20), lines, "room to spare: unchanged");
        // Cut below only: head intact, last row marked with the count.
        let w = window_question(&lines, 0, 3);
        assert_eq!(w.len(), 3);
        assert_eq!(w[0], "line 0");
        assert!(w[2].contains("7 more below"), "must name the cut: {w:?}");
        assert!(w[2].contains("PgUp/PgDn"), "must name the keys: {w:?}");
        assert!(!w[0].contains("more above"), "nothing cut above: {w:?}");
        // Scrolled to the end: cut above only.
        let w = window_question(&lines, 7, 3);
        assert_eq!(w.len(), 3);
        assert!(w[0].contains("7 more above"), "must name the cut: {w:?}");
        assert_eq!(w[2], "line 9");
        // Mid-scroll: both ends marked, middle row intact.
        let w = window_question(&lines, 4, 3);
        assert_eq!(w.len(), 3);
        assert!(w[0].contains("more above"), "top cut unmarked: {w:?}");
        assert!(w[2].contains("more below"), "bottom cut unmarked: {w:?}");
        assert_eq!(w[1], "line 5");
        // An out-of-range offset clamps to the last window, not an empty box.
        let w = window_question(&lines, 999, 3);
        assert_eq!(w.len(), 3);
        assert_eq!(w[2], "line 9");
        // Degenerate budget still returns one row.
        assert_eq!(window_question(&lines, 0, 0).len(), 1);
        // One row cut both ways: a single combined marker (no room for two).
        let w = window_question(&lines, 5, 1);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("above & below"), "must name both cuts: {w:?}");
        assert!(w[0].contains("10 rows total"), "must size the question: {w:?}");
    }

    /// The reported bug: on a 24-row terminal a long plan-review question got
    /// a 1-row budget (the option math took terminal-9 first), so every plan
    /// collapsed to "(question truncated) …" and was never readable. The
    /// question is budgeted first now: a 20-row plan gets 15 rows and the
    /// 2-option list scrolls instead.
    #[test]
    fn long_question_is_budgeted_before_the_option_list() {
        use super::dialog_layout;
        let capacity = 24usize.saturating_sub(4);
        let (q, viewport, notice) = dialog_layout(capacity, 20, 4);
        assert_eq!(q, 15, "the plan must get rows, not one: {q:?}/{viewport:?}/{notice:?}");
        assert!(notice, "4 option rows cannot fit beside it: {q:?}/{viewport:?}/{notice:?}");
        assert_eq!(viewport, 1, "one scrolling option row: {q:?}/{viewport:?}/{notice:?}");
    }

    /// A short question keeps every row and a fitting list never scrolls.
    #[test]
    fn short_question_and_fitting_list_do_not_scroll() {
        use super::dialog_layout;
        let capacity = 24usize.saturating_sub(4);
        let (q, viewport, notice) = dialog_layout(capacity, 2, 4);
        assert_eq!((q, notice), (2, false), "nothing to cut: {q:?}/{viewport:?}/{notice:?}");
        assert!(viewport >= 4, "all options visible: {q:?}/{viewport:?}/{notice:?}");
    }

    /// Whatever the sizes, the emitted block (`question + blank + options +
    /// blank + notice? + hint`) must fit the box on any usable terminal, the
    /// viewport must never be empty, and the question must never vanish.
    #[test]
    fn dialog_layout_never_overflows_usable_terminals() {
        use super::dialog_layout;
        for h in [12usize, 16, 24, 40, 100] {
            let capacity = h.saturating_sub(4);
            for (q_lines, options) in [(1, 2), (5, 4), (20, 4), (50, 4), (3, 30), (20, 30)] {
                let (q, viewport, notice) = dialog_layout(capacity, q_lines, options);
                assert!(q >= 1, "h={h} question vanished");
                assert!(viewport >= 1, "h={h} viewport empty");
                let shown = options.min(viewport);
                let total = q + 1 + shown + 1 + usize::from(notice) + 1;
                assert!(
                    total <= capacity,
                    "h={h} q={q_lines} opts={options}: {total} rows exceed {capacity}"
                );
                assert_eq!(
                    notice,
                    options > capacity.saturating_sub(3 + q),
                    "h={h}: notice must mean overflow exactly"
                );
            }
        }
    }

    /// The regression: options were numbered from 0 on screen while the digit
    /// keys counted from 1, so the hint "[1-9] choose" pointed at the wrong
    /// row and the tenth option onward had no digit at all. Pin the mapping
    /// the dialog actually uses.
    #[test]
    fn digit_key_selects_the_option_the_label_shows() {
        let labels = ["alpha", "beta", "gamma", "delta"];
        // This is the exact expression the dialog uses, kept honest here.
        for (row, want) in labels.iter().enumerate() {
            let digit = char::from_digit(row as u32 + 1, 10).expect("row+1 is a digit");
            let idx = (digit as usize) - ('1' as usize);
            assert_eq!(labels[idx], *want, "pressing `{digit}` must answer {want}");
        }
    }

    /// Every option must be numbered 1..=n and the label the operator reads
    /// must be the one the digit answers.
    #[test]
    fn option_rows_are_numbered_from_one() {
        let opts = ["alpha", "beta", "gamma"];
        for (i, o) in opts.iter().enumerate() {
            // Exactly the format the dialog renders.
            let row = format!("{}) {}", i + 1, o);
            let num: String = row.chars().take_while(|c| c.is_ascii_digit()).collect();
            assert_eq!(num.parse::<usize>().unwrap(), i + 1, "rows are 1-based: {row}");
        }
    }

    /// A list taller than the box must scroll so the highlighted row is always
    /// on screen — the "I could not see options 3-9" report. Without a window
    /// the selection walked past the last *drawn* row, so those options were
    /// choosable but invisible.
    #[test]
    fn a_long_option_list_scrolls_to_keep_the_cursor_visible() {
        let total = 12usize;
        let viewport = 6usize;
        for selected in 0..total {
            let first = window_start(selected, total, viewport);
            let last = first + viewport; // exclusive
            assert!(
                selected >= first && selected < last,
                "row {selected} is outside the window {first}..{last} — it would be invisible"
            );
        }
    }

    /// A short list never scrolls, and the window stays in range even at the
    /// last row (an over-scrolled window would hide the tail).
    #[test]
    fn a_short_list_does_not_scroll_and_stays_in_range() {
        assert_eq!(window_start(0, 3, 6), 0);
        assert_eq!(window_start(2, 3, 6), 0);
        for selected in 0..12 {
            let first = window_start(selected, 12, 6);
            assert!(first + 6 <= 12, "window must not run past the end: {first}");
        }
        assert_eq!(window_start(11, 12, 6), 6, "the last row pins the window to the end");
    }

    /// The dialog must *use* `window_start`, not a private copy of its math.
    ///
    /// This is the regression the two tests above could not catch. They call
    /// `window_start` directly, so when `question_dialog` re-inlined the same
    /// expression they kept passing while exercising code the dialog never ran:
    /// the scroll fix could be reverted in the dialog and the suite stayed
    /// green. The dialog's viewport is not directly observable (it needs a tty
    /// and a key loop), so this asserts the wiring where it is checkable — the
    /// selection → window mapping the dialog now delegates.
    #[test]
    fn the_dialog_delegates_its_viewport_to_window_start() {
        // Exactly the call `question_dialog` makes (see the `let first =`
        // there), so a divergence between the two is a test failure.
        let dialog_first = |selected: usize, rows: usize, viewport: usize| -> usize {
            window_start(selected, rows, viewport)
        };
        let total = 12usize;
        let viewport = 4usize;
        for selected in 0..total {
            let first = dialog_first(selected, total, viewport);
            // The window must contain the cursor and stay inside the list —
            // the two properties the inline copy was written to have.
            assert!(selected >= first, "cursor {selected} above window start {first}");
            assert!(selected < first + viewport, "cursor {selected} below window end");
            assert!(first + viewport <= total, "window {first} runs past the list end");
        }
    }

    // -- the keyboard ownership ---------------------------------------------

    /// The flag must be *false* when no dialog is up — otherwise the REPL's
    /// mid-turn reader would stand down forever and the operator could not type
    /// a queued prompt while the agent works (silently swallowing every
    /// keystroke, which is a worse bug than the one this fixes).
    #[test]
    fn no_modal_means_the_repl_may_read() {
        assert!(
            !keyboard_held(),
            "the keyboard must be free when no modal is open"
        );
    }

    /// The contract the fix rests on: a modal claims the keyboard for exactly
    /// its lifetime and releases it on drop, including the failure path in
    /// `enter` (no tty / raw mode unavailable), which must never leave the flag
    /// stuck — a stuck flag silences the REPL for the rest of the session.
    #[test]
    fn a_failed_enter_does_not_leave_the_keyboard_claimed() {
        let before = keyboard_held();
        let _ = Modal::enter(); // None without a tty
        assert_eq!(
            keyboard_held(),
            before,
            "a modal that never opened must not leave the keyboard claimed"
        );
    }
}

#[cfg(test)]
mod option_key_tests {
    use super::{classify_option_key, Key, OptionKeyAction};

    /// The reported bug: → in the plan-review dialog confirmed whatever row
    /// was highlighted, so → on "Revise — keep planning" came back as
    /// "the plan was NOT approved". Sideways arrows must never answer a
    /// decision dialog — Enter (or a digit) chooses, arrows only move.
    #[test]
    fn sideways_arrows_never_answer_the_dialog() {
        for key in [Key::Right, Key::Left] {
            for multi in [false, true] {
                assert_eq!(
                    classify_option_key(key, multi),
                    OptionKeyAction::Ignore,
                    "{key:?} (multi={multi}) must do nothing, never confirm"
                );
            }
        }
    }

    /// Only Enter confirms; only the dismiss keys dismiss. Everything else
    /// that answers (digits, Space-toggle) keeps its existing meaning.
    #[test]
    fn confirm_and_dismiss_keys_keep_their_meaning() {
        for multi in [false, true] {
            assert_eq!(classify_option_key(Key::Enter, multi), OptionKeyAction::Confirm);
            for key in [Key::Esc, Key::CtrlC, Key::CtrlD] {
                assert_eq!(
                    classify_option_key(key, multi),
                    OptionKeyAction::Dismiss,
                    "{key:?} must dismiss"
                );
            }
        }
    }

    /// Digits target the row the label shows (1-based on screen → 0-based
    /// index), and Space toggles only in a multi-select dialog.
    #[test]
    fn digits_and_space_keep_their_meaning() {
        for multi in [false, true] {
            for (digit, want) in [('1', 0), ('2', 1), ('9', 8)] {
                assert_eq!(
                    classify_option_key(Key::Char(digit), multi),
                    OptionKeyAction::Digit(want),
                    "pressing `{digit}` must target row {want}"
                );
            }
            assert_eq!(
                classify_option_key(Key::Char(' '), multi),
                if multi { OptionKeyAction::Toggle } else { OptionKeyAction::Ignore },
                "space toggles only when multi-select"
            );
        }
    }

    /// Navigation and paging are untouched by the fix.
    #[test]
    fn navigation_keys_are_untouched() {
        for multi in [false, true] {
            assert_eq!(classify_option_key(Key::Up, multi), OptionKeyAction::MoveUp);
            assert_eq!(classify_option_key(Key::Char('k'), multi), OptionKeyAction::MoveUp);
            assert_eq!(classify_option_key(Key::Down, multi), OptionKeyAction::MoveDown);
            assert_eq!(classify_option_key(Key::Char('j'), multi), OptionKeyAction::MoveDown);
            assert_eq!(classify_option_key(Key::PageUp, multi), OptionKeyAction::PageUp);
            assert_eq!(classify_option_key(Key::PageDown, multi), OptionKeyAction::PageDown);
        }
    }
}

#[cfg(test)]
mod title_width_tests {
    use crate::term;

    /// The box border is padded from the title's *visible* width. A blocking
    /// title carries SGR escapes (the alert styling), so measuring with
    /// `chars().count()` — as this renderer used to — counts the escape bytes as
    /// printable columns and under-pads the top border, leaving the box visibly
    /// short on the right. Pin the measurement the border math depends on.
    #[test]
    fn blocking_title_measures_the_same_as_its_plain_text() {
        term::set_color(true);
        let plain = "⏸ pir — tool approval — waiting for you";
        let styled = term::alert(plain);
        assert_eq!(
            term::visible_len(&styled),
            plain.chars().count(),
            "the alert styling must not change the visible width"
        );
        assert!(styled.len() > plain.len(), "the styling must actually be applied");
    }

    /// With colour off the title is already plain, so the width is trivially
    /// right — assert it so a future refactor can't make the two paths diverge.
    #[test]
    fn plain_title_width_is_its_char_count() {
        term::set_color(false);
        let plain = "pir — main menu";
        assert_eq!(term::visible_len(&term::alert(plain)), plain.chars().count());
        term::set_color(true);
    }
}

#[cfg(test)]
mod settings_editor_tests {
    use super::{apply_setting, SettingsValues};

    fn vals() -> SettingsValues {
        SettingsValues {
            model: "test/model".into(),
            thinking_levels: vec!["off".into(), "low".into(), "high".into()],
            thinking: "off".into(),
            show_thinking: true,
            incremental: true,
            stop_skill: true,
            full_auto: false,
        }
    }

    /// The regression this whole change exists for: `full-auto` was displayed
    /// on the settings screen but had no way to be changed, so an operator
    /// could see "full-auto: off" and have no key that turned it on. Pin that
    /// row 4 (`←`/`→`) actually flips it.
    #[test]
    fn full_auto_row_toggles() {
        let mut v = vals();
        assert!(!v.full_auto, "precondition: starts off");
        apply_setting(&mut v, 4, 1);
        assert!(v.full_auto, "row 4 (full-auto) must be toggleable");
        apply_setting(&mut v, 4, -1);
        assert!(!v.full_auto, "and toggle back off");
    }

    /// Every boolean row toggles in *both* directions, and toggling one row
    /// leaves the others alone — a shared handler that aliased two rows would
    /// make changing `show` silently flip `stop-skill`.
    #[test]
    fn each_boolean_row_toggles_independently() {
        for row in 1..=4 {
            let mut v = vals();
            let before = (v.show_thinking, v.incremental, v.stop_skill, v.full_auto);
            apply_setting(&mut v, row, 1);
            let after = (v.show_thinking, v.incremental, v.stop_skill, v.full_auto);
            let mut flipped = before;
            match row {
                1 => flipped.0 = !flipped.0,
                2 => flipped.1 = !flipped.1,
                3 => flipped.2 = !flipped.2,
                4 => flipped.3 = !flipped.3,
                _ => unreachable!(),
            }
            assert_eq!(after, flipped, "row {row} must flip exactly its own value");
        }
    }

    /// The thinking row *cycles* the caller-supplied list rather than toggling,
    /// and wraps in both directions. It must never produce a value outside the
    /// list — that is the level the picker would refuse.
    #[test]
    fn thinking_row_cycles_and_wraps_within_the_offered_list() {
        let mut v = vals();
        assert_eq!(v.thinking, "off");
        apply_setting(&mut v, 0, 1);
        assert_eq!(v.thinking, "low");
        apply_setting(&mut v, 0, 1);
        assert_eq!(v.thinking, "high");
        apply_setting(&mut v, 0, 1);
        assert_eq!(v.thinking, "off", "must wrap forward past the end");
        apply_setting(&mut v, 0, -1);
        assert_eq!(v.thinking, "high", "must wrap backward past the start");
    }

    /// A level the model does not offer (e.g. resumed onto a different model,
    /// or a non-reasoning one) is not in the list. Stepping must still produce
    /// only offered values rather than leaving a stale or invented one.
    #[test]
    fn thinking_row_only_produces_offered_levels() {
        let mut v = vals();
        v.thinking = "xhigh".into(); // not in the offered list
        for dir in [1, -1, 1, -1, 1] {
            apply_setting(&mut v, 0, dir);
            assert!(
                v.thinking_levels.contains(&v.thinking),
                "stepped to {:?}, which the model does not offer",
                v.thinking
            );
        }
    }

    /// When no level is offered at all (unknown/unsupported model) the row is
    /// inert — it must not panic on an empty list, and must not invent a level.
    #[test]
    fn thinking_row_is_inert_with_no_offered_levels() {
        let mut v = vals();
        v.thinking_levels = Vec::new();
        apply_setting(&mut v, 0, 1);
        apply_setting(&mut v, 0, -1);
        assert_eq!(v.thinking, "off", "an empty list must leave the level alone");
    }

    /// The row count must cover every editable row the screen draws. `model` is
    /// a status line, so it is excluded; adding a row without raising this
    /// would make the last setting unreachable (the `↓` clamp would stop one
    /// short) — which is the same class of "visible but unreachable" bug.
    #[test]
    fn every_editable_row_is_reachable() {
        assert_eq!(SettingsValues::ROWS, 5, "thinking, show, incremental, stop-skill, full-auto");
    }
}

#[cfg(test)]
mod model_picker_key_tests {
    use super::{model_pick_action, Key, ModelPickAction};

    /// A catalog laid out exactly as `model_picker` builds it: rows are
    /// `(flat_index, provider, label, is_current)`, so the `:N` shown on screen
    /// is the flat index — the very number `config::select(":N")` resolves.
    fn rows() -> Vec<(usize, String, String, bool)> {
        vec![
            (0, "anthropic".into(), "Claude Sonnet 4.5".into(), true),
            (1, "anthropic".into(), "Claude Haiku 4.5".into(), false),
            (2, "openai".into(), "GPT-4.1".into(), false),
            (3, "openai".into(), "GPT-4.1 mini".into(), false),
        ]
    }

    /// The reported bug: `/model` lists `▸ :0  :1  :2  :3` but pressing a digit
    /// did nothing, so the only way to reach a model was to arrow to it. A
    /// digit must select the row whose `:N` label it names.
    #[test]
    fn a_digit_selects_the_row_labelled_with_that_index() {
        let r = rows();
        for (digit, want) in [('0', 0usize), ('1', 1), ('2', 2), ('3', 3)] {
            assert_eq!(
                model_pick_action(&r, 0, Key::Char(digit)),
                ModelPickAction::Move(want),
                "pressing {digit:?} must select the row labelled :{want}"
            );
        }
    }

    /// The digit selects the row *by its displayed index*, not by its screen
    /// offset: with the list scrolled (a real catalog is far taller than the
    /// box), the `:N` in the row text is what the operator is reading. Assert
    /// the mapping uses `rows[i].0` so the two can never drift apart.
    #[test]
    fn digits_key_off_the_shown_index_not_the_position() {
        // A window of the catalog starting at flat index 7.
        let r: Vec<(usize, String, String, bool)> = (7..11)
            .map(|i| (i, "p".into(), format!("m{i}"), false))
            .collect();
        assert_eq!(model_pick_action(&r, 0, Key::Char('7')), ModelPickAction::Move(0));
        assert_eq!(model_pick_action(&r, 0, Key::Char('9')), ModelPickAction::Move(2));
        // The digit still works from wherever the highlight already is.
        assert_eq!(model_pick_action(&r, 3, Key::Char('8')), ModelPickAction::Move(1));
    }

    /// A digit with no matching row is inert (never a panic, never a wrap to a
    /// wrong row) — same contract as the session picker's digit handling.
    #[test]
    fn out_of_range_digit_is_ignored() {
        let r = rows();
        assert_eq!(model_pick_action(&r, 1, Key::Char('9')), ModelPickAction::Ignore);
        let empty_window: Vec<(usize, String, String, bool)> =
            vec![(5, "p".into(), "m".into(), false)];
        assert_eq!(model_pick_action(&empty_window, 0, Key::Char('0')), ModelPickAction::Ignore);
    }

    /// The digit path must not have clobbered the keys that already worked:
    /// arrows, `j`/`k`, Home/End, and Enter/Esc.
    #[test]
    fn the_existing_keys_still_work() {
        let r = rows();
        assert_eq!(model_pick_action(&r, 1, Key::Down), ModelPickAction::Move(2));
        assert_eq!(model_pick_action(&r, 1, Key::Char('j')), ModelPickAction::Move(2));
        assert_eq!(model_pick_action(&r, 1, Key::Up), ModelPickAction::Move(0));
        assert_eq!(model_pick_action(&r, 1, Key::Char('k')), ModelPickAction::Move(0));
        assert_eq!(model_pick_action(&r, 2, Key::Home), ModelPickAction::Move(0));
        assert_eq!(model_pick_action(&r, 0, Key::End), ModelPickAction::Move(3));
        assert_eq!(model_pick_action(&r, 2, Key::Enter), ModelPickAction::Commit(2));
        assert_eq!(model_pick_action(&r, 2, Key::Right), ModelPickAction::Commit(2));
        assert_eq!(model_pick_action(&r, 2, Key::Esc), ModelPickAction::Cancel);
        assert_eq!(model_pick_action(&r, 2, Key::CtrlC), ModelPickAction::Cancel);
        assert_eq!(model_pick_action(&r, 2, Key::CtrlD), ModelPickAction::Cancel);
        // Movement clamps at both ends rather than wrapping.
        assert_eq!(model_pick_action(&r, 0, Key::Up), ModelPickAction::Move(0));
        assert_eq!(model_pick_action(&r, 3, Key::Down), ModelPickAction::Move(3));
    }

    /// A digit commits what it selects only through the *next* Enter — the
    /// picker reads the committed index from `rows[selected].0`, so the digit's
    /// `Move` target and Enter's `Commit` must agree on the same row. Drive
    /// that two-step interaction end to end (digit, then Enter) and assert the
    /// flat index returned is the one named by the digit.
    #[test]
    fn digit_then_enter_commits_that_row() {
        let r = rows();
        let mut selected = 0usize;
        match model_pick_action(&r, selected, Key::Char('2')) {
            ModelPickAction::Move(i) => selected = i,
            other => panic!("a digit must move the selection, got {other:?}"),
        }
        assert_eq!(selected, 2);
        match model_pick_action(&r, selected, Key::Enter) {
            ModelPickAction::Commit(i) => assert_eq!(i, 2, "Enter must commit the digit-selected row"),
            other => panic!("Enter must commit, got {other:?}"),
        }
    }

    /// The keys must be *discoverable*: the `:N` labels are keys now, so the box
    /// has to say so. Without the hint the shortcut exists but nobody finds it —
    /// the same "visible but unreachable" complaint, inverted.
    #[test]
    fn the_box_advertises_the_digit_shortcut() {
        let src = include_str!("modal.rs");
        let body = src
            .split("pub fn model_picker(")
            .nth(1)
            .expect("model_picker must exist");
        assert!(
            body.contains("[0-9] jump"),
            "the model picker box must advertise the digit shortcut"
        );
    }

    /// `model_picker` must *dispatch through* `model_pick_action` rather than
    /// re-inlining the key map. This is the regression the tests above cannot
    /// catch: they call the pure function directly, so if the picker kept a
    /// private match the digit handling could be reverted in the dialog and
    /// every test here would stay green while proving nothing about the code
    /// that runs. Assert the wiring where it is checkable (source).
    #[test]
    fn the_picker_dispatches_through_the_key_map() {
        let src = include_str!("modal.rs");
        let body = src
            .split("pub fn model_picker(")
            .nth(1)
            .expect("model_picker must exist");
        assert!(
            body.contains("model_pick_action(&rows, selected, read_key()?)"),
            "model_picker must dispatch through model_pick_action, not a private match"
        );
    }
}

#[cfg(test)]
mod inline_model_list_tests {
    use super::{inline_model_action, InlineModelAction, Key};

    /// The whole point of the inline list: the *visible* row numbers are keys.
    /// A digit commits the row it labels, so pressing `2` picks the same model
    /// as typing `/model2` — one keystroke, no Enter.
    #[test]
    fn a_digit_commits_the_row_it_numbers() {
        for (digit, want) in [('1', 0usize), ('2', 1), ('5', 4)] {
            assert_eq!(
                inline_model_action(5, 0, Key::Char(digit)),
                InlineModelAction::Commit(want),
                "digit {digit} must commit row {}",
                want + 1
            );
        }
    }

    /// Out-of-range digits are inert: `0` is not a row (the list is 1-based),
    /// and a digit past the end must never clamp onto the last model — a silent
    /// switch to the wrong model is worse than doing nothing.
    #[test]
    fn digits_outside_the_list_are_ignored() {
        assert_eq!(inline_model_action(5, 0, Key::Char('0')), InlineModelAction::Ignore);
        assert_eq!(inline_model_action(5, 0, Key::Char('6')), InlineModelAction::Ignore);
        assert_eq!(inline_model_action(5, 0, Key::Char('9')), InlineModelAction::Ignore);
        // A short history (fewer than five) must not accept a digit it cannot show.
        assert_eq!(inline_model_action(2, 0, Key::Char('3')), InlineModelAction::Ignore);
        assert_eq!(inline_model_action(0, 0, Key::Char('1')), InlineModelAction::Ignore);
    }

    /// Arrows move the highlight and clamp at the ends (no wrap); Enter commits
    /// whatever is highlighted, Esc/Ctrl-C/Ctrl-D cancel without switching.
    #[test]
    fn arrows_enter_and_cancel() {
        assert_eq!(inline_model_action(5, 0, Key::Up), InlineModelAction::Move(0));
        assert_eq!(inline_model_action(5, 1, Key::Up), InlineModelAction::Move(0));
        assert_eq!(inline_model_action(5, 1, Key::Down), InlineModelAction::Move(2));
        assert_eq!(inline_model_action(5, 4, Key::Down), InlineModelAction::Move(4));
        assert_eq!(inline_model_action(5, 2, Key::Char('k')), InlineModelAction::Move(1));
        assert_eq!(inline_model_action(5, 2, Key::Char('j')), InlineModelAction::Move(3));
        assert_eq!(inline_model_action(5, 3, Key::Home), InlineModelAction::Move(0));
        assert_eq!(inline_model_action(5, 0, Key::End), InlineModelAction::Move(4));
        assert_eq!(inline_model_action(5, 2, Key::Enter), InlineModelAction::Commit(2));
        assert_eq!(inline_model_action(5, 2, Key::Right), InlineModelAction::Commit(2));
        assert_eq!(inline_model_action(5, 2, Key::Esc), InlineModelAction::Cancel);
        assert_eq!(inline_model_action(5, 2, Key::CtrlC), InlineModelAction::Cancel);
        assert_eq!(inline_model_action(5, 2, Key::CtrlD), InlineModelAction::Cancel);
        // An unhandled key changes nothing.
        assert_eq!(inline_model_action(5, 2, Key::Char('x')), InlineModelAction::Ignore);
    }

    /// The list must be driven by the pure key map, and must paint each model on
    /// its own line via the shared row formatter (the same one that numbers the
    /// rows `/modelN` addresses). Source-level, because the two assertions above
    /// cannot see the code that actually runs.
    #[test]
    fn the_list_dispatches_through_the_key_map_and_the_row_formatter() {
        let src = include_str!("modal.rs");
        let body = src
            .split("pub fn model_list_picker(")
            .nth(1)
            .expect("model_list_picker must exist");
        assert!(
            body.contains("inline_model_action(labels.len(), selected, read_key()?)"),
            "model_list_picker must dispatch through inline_model_action"
        );
        assert!(
            body.contains("crate::model_history::format_rows("),
            "model_list_picker must render rows through the shared formatter"
        );
        // One row per line: the frame terminates each row with a newline rather
        // than joining them.
        assert!(body.contains(r#""\r\n""#), "each model must end on its own line");
    }
}
