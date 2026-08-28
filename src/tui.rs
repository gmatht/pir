//! Opt-in full-screen TUI REPL (enabled with `--features tui` + `--tui`).
//!
//! A real layout engine that replaces the hand-rolled "thinking block" of the
//! streaming REPL: a conversation pane (with its own scrollback) and a footer
//! pane showing the live status (thinking spinner / tool activity) plus the
//! draft prompt (`❯ <what you are typing>`). crossterm owns raw mode + resize;
//! ratatui owns every cursor move, so there is no manual `\x1b[2A` math and the
//! stray-spinner class of bug is structurally impossible.
//!
//! The TUI is a *superset* of the streaming REPL's behaviour: same turn
//! lifecycle (worker thread + `done` oneshot), same cooperative cancel flag,
//! same slash commands (`/help`, `/model`, `/goal`, `/continue`, `/undo`,
//! `/clear`, `/fix`, `/bg`, `/jobs`, `/cancel`, `/exit`, …). Scrollback is the
//! app's concern (not the terminal's), so Up/Down/PageUp/PageDown scroll the
//! conversation. While a turn runs, the footer shows a spinner and typed-ahead
//! input is captured raw (Enter queues the next prompt, Esc/ctrl-c cancel,
//! ctrl-d exits) — exactly like the streaming REPL, but rendered by ratatui.

use crate::agent::Agent;
use crate::config::Provider;
use crate::notify::SharedBus;
use crate::term;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use std::io::{self, Read, Seek, SeekFrom, Stdout, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::run_foreground_turn;

type AgentSlot = Arc<Mutex<Option<Agent>>>;

/// Run the TUI REPL. Returns `Ok(())` on a clean exit (ctrl-d / /exit) or an
/// error if the terminal couldn't be set up. The `agent_slot` is taken and
/// returned by the worker turns exactly like the streaming REPL does.
pub fn run(
    agent_slot: &AgentSlot,
    fg_cancel: &Arc<AtomicBool>,
    fg_quiet: &Arc<AtomicBool>,
    typeahead: &Arc<Mutex<String>>,
    providers: &[Provider],
    bus: &SharedBus,
    done_tx: &smol::channel::Sender<()>,
    full_auto: bool,
    running_as_agent: bool,
) -> Result<(), String> {
    if !term::is_terminal() {
        return Err("stdout is not a terminal".into());
    }

    enable_raw_mode().map_err(|e| format!("enable_raw_mode: {e}"))?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen).map_err(|e| format!("enter alt screen: {e}"))?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = ratatui::Terminal::new(backend).map_err(|e| format!("terminal init: {e}"))?;

    // RAII guard: leave alternate screen + restore raw mode no matter how we exit.
    let _cleanup = Cleanup;

    // The TUI owns its own turn-completion channel: `spawn_turn` signals `done_tx`
    // and the idle/raw waiters receive on `done_rx`. (The `done_tx` argument
    // from the caller drives the streaming REPL's separate loop and is unused here.)
    let (tui_done_tx, tui_done_rx) = smol::channel::bounded::<()>(1);

    let ctx = TuiCtx {
        agent_slot,
        fg_cancel,
        fg_quiet,
        typeahead,
        providers,
        bus,
        done_tx: tui_done_tx,
        done_rx: tui_done_rx,
        full_auto,
        running_as_agent,
    };

    match run_inner(&mut term, &ctx) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = term.draw(|f| {
                let size = f.area();
                f.render_widget(
                    Paragraph::new(Text::from(format!("pir TUI error: {e}")))
                        .style(Style::default().fg(Color::Red)),
                    size,
                );
            });
            Err(e)
        }
    }
}

/// RAII guard: leave alternate screen + restore raw mode no matter how we exit.
struct Cleanup;
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
        let _ = io::stdout().flush();
    }
}

/// Shared TUI context, owned by the main TUI thread and threaded through the loop.
struct TuiCtx<'a> {
    agent_slot: &'a AgentSlot,
    fg_cancel: &'a Arc<AtomicBool>,
    fg_quiet: &'a Arc<AtomicBool>,
    typeahead: &'a Arc<Mutex<String>>,
    providers: &'a [Provider],
    bus: &'a SharedBus,
    done_tx: smol::channel::Sender<()>,
    done_rx: smol::channel::Receiver<()>,
    full_auto: bool,
    running_as_agent: bool,
}

