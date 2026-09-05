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
use std::sync::Mutex;
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
        let mut stdout = io::stdout();
        // Enter alternate screen, hide the cursor, enable raw mode.
        if execute!(stdout, EnterAlternateScreen, Hide).is_err() {
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
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, Show);
        let _ = stdout.flush();
    }
}

/// Read a single key from the terminal (blocking). Returns `None` on EOF or a
/// read error. Translates crossterm events into the [`Key`] enum.
pub fn read_key() -> Option<Key> {
    loop {
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
}

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
    let w = crate::term::terminal_width();
    let h = crate::term::terminal_height();
    let body_w = w.saturating_sub(4).max(10);
    let body_h = lines.len().min(h.saturating_sub(4).max(1));
    let top = (h.saturating_sub(body_h + 2)) / 2;
    let left = (w.saturating_sub(body_w + 2)) / 2;
    let bottom = top + 1 + body_h;

    let mut out = String::new();
    // Title bar.
    let title = truncate(title, body_w);
    out.push_str(&format!("\x1b[{};{}H┌─ {} ─", top, left, title));
    // Fill the rest of the top border.
    let used = 4 + title.chars().count();
    for _ in used..body_w + 2 {
        out.push('─');
    }
    out.push_str("┐");
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
        out.push_str("│");
    }
    // Bottom border.
    out.push_str(&format!("\x1b[{};{}H└", bottom, left));
    for _ in 0..body_w + 2 {
        out.push('─');
    }
    out.push_str("┘");
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
    out.push_str("┐");
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
        out.push_str("│");
    }
    // Bottom border.
    out.push_str(&format!("\x1b[{};{}H└", bottom, left_col));
    for _ in 0..body_w + 2 {
        out.push('─');
    }
    out.push_str("┘");
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
    if let Some(p) = prev.as_ref() {
        if p.out == out {
            // Unchanged frame: nothing to do. Return the previous extents so the
            // caller's cursor math stays consistent.
            return p.bottom.saturating_sub(p.top);
        }
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
    let mut vis = 0usize;
    for c in s.chars() {
        if vis >= n.saturating_sub(1) {
            break;
        }
        out.push(c);
        vis += 1;
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
                format!("{marker} [{}] {label}", if i == selected { key } else { key })
            })
            .collect();
        draw_box("pir — main menu", &lines);
        match read_key()? {
            Key::Up | Key::Char('k') => selected = selected.saturating_sub(1),
            Key::Down | Key::Char('j') => selected = (selected + 1).min(items.len() - 1),
            Key::Enter | Key::Right => return Some(action_for(items[selected].0)),
            Key::Char(c) => {
                let c = c.to_ascii_lowercase();
                if let Some((i, _)) = items.iter().enumerate().find(|(_, (k, _))| k.chars().next() == Some(c)) {
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

/// Show the settings dialog on the alternate screen. Displays the current
/// settings (model, thinking, done-prompt color, markdown backend, incremental,
/// full-auto). Returns `None` if not a tty.
pub fn settings_dialog(
    model: &str,
    thinking: &str,
    show_thinking: bool,
    incremental: bool,
    full_auto: bool,
) -> Option<()> {
    let _modal = Modal::enter()?;
    let lines = settings_lines(model, thinking, show_thinking, incremental, full_auto);
    scroll_until_dismiss("pir — settings", &lines)
}

fn settings_lines(
    model: &str,
    thinking: &str,
    show_thinking: bool,
    incremental: bool,
    full_auto: bool,
) -> Vec<String> {
    use crate::term;
    let mut lines = Vec::new();
    lines.push(format!("  model: {}", term::cyan(model)));
    lines.push(format!("  thinking: {}   show: {}", term::cyan(thinking), if show_thinking { "on" } else { "off" }));
    lines.push(format!("  done-prompt color: {}", term::cyan(&term::done_prompt_color_token())));
    lines.push(format!("  markdown backend: {}", term::cyan(crate::config::markdown_renderer_backend())));
    lines.push(format!("  incremental markdown: {}", if incremental { "on" } else { "off" }));
    lines.push(format!("  full-auto: {}", if full_auto { term::green("on") } else { "off".to_string() }));
    lines.push(String::new());
    lines.push(term::dim("change with /model, /thinking, /default-model, /su-security").to_string());
    lines.push(term::dim("[esc] close").to_string());
    lines
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
        term::dim("deps: pulldown-cmark · streamdown-parser · rustyline · ureq · smol · crossterm").to_string(),
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
                format!("{marker} {:>3}  {:<w$}  {}{}  ", ix, term::cyan(&**pid), label, cur_mark)
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

/// The editable security-options dialog (opened from the menu's Security
/// item). Arrow through the rows, `←/→` (or `h`/`l`) cycles the value, `s` or
/// Enter saves to `security.toml` (takes effect for new sessions), `q`/Esc
/// discards. Mutates `policy` only when the user saves.
pub fn security_editor(policy: &mut crate::security::SecurityPolicy) -> Option<()> {
    let _modal = Modal::enter()?;

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
            term::dim("[↑/↓] move  [←/→] change  [s] save  [q] discard").into(),
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
        match read_key()? {
            Key::Up | Key::Char('k') => selected = selected.saturating_sub(1),
            Key::Down | Key::Char('j') => selected = (selected + 1).min(7),
            Key::Left | Key::Char('h') => apply_spin(selected, -1, &spin, &mut level, &mut apt, &mut network, &mut ask, &mut read, &mut quarantine, &mut quarantine_project, &mut user_security),
            Key::Right | Key::Char('l') => apply_spin(selected, 1, &spin, &mut level, &mut apt, &mut network, &mut ask, &mut read, &mut quarantine, &mut quarantine_project, &mut user_security),
            Key::Char('s') | Key::Enter => {
                policy.level = level;
                policy.apt = apt;
                policy.network = network;
                policy.ask = ask;
                policy.read = read;
                policy.quarantine = quarantine;
                policy.quarantine_project = quarantine_project;
                policy.user_security = user_security;
                let _ = crate::security::save_policy(policy);
                return Some(());
            }
            Key::Esc | Key::CtrlC | Key::CtrlD | Key::Char('q') => return Some(()),
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

/// Show a thinking-level picker on the alternate screen and return the chosen
/// level. Returns `None` if not a tty or the user cancels.
pub fn thinking_picker(current: &str, kind: Option<ApiKind>, ctx: u64) -> Option<String> {
    let _modal = Modal::enter()?;
    // Only offer levels that actually take effect for this provider + context
    // window. Showing `minimal` on an OpenAI model (no reasoning_effort) or an
    // under-budget Anthropic level would let the user pick something that is
    // silently ignored. `off` is always offered as an explicit opt-out.
    let all: Vec<crate::config::ThinkingLevel> = [
        "off", "minimal", "low", "medium", "high", "xhigh", "max",
    ]
    .iter()
    .filter_map(|l| crate::config::ThinkingLevel::parse(l))
    .filter(|l| l.effective(kind, ctx))
    .collect();
    let levels: Vec<&'static str> = all.iter().map(|l| l.as_str()).collect();
    let mut selected = levels.iter().position(|l| *l == current).unwrap_or(0);
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
            Key::Enter | Key::Right => return Some(levels[selected].to_string()),
            Key::Esc | Key::CtrlC | Key::CtrlD => return None,
            _ => {}
        }
    }
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
        draw_box("pir — secret entry", &lines);
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
        draw_box("pir — tool approval", &lines);
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

