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

use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use std::io::{self, Write};

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
        if enable_raw_mode().is_err() {
            let _ = execute!(stdout, LeaveAlternateScreen, Show);
            return None;
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
            Ok(Event::Key(KeyEvent { code, modifiers, .. })) => {
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
pub fn draw_box(title: &str, lines: &[String]) -> usize {
    let w = crate::term::terminal_width();
    let h = crate::term::terminal_height();
    let body_w = w.saturating_sub(4).max(10);
    let body_h = lines.len().min(h.saturating_sub(4).max(1));
    let top = (h.saturating_sub(body_h + 2)) / 2;
    let left = (w.saturating_sub(body_w + 2)) / 2;

    let mut out = String::new();
    // Clear the alternate screen.
    out.push_str("\x1b[2J");
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
    let bottom = top + 1 + body_h;
    out.push_str(&format!("\x1b[{};{}H└", bottom, left));
    for _ in 0..body_w + 2 {
        out.push('─');
    }
    out.push_str("┘");
    // Move cursor to a safe spot.
    out.push_str(&format!("\x1b[{};{}H", bottom + 1, left));
    let _ = io::stdout().write_all(out.as_bytes());
    let _ = io::stdout().flush();
    body_h + 2
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
        "h" => MenuAction::Help,
        "a" => MenuAction::About,
        "q" => MenuAction::Quit,
        _ => MenuAction::None,
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

