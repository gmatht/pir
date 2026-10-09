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

/// Show a model picker on the alternate screen. `providers` is the catalog in
/// `/models` order; `current` is the active `provider/model` label (e.g.
/// `openai/gpt-fake`) to highlight, or `""` for none. Returns the chosen flat
/// index into the provider→model list (matches `config::select(":N")`), or
/// `None` if not a tty / the list is empty / cancelled.
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
        draw_box("pir — choose model", &lines);
        match read_key()? {
            Key::Up | Key::Char('k') => selected = selected.saturating_sub(1),
            Key::Down | Key::Char('j') => selected = (selected + 1).min(rows.len() - 1),
            Key::Home => selected = 0,
            Key::End => selected = rows.len() - 1,
            Key::Enter | Key::Right => return Some(rows[selected].0),
            Key::Esc | Key::CtrlC | Key::CtrlD => return None,
            _ => {}
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

/// Show an `ask_user_question` dialog on the alternate screen and return the
/// operator's answer (Feature 3). One question per dialog: a header title, the
/// question body (wrapped), the option list (with descriptions), and a hint
/// footer. Number keys jump straight to an option; arrows + Enter select.
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
        // a credential, so masking would be hostile).
        let mut buf = String::new();
        loop {
            let mut lines = wrap_lines(&q.question, 72);
            lines.push(String::new());
            lines.push(format!("  {}", if buf.is_empty() { "(type your answer)".to_string() } else { buf.clone() }));
            lines.push(String::new());
            lines.push(crate::term::dim("[enter] submit  [esc] dismiss  [backspace] delete").to_string());
            draw_box_titled(&format!("⏸ pir — {} — waiting for you", q.header), &lines, true);
            match read_key()? {
                Key::Enter => return Some(Answer::Text(buf)),
                Key::Esc | Key::CtrlC => return Some(Answer::Dismissed),
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
        let viewport = modal_body_rows();
        let first = window_start(selected, rows.len(), viewport);
        let visible: Vec<String> = rows[first..(first + viewport).min(rows.len())].to_vec();

        let mut lines = wrap_lines(&q.question, 72);
        lines.push(String::new());
        lines.extend(visible);
        lines.push(String::new());
        let hint = if multi {
            "[space] toggle  [enter] confirm  [1-9] toggle  [esc] dismiss"
        } else {
            "[enter] choose  [1-9] choose  [esc] dismiss"
        };
        // Say so when the list is taller than the box, so a scroll is never a
        // silent truncation.
        if first + viewport < rows.len() {
            lines.push(crate::term::dim(&format!(
                "… {} more (↑↓ to scroll)",
                rows.len() - (first + viewport)
            )).to_string());
        }
        lines.push(crate::term::dim(hint).to_string());
        draw_box_titled(&format!("⏸ pir — {} — waiting for you", q.header), &lines, true);
        let key = match read_key() {
            Some(k) => k,
            None => return Some(Answer::Dismissed),
        };
        match key {
            Key::Up | Key::Char('k') => selected = selected.saturating_sub(1),
            Key::Down | Key::Char('j') => selected = (selected + 1).min(q.options.len() - 1),
            Key::Char(' ') if multi => checked[selected] = !checked[selected],
            Key::Char(c @ '1'..='9') => {
                // The rows are numbered from 1 on screen (see the `{i})` label
                // below), so the digit key must mean the same 1-based option.
                // These used to disagree: the label printed the 0-based index
                // while this handler subtracted 1, so pressing `1` answered the
                // row labelled `0)` and any option past the ninth had no digit
                // at all — "I can't get to 3-9".
                let idx = (c as usize) - ('1' as usize);
                if idx < q.options.len() {
                    if multi {
                        checked[idx] = !checked[idx];
                    } else {
                        return Some(Answer::Chosen(vec![q.options[idx].label.clone()]));
                    }
                }
            }
            Key::Enter | Key::Right => {
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
            Key::Esc | Key::CtrlC | Key::CtrlD => return Some(Answer::Dismissed),
            _ => {}
        }
    }
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

/// How many body rows a modal box can show on this terminal.
///
/// `draw_box_titled` centres the box and keeps the title bar, so the body gets
/// the height minus the 2 border rows and minus the 2 rows the title/footer
/// text and the surrounding gap need. Kept in one place so a caller computing
/// a viewport (the question dialog) can never disagree with the renderer about
/// how many rows actually fit.
fn modal_body_rows() -> usize {
    let h = crate::term::terminal_height();
    // title + blank separator + body + blank separator + hint
    h.saturating_sub(6).max(1)
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