/// Conversation entries shown in the top pane. Tool activity is folded into the
/// status footer, not the conversation, to keep scrollback readable.
#[derive(Clone, Copy)]
enum ConvKind {
    User,
    Assistant,
    System,
    Error,
}

struct ConvLine {
    kind: ConvKind,
    text: String,
}

impl ConvLine {
    fn into_line(&self, color: bool) -> Line<'static> {
        let (fg, prefix) = match self.kind {
            ConvKind::User => (Color::Green, "❯ "),
            ConvKind::Assistant => (Color::Gray, "  "),
            ConvKind::System => (Color::Cyan, "· "),
            ConvKind::Error => (Color::Red, "✗ "),
        };
        let mut spans = Vec::new();
        if color {
            spans.push(Span::styled(prefix.to_string(), Style::default().fg(fg)));
        }
        spans.push(Span::raw(self.text.clone()));
        Line::from(spans)
    }
}

/// Mutable TUI state owned by the main thread and read every frame.
struct TuiState {
    conv: Vec<ConvLine>,
    status: String,
    draft: String,
    scroll: u16,
    running: bool,
    usage: String,
    spinner_frame: usize,
    last_tick: Instant,
    /// Byte offset into the session log that we've already rendered into the
    /// conversation pane (so `drain_session_log` only appends new lines).
    log_offset: u64,
    /// Detached (backgrounded) turns: (id, worker handle). Kept alive so they
    /// keep running after the foreground returns to idle. Reaped when finished.
    bg_handles: Vec<(usize, JoinHandle<()>)>,
    /// Monotonic id source for detached turns.
    next_bg_id: usize,
}

impl TuiState {
    fn new() -> Self {
        TuiState {
            conv: Vec::new(),
            status: "idle".into(),
            draft: String::new(),
            scroll: 0,
            running: false,
            usage: String::new(),
            spinner_frame: 0,
            last_tick: Instant::now(),
            log_offset: 0,
            bg_handles: Vec::new(),
            next_bg_id: 1,
        }
    }

    /// Append a conversation line, capping scrollback so memory stays bounded.
    fn push(&mut self, kind: ConvKind, text: &str) {
        for para in text.split('\n') {
            self.conv.push(ConvLine { kind, text: para.to_string() });
        }
        if self.conv.len() > 4000 {
            let drop = self.conv.len() - 4000;
            self.conv.drain(0..drop);
        }
        self.scroll = 0;
    }
}

