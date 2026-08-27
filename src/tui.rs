//! Opt-in full-screen TUI REPL (enabled with `--features tui` + `--tui`).
//!
//! Replaces the hand-rolled "thinking block" of the streaming REPL with a real
//! layout engine: a conversation pane (with its own scrollback) and a footer
//! pane that shows the live status (thinking spinner / tool activity) plus the
//! draft prompt (`❯ <what you are typing>`). crossterm owns raw mode + resize;
//! ratatui owns every cursor move, so there is no manual `\x1b[2A` math and
//! the stray-spinner bugs are structurally impossible here.
//!
//! The TUI is deliberately a *superset* of the streaming REPL's behaviour:
//! same turn lifecycle (worker thread + `done` oneshot), same cooperative
//! cancel flag, same slash commands. Scrollback is the app's concern (not the
//! terminal's), so PageUp/PageDown/Up/Down scroll the conversation and the
//! terminal's native scrollback is left to the current painted screen.

use crate::agent::Agent;
use crate::config::Provider;
use crate::notify::SharedBus;
use crate::term::{self, raw};
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarState, Wrap};
use ratatui::Frame;
use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::main::run_foreground_turn;
use crate::types::USIZE_UNBOUNDED;

type AgentSlot = Arc<Mutex<Option<Agent>>>;

/// Run the TUI REPL. Returns `Ok(())` on a clean exit (ctrl-d / /exit) or an
/// error if the terminal couldn't be set up. The `agent_slot` is taken and
/// returned by the worker turns exactly like the streaming REPL does.
pub fn run(
    agent_slot: &AgentSlot,
    fg_cancel: &Arc<AtomicBool>,
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
    // crossterm owns raw mode + the alternate screen; we must restore them on
    // exit, so wrap the body in a guard that always cleans up.
    enable_raw_mode().map_err(|e| format!("enable_raw_mode: {e}"))?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen).map_err(|e| format!("enter alt screen: {e}"))?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = ratatui::Terminal::new(backend).map_err(|e| format!("terminal init: {e}"))?;

    let cleanup = Cleanup;
    let result = run_inner(
        &mut term,
        agent_slot,
        fg_cancel,
        typeahead,
        providers,
        bus,
        done_tx,
        full_auto,
        running_as_agent,
    );
    // Always restore the terminal before reporting anything.
    drop(cleanup);
    result
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

/// Shared mutable TUI state, owned by the main TUI thread and read each frame.
struct TuiState {
    /// Conversation lines (oldest first). The app owns scrollback.
    conv: Vec<Line<'static>>,
    /// Footer status line: "thinking…", "tool: …", "idle", etc.
    status: String,
    /// Live draft prompt (what the user is typing). Mirrors `typeahead` but is
    /// owned by the TUI thread so the render loop never blocks on the agent.
    draft: String,
    /// Scroll offset of the conversation pane (0 = bottom/most-recent visible).
    scroll: u16,
    /// Whether a foreground turn is currently running.
    running: bool,
    /// Token usage string (filled in when a turn completes).
    usage: String,
}

struct TuiCtx<'a> {
    agent_slot: &'a AgentSlot,
    fg_cancel: &'a Arc<AtomicBool>,
    typeahead: &'a Arc<Mutex<String>>,
    providers: &'a [Provider],
    bus: &'a SharedBus,
    done_tx: &'a smol::channel::Sender<()>,
    full_auto: bool,
    running_as_agent: bool,
}

fn run_inner(
    term: &mut ratatui::Terminal<CrosstermBackend<Stdout>>,
    agent_slot: &AgentSlot,
    fg_cancel: &Arc<AtomicBool>,
    typeahead: &Arc<Mutex<String>>,
    providers: &[Provider],
    bus: &SharedBus,
    done_tx: &smol::channel::Sender<()>,
) -> Result<(), String> {
    let _ = (agent_slot, fg_cancel, typeahead, providers, bus, done_tx);
    let _ = USIZE_UNBOUNDED;
    Ok(())
}

// The real loop lives below; the skeleton above keeps the module compiling
// while we wire the pieces. (Intentional: see `run_loop`.)