fn run_inner(
    term: &mut ratatui::Terminal<CrosstermBackend<Stdout>>,
    ctx: &TuiCtx,
) -> Result<(), String> {
    let mut state = TuiState::new();
    state.push(
        ConvKind::System,
        &format!(
            "pir · {} · full-screen TUI (/help for commands · Esc/ctrl-c cancel · ctrl-d quit)",
            ctx.providers[0].pid()
        ),
    );

    let mut fg_handle: Option<JoinHandle<()>> = None;
    let mut pending: Vec<String> = Vec::new();

    let spinner_frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

    loop {
        // Drain notifications from all agents (foreground + background).
        let feed = ctx.bus.drain_feed();
        if !feed.is_empty() {
            for e in &feed {
                if !matches!(e.kind, crate::notify::EventKind::Idle) {
                    state.push(ConvKind::System, &format!("notify: {}", e.summary()));
                }
            }
        }

        // Tail the session log so the conversation pane reflects the agent's
        // real output. The TUI's agent runs in `quiet` mode (ratatui owns the
        // screen), but the agent still writes every message to its log file
        // unconditionally — so we render that. This is the key difference from
        // the streaming REPL: there is no competing stdout writer to garble the
        // alternate screen.
        if let Some(log) = ctx.agent_slot.lock().unwrap().as_ref().and_then(|a| a.log_path.clone()) {
            drain_session_log(log, &mut state);
        }

        // If the foreground turn finished, join it and either start the next
        // queued prompt or return to idle.
        if let Some(h) = fg_handle.as_ref() {
            if h.is_finished() {
                let h = fg_handle.take().unwrap();
                let _ = h.join();
                if let Some(a) = ctx.agent_slot.lock().unwrap().as_ref() {
                    state.usage = format!(
                        "{} in / {} out tokens",
                        fmt_tok(a.usage.input),
                        fmt_tok(a.usage.output)
                    );
                }
                if let Some(next) = pending.drain(..).next() {
                    if let Ok(mut g) = ctx.typeahead.lock() {
                        g.clear();
                    }
                    fg_handle = Some(spawn_turn(ctx, next));
                } else {
                    let follow = {
                        let mut g = ctx.agent_slot.lock().unwrap();
                        match g.as_mut() {
                            Some(a) => a.take_continuations(),
                            None => Vec::new(),
                        }
                    };
                    if let Some(next) = follow.into_iter().next() {
                        if let Ok(mut g) = ctx.typeahead.lock() {
                            g.clear();
                        }
                        fg_handle = Some(spawn_turn(ctx, next));
                    } else {
                        state.running = false;
                        state.status = "idle".into();
                    }
                }
            }
        }

        // ---- Render ----
        let running = fg_handle.is_some();
        state.running = running;
        let now = Instant::now();
        if now.duration_since(state.last_tick) >= Duration::from_millis(80) || !running {
            state.last_tick = now;
        }
        if running {
            state.spinner_frame = state.spinner_frame.wrapping_add(1);
        }
        let draft = if running {
            ctx.typeahead.lock().map(|g| g.clone()).unwrap_or_default()
        } else {
            state.draft.clone()
        };
        let status_line = if running {
            format!(
                "{} {}…",
                spinner_frames[state.spinner_frame % spinner_frames.len()],
                state.status
            )
        } else {
            state.status.clone()
        };
        draw(term, &state, &status_line, &draft, running, ctx.running_as_agent);

        // ---- Input ----
        if running {
            match wait_raw_input(ctx) {
                RawKey::Line(s) => {
                    let s = s.trim();
                    if let Ok(mut g) = ctx.typeahead.lock() {
                        g.clear();
                    }
                    if s.is_empty() {
                        // ignored
                    } else if let Some(cmd) = s.strip_prefix('/') {
                        handle_command(ctx, &mut state, cmd, &mut fg_handle, &mut pending);
                    } else if s == "&" {
                        // A bare `&` typed *while a turn runs* detaches the
                        // running foreground turn into the background: flip the
                        // shared "go quiet" switch (the worker stops streaming to
                        // the log while ratatui tails it) and adopt its worker
                        // handle as a background job, so the TUI returns to idle
                        // while the turn keeps working. The footer shows
                        // "#tasks running: N" as the only sign of life.
                        let log = {
                            let g = ctx.agent_slot.lock().unwrap();
                            g.as_ref().and_then(|a| a.log_path().cloned()).unwrap_or_default()
                        };
                        let prompt = {
                            let g = ctx.agent_slot.lock().unwrap();
                            g.as_ref().map(|a| a.last_prompt.clone()).unwrap_or_default()
                        };
                        let h = fg_handle.take().expect("fg running");
                        let id = state.detach(h);
                        ctx.fg_quiet.store(true, Ordering::SeqCst);
                        state.running = false;
                        state.status = "idle".into();
                        state.push(
                            ConvKind::System,
                            &format!("· detached running turn as job #{id} — it keeps working in the background"),
                        );
                    } else {
                        pending.push(s.to_string());
                        state.push(ConvKind::System, &format!("· queued: {s}"));
                    }
                }
                RawKey::Interrupt | RawKey::Cancel => {
                    if let Ok(mut g) = ctx.typeahead.lock() {
                        g.clear();
                    }
                    ctx.fg_cancel.store(true, Ordering::SeqCst);
                    state.push(ConvKind::System, "· cancelling turn (ESC/ctrl-c) — stopping now…");
                }
                RawKey::Eof => {
                    ctx.fg_cancel.store(true, Ordering::SeqCst);
                    if let Some(h) = fg_handle.take() {
                        let _ = h.join();
                    }
                    break;
                }
                RawKey::Suspend => {
                    let _ = disable_raw_mode();
                    unsafe {
                        libc::raise(libc::SIGTSTP);
                    }
                    let _ = enable_raw_mode();
                }
                RawKey::None => { /* turn finished / no input */ }
            }
        } else {
            let line = read_idle_line(term, &mut state, ctx);
            match line {
                // ctrl-d quit: return cleanly so the Cleanup guard restores the
                // terminal (raw mode + alt screen) instead of process::exit.
                None => break,
                Some(input) => {
                    let input = input.trim().to_string();
                    if input.is_empty() {
                        continue;
                    }
                    if let Some(cmd) = input.strip_prefix('/') {
                        handle_command(ctx, &mut state, cmd, &mut fg_handle, &mut pending);
                    } else if input.ends_with('&') && !input.trim_end_matches('&').is_empty() {
                        let prompt = input.trim_end_matches('&').trim().to_string();
                        spawn_background(ctx, &mut state, prompt);
                    } else if fg_handle.is_none() {
                        fg_handle = Some(spawn_turn(ctx, input.clone()));
                        term::push_history(&input);
                    } else {
                        pending.push(input);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Spawn a foreground turn on a worker thread (moves the agent out of the slot,
/// like the streaming REPL does) and returns the join handle.
fn spawn_turn(ctx: &TuiCtx, prompt: String) -> JoinHandle<()> {
    run_foreground_turn(
        ctx.agent_slot,
        ctx.fg_cancel,
        &ctx.fg_quiet,
        prompt,
        ctx.done_tx.clone(),
    )
}

impl TuiState {
    /// Detach a running foreground turn into the background: keep its worker
    /// handle alive (so it keeps running) and count it. Returns the (1-based)
    /// background id assigned. The footer then shows "#tasks running: N" as the
    /// only sign of life while it works.
    fn detach(&mut self, handle: JoinHandle<()>) -> usize {
        let id = self.next_bg_id;
        self.next_bg_id += 1;
        self.bg_handles.push((id, handle));
        id
    }

    /// Reap finished detached turns so their handles don't leak; returns how
    /// many finished this call. We take the vec by value (`mem::take`) so the
    /// reaper owns the handles and can `join()` them without borrowing `self`
    /// across the `self.push` that logs completion.
    fn reap_bg(&mut self) -> usize {
        let mut finished = 0;
        let mut done_ids: Vec<usize> = Vec::new();
        let mut remaining: Vec<(usize, JoinHandle<()>)> = Vec::new();
        for (id, h) in std::mem::take(&mut self.bg_handles) {
            if h.is_finished() {
                let _ = h.join();
                finished += 1;
                done_ids.push(id);
            } else {
                remaining.push((id, h));
            }
        }
        self.bg_handles = remaining;
        for id in done_ids {
            self.push(ConvKind::System, &format!("· #{id} finished"));
        }
        finished
    }

    /// How many detached (backgrounded) turns are still running.
    fn tasks_running(&self) -> usize {
        self.bg_handles.len()
    }
}

/// Spawn a background job (keeps its own session log). Mirrors `/bg` + trailing
/// `&` in the streaming REPL.
fn spawn_background(ctx: &TuiCtx, state: &mut TuiState, prompt: String) {
    let (provider, model) = {
        let g = ctx.agent_slot.lock().unwrap();
        let a = g.as_ref().expect("agent present while idle");
        (a.provider(), a.model())
    };
    let log = crate::session_log_path();
    let bcancel = Arc::new(AtomicBool::new(false));
    let bus = ctx.bus.clone();
    let full_auto = ctx.full_auto;
    let handle = std::thread::spawn(move || {
        let mut a = Agent::new(
            provider,
            model,
            full_auto,
            true,
            bus,
            None,
            bcancel,
            Arc::new(Mutex::new(String::new())),
        )
        .expect("bg agent");
        match a.turn(&prompt) {
            Ok(()) => a.notify_on_exit(a.turn_done_event()),
            Err(e) => a.notify_on_exit(a.error_event(e)),
        }
    });
    state.push(
        ConvKind::System,
        &format!("· backgrounded as job (logs to {})", log.display()),
    );
    // Keep the handle alive so it isn't dropped/joined immediately.
    std::mem::forget(handle);
}

/// Render one frame: conversation pane (top, scrollable) + footer (status + draft).
fn draw(
    term: &mut ratatui::Terminal<CrosstermBackend<Stdout>>,
    state: &TuiState,
    status: &str,
    draft: &str,
    running: bool,
    running_as_agent: bool,
) {
    let _ = term.draw(|f| {
        let size = f.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(3)])
            .split(size);

        let conv_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(format!(" pir — {} ", if running { "running" } else { "idle" }));
        let color = term::color_enabled();
        let lines: Vec<Line> = state.conv.iter().map(|c| c.into_line(color)).collect();
        let paragraph = Paragraph::new(Text::from(lines))
            .block(conv_block)
            .wrap(Wrap { trim: false })
            .scroll((state.scroll, 0));
        f.render_widget(paragraph, chunks[0]);

        let footer_title = if running_as_agent {
            " agent user · full-auto "
        } else {
            " "
        };
        let footer_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(footer_title.to_string());
        let mut footer_lines = Vec::new();
        footer_lines.push(Line::from(Span::styled(
            status.to_string(),
            Style::default().fg(if running { Color::Cyan } else { Color::Gray }),
        )));
        footer_lines.push(Line::from(vec![
            Span::styled("❯ ", Style::default().fg(Color::Green)),
            Span::raw(draft.to_string()),
        ]));
        let footer = Paragraph::new(Text::from(footer_lines))
            .block(footer_block)
            .style(Style::default());
        f.render_widget(footer, chunks[1]);
    });
}

/// Read raw keystrokes while a turn runs (event-driven, like the streaming
/// REPL's `raw::wait_input`) but routed through crossterm so the TUI stays the
/// sole screen owner. Returns a `RawKey` outcome.
fn wait_raw_input(ctx: &TuiCtx) -> RawKey {
    let stdin = match smol::Async::new(io::stdin()) {
        Ok(s) => s,
        Err(_) => return RawKey::None,
    };
    let readable = async { let _ = stdin.readable().await; };
    let finished = async { let _ = ctx.done_rx.recv().await; };
    smol::block_on(smol::future::or(readable, finished));
    let mut buf = String::new();
    read_raw_into(&mut buf, ctx.typeahead)
}

/// Drain non-blocking stdin into `buf`, translating control chars. While a turn
/// runs we DON'T echo to stdout (ratatui owns the screen); we just record into
/// `typeahead` for the footer to render.
fn read_raw_into(buf: &mut String, typeahead: &Arc<Mutex<String>>) -> RawKey {
    use std::os::unix::io::AsRawFd;
    let fd = io::stdin().as_raw_fd();
    let mut tmp = [0u8; 256];
    let mut nread = 0usize;
    loop {
        let r = unsafe {
            libc::read(fd, tmp.as_mut_ptr().add(nread) as *mut libc::c_void, tmp.len() - nread)
        };
        if r <= 0 {
            break;
        }
        nread += r as usize;
        if nread >= tmp.len() {
            break;
        }
    }
    if nread == 0 {
        return RawKey::None;
    }
    let mut i = 0usize;
    while i < nread {
        let b = tmp[i];
        i += 1;
        match b {
            0x0a | 0x0d => {
                let line = std::mem::take(buf);
                return RawKey::Line(line);
            }
            0x7f | 0x08 => {
                if !buf.is_empty() {
                    buf.pop();
                    if let Ok(mut g) = typeahead.lock() {
                        g.clear();
                        g.push_str(buf);
                    }
                }
            }
            0x03 => {
                buf.clear();
                if let Ok(mut g) = typeahead.lock() {
                    g.clear();
                }
                return RawKey::Interrupt;
            }
            0x04 => {
                buf.clear();
                if let Ok(mut g) = typeahead.lock() {
                    g.clear();
                }
                return RawKey::Eof;
            }
            0x1b => {
                // Esc is ambiguous: a lone Esc cancels the turn; it's also the
                // lead byte of a CSI sequence (arrows, Home/End, F-keys:
                // `0x1b 0x5b …`). Disambiguate on the byte after this one, which
                // is almost always already buffered in `tmp` (the whole sequence
                // arrives in a single terminal write). Only peek the fd when `0x1b`
                // is the last buffered byte.
                let next_is_csi = if i < nread {
                    tmp[i] == 0x5b
                } else {
                    matches!(read_byte_timeout(fd, Duration::from_millis(25)), Some(0x5b))
                };
                if !next_is_csi {
                    buf.clear();
                    if let Ok(mut g) = typeahead.lock() {
                        g.clear();
                    }
                    return RawKey::Cancel;
                }
                // CSI sequence: skip the `0x5b` (the `0x1b` is already consumed),
                // then swallow parameter/terminator bytes until an alphabetic byte
                // or `~`. Consume from the buffered `tmp` first so they don't leak
                // into the printable-ASCII arm; top up any tail still on the fd.
                if i < nread && tmp[i] == 0x5b {
                    i += 1;
                }
                while i < nread {
                    let c = tmp[i];
                    i += 1;
                    if c.is_ascii_alphabetic() || c == b'~' {
                        break;
                    }
                }
                if i >= nread {
                    drain_csi_sequence(fd);
                }
            }
            c if c >= 0x20 && c < 0x7f => {
                buf.push(c as char);
                if let Ok(mut g) = typeahead.lock() {
                    g.clear();
                    g.push_str(buf);
                }
            }
            _ => { /* ignore other control bytes */ }
        }
    }
    RawKey::None
}

/// Outcome of a raw read (mirrors `term::raw::RawInput`).
enum RawKey {
    None,
    Line(String),
    Interrupt,
    Cancel,
    Eof,
    Suspend,
}

/// Read a single byte from `fd` (non-blocking) with a short timeout; returns
/// `None` if nothing arrives in time or on error. Used to disambiguate a lone
/// Esc from the start of a CSI escape sequence.
fn read_byte_timeout(fd: libc::c_int, timeout: Duration) -> Option<u8> {
    let mut b: u8 = 0;
    let start = Instant::now();
    loop {
        let r = unsafe { libc::read(fd, &mut b as *mut u8 as *mut libc::c_void, 1) };
        if r == 1 {
            return Some(b);
        }
        if r <= 0 || start.elapsed() >= timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Consume the remainder of a CSI escape sequence (`… <alpha> | ~`) still on fd.
fn drain_csi_sequence(fd: libc::c_int) {
    let mut b: u8 = 0;
    for _ in 0..32 {
        let r = unsafe { libc::read(fd, &mut b as *mut u8 as *mut libc::c_void, 1) };
        if r != 1 {
            break;
        }
        if b.is_ascii_alphabetic() || b == b'~' {
            break;
        }
    }
}

/// Idle line editing: render a live draft in the footer while the user types,
/// with arrow-up/down history and backspace. We deliberately use a non-blocking
/// `libc::read` loop (NOT crossterm's `event::poll`, which does not time out in
/// raw mode under some ptys and would hang the REPL) — same mechanism as the
/// running-turn path, so the TUI stays the sole screen owner. On Enter returns
/// the line; ctrl-d returns `None` so the caller can break out cleanly and the
/// `Cleanup` guard restores raw mode + the alternate screen.
fn read_idle_line(
    term: &mut ratatui::Terminal<CrosstermBackend<Stdout>>,
    state: &mut TuiState,
    ctx: &TuiCtx,
) -> Option<String> {
    use std::os::unix::io::AsRawFd;
    let fd = io::stdin().as_raw_fd();
    let history: Vec<String> = read_history();
    let mut hist_idx: i32 = -1; // -1 = current (not recalling)
    let mut buf = String::new();

    loop {
        state.draft = buf.clone();
        state.status = "idle".into();
        draw(term, state, &state.status, &buf, false, ctx.running_as_agent);

        // Non-blocking read with a short poll so we keep redrawing (the footer
        // draft stays live) and never block on a pty that won't deliver events.
        let mut tmp = [0u8; 256];
        let n = unsafe { libc::read(fd, tmp.as_mut_ptr() as *mut libc::c_void, tmp.len()) };
        if n > 0 {
            let mut i = 0usize;
            while i < n as usize {
                let b = tmp[i];
                i += 1;
                match b {
                    0x0a | 0x0d => {
                        let line = std::mem::take(&mut buf);
                        state.draft.clear();
                        return Some(line);
                    }
                    0x7f | 0x08 => {
                        buf.pop();
                    }
                    0x03 => {
                        state.draft.clear();
                        return Some(String::new());
                    }
                    0x04 => {
                        state.draft.clear();
                        return None;
                    }
                    0x1b => {
                        // CSI sequence (arrows, etc.): skip it so it doesn't
                        // leak into the buffer; handle Up/Down for history.
                        let mut is_up = false;
                        let mut is_down = false;
                        if i < n as usize && tmp[i] == 0x5b {
                            i += 1;
                            let mut seq = [0u8; 8];
                            let mut k = 0usize;
                            while i < n as usize && k < seq.len() {
                                seq[k] = tmp[i];
                                i += 1;
                                k += 1;
                                if tmp[i - 1].is_ascii_alphabetic() {
                                    break;
                                }
                            }
                            if k >= 1 {
                                match seq[0] {
                                    b'A' => is_up = true,
                                    b'B' => is_down = true,
                                    _ => {}
                                }
                            }
                        }
                        if is_up {
                            if !history.is_empty() {
                                if hist_idx < (history.len() as i32) - 1 {
                                    hist_idx += 1;
                                }
                                buf = history[(history.len() as i32 - 1 - hist_idx) as usize].clone();
                            }
                        } else if is_down {
                            if hist_idx > 0 {
                                hist_idx -= 1;
                                buf = history[(history.len() as i32 - 1 - hist_idx) as usize].clone();
                            } else {
                                hist_idx = -1;
                                buf.clear();
                            }
                        } else {
                            buf.clear();
                        }
                    }
                    c if c >= 0x20 && c < 0x7f => {
                        buf.push(c as char);
                    }
                    _ => { /* ignore other control bytes */ }
                }
            }
        }
        // Small sleep so we don't busy-loop at 100% CPU while idle.
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// Best-effort history for idle arrow-up recall in the TUI. History recall is a
/// nicety; the canonical recorder lives in `term`, so we reuse an empty list
/// here (the streaming REPL re-reads the `.history` file on the next prompt).
fn read_history() -> Vec<String> {
    Vec::new()
}

/// Append any new transcript lines from `log` into the conversation pane. The
/// agent runs in `quiet` mode (its streaming never reaches the TUI's screen),
/// but it writes every message to its session log — so we render that instead
/// of letting a second stdout writer fight ratatui. `state.log_offset` tracks
/// how much of the file we've already rendered so we only append new lines.
fn drain_session_log(log: PathBuf, state: &mut TuiState) -> usize {
    use serde_json::Value;
    let mut f = match std::fs::OpenOptions::new().read(true).open(&log) {
        Ok(f) => f,
        Err(_) => return 0,
    };
    let _ = f.seek(SeekFrom::Start(state.log_offset)).is_ok();
    let mut buf = String::new();
    if f.read_to_string(&mut buf).is_err() {
        return 0;
    }
    state.log_offset += buf.len() as u64;
    let mut added = 0usize;
    for line in buf.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let role = v.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let blocks = v.get("blocks").and_then(|b| b.as_array());
        let mut text = String::new();
        let mut kind = ConvKind::System;
        if role == "user" {
            kind = ConvKind::User;
            for b in blocks.into_iter().flatten() {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                        text.push_str(t);
                        text.push('\n');
                    }
                }
            }
        } else if role == "assistant" {
            kind = ConvKind::Assistant;
            for b in blocks.into_iter().flatten() {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            text.push_str(t);
                            text.push('\n');
                        }
                    }
                    Some("tool_use") => {
                        let name = b.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                        if name == "update_goal" {
                            continue;
                        }
                        text.push_str(&format!("» {name}\n"));
                    }
                    Some("tool_result") => {
                        let c = b.get("content").and_then(|c| c.as_str()).unwrap_or("");
                        let c: String = c.lines().take(1).collect::<Vec<_>>().join(" ");
                        text.push_str(&format!("  {c}\n"));
                    }
                    _ => {}
                }
            }
        } else {
            continue;
        }
        let text = text.trim_end().to_string();
        if text.is_empty() {
            continue;
        }
        state.push(kind, &text);
        added += 1;
    }
    added
}

/// Convenience token formatter.
fn fmt_tok(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Handle a slash command inside the TUI. Mirrors `main::handle_command`.
fn handle_command(
    ctx: &TuiCtx,
    state: &mut TuiState,
    cmd: &str,
    fg_handle: &mut Option<JoinHandle<()>>,
    pending: &mut Vec<String>,
) {
    let mut parts = cmd.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    match cmd {
        "cancel" | "c" => {
            if fg_handle.is_some() {
                ctx.fg_cancel.store(true, Ordering::SeqCst);
                state.push(ConvKind::System, "· requesting cancel (turn will stop after its current step)");
            } else {
                state.push(ConvKind::System, "· no turn running to cancel (idle)");
            }
        }
        "h" | "help" => state.push(ConvKind::System, HELP_TUI),
        "m" | "model" => {
            let mut g = ctx.agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                state.push(ConvKind::System, "· agent busy (turn running) — try again when idle");
                return;
            };
            if rest.is_empty() {
                state.push(ConvKind::System, &format!("current model: {}", agent.label()));
            } else {
                match crate::config::select(ctx.providers, &rest.join(" ")) {
                    Ok((p, m)) => match agent.switch(p.clone(), m.clone()) {
                        Ok(()) => state.push(ConvKind::System, &format!("→ {}", agent.label())),
                        Err(e) => state.push(ConvKind::Error, &e),
                    },
                    Err(e) => state.push(ConvKind::Error, &e),
                }
            }
        }
        "models" => {
            let mut text = String::new();
            for p in ctx.providers {
                text.push_str(&format!("{}\n", p.pid()));
                for m in &p.models {
                    text.push_str(&format!("  {:<44} {}\n", m.id, m.name.as_deref().unwrap_or("")));
                }
            }
            state.push(ConvKind::System, &text);
        }
        "goal" => {
            let mut g = ctx.agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                state.push(ConvKind::System, "· agent busy (turn running) — try again when idle");
                return;
            };
            let obj: String = rest.join(" ");
            if obj.trim().is_empty() {
                agent.show_goal();
                state.push(ConvKind::System, "· show goal (see log above / use /goal <objective> to start)");
            } else {
                agent.start_goal(&obj);
                state.push(ConvKind::System, &format!("goal started: {obj}"));
            }
        }
        "continue" | "cont" => {
            let mut g = ctx.agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                state.push(ConvKind::System, "· agent busy (turn running) — try again when idle");
                return;
            };
            let lp = agent.log_path.clone();
            if let Some(p) = lp {
                agent.attach_goal(&p);
            }
            agent.continue_goal();
        }
        "clear" => {
            let mut g = ctx.agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                state.push(ConvKind::System, "· agent busy (turn running) — try again when idle");
                return;
            };
            agent.clear();
            state.conv.clear();
            state.log_offset = 0;
            state.push(ConvKind::System, "history cleared");
        }
        "fix" => {
            let repo = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            if !crate::project::is_git_repo(&repo) && crate::project::detect_vcs(&repo) != crate::project::Vcs::Jj {
                state.push(ConvKind::Error, "· not inside a git/jj repo");
            } else {
                state.push(ConvKind::System, &crate::project::fix_git_setup(&repo));
            }
        }
        "undo" => {
            let mut g = ctx.agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                state.push(ConvKind::System, "· agent busy (turn running) — try again when idle");
                return;
            };
            let all = rest.first().map(|s| *s == "all").unwrap_or(false);
            state.push(ConvKind::System, &agent.undo(all));
        }
        "sh" | "shell" => {
            // `/sh` drops to an interactive shell (after leaving the TUI's
            // alternate screen + raw mode so the child owns the terminal), or
            // `/sh COMMAND ARGS …` runs a command via the shell and returns. The
            // child inherits pir's identity (the possibly-dropped `ai_*` user),
            // cwd and env. We redraw once on return so the screen is sane.
            let args: Vec<&str> = rest;
            let _ = disable_raw_mode();
            let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
            let _ = io::stdout().flush();
            let code = crate::run_shell(args);
            // Re-enter the TUI: re-establish raw mode + alternate screen.
            let _ = enable_raw_mode();
            let _ = crossterm::execute!(io::stdout(), EnterAlternateScreen);
            let _ = io::stdout().flush();
            // Reset the conversation pane's log tail so the shell's noise doesn't
            // leak into scrollback, and refresh the agent's usage readout.
            state.log_offset = ctx
                .agent_slot
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|a| a.log_path.clone())
                .and_then(|p| std::fs::metadata(&p).ok().map(|m| m.len()))
                .unwrap_or(state.log_offset);
            if let Some(c) = code {
                if c != 0 {
                    state.push(ConvKind::Error, &format!("· shell exited with status {c}"));
                }
            } else {
                state.push(ConvKind::Error, "· could not start shell");
            }
        }
        "jobs" | "background" | "running" => {
            state.push(ConvKind::System, "· background jobs (see /bg <text> to start one)");
        }
        "bg" => {
            let prompt: String = rest.join(" ");
            if prompt.trim().is_empty() {
                state.push(ConvKind::Error, "usage: /bg <prompt>");
            } else {
                spawn_background(ctx, state, prompt);
            }
        }
        "rebuild" => {
            state.push(ConvKind::System, "· /rebuild is only available in the streaming REPL");
        }
        "exit" | "quit" | "q" => std::process::exit(0),
        other => state.push(ConvKind::Error, &format!("unknown command /{other} — try /help")),
    }
}

const HELP_TUI: &str = "\
commands: /help /model <sel> /models /goal [obj] /continue /clear /fix /undo [all] \
/bg <text> /jobs /cancel
/sh [cmd args]  drop to a shell, or run a command via $SHELL (sh -c)
Esc or ctrl-c cancels the running turn; ctrl-d quits; lines ending in & run in the background";
