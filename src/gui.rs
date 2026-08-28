//! Opt-in graphical (GTK) REPL (enabled with `--features gui` + `--gui`).
//!
//! Built on rustxwidgets' dlopen'd GTK backend: a window with a scrollable
//! conversation TextView on top and an Entry prompt + status line at the bottom.
//! The agent runs in `quiet` mode (it still writes every message to its session
//! log), and a periodic glib timeout drains that log into the conversation pane —
//! the same "render from the log, no competing stdout writer" trick the TUI
//! uses. Enter in the Entry starts a foreground turn on a worker thread (via
//! `run_foreground_turn`); slash commands are handled inline.

use crate::agent::Agent;
use crate::config::Provider;
use crate::notify::SharedBus;
use rustxwidgets::prelude::*;
use std::os::raw::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::run_foreground_turn;

type AgentSlot = Arc<Mutex<Option<Agent>>>;

/// Shared per-UI state, guarded by a Mutex so both the Entry-activate callback
/// (main thread) and the periodic drain callback (also main thread via glib)
/// can see it. Since glib callbacks run on the main loop, contention is trivial.
struct GuiState {
    /// Rendered conversation text (the full transcript so far).
    conv: String,
    /// Prompts queued by the user while a turn runs.
    pending: Vec<String>,
    /// Whether a foreground turn is currently running.
    running: bool,
    /// Byte offset into the session log already rendered.
    log_offset: u64,
    /// Status line (spinner / model / usage).
    status: String,
    /// Whether to show thinking blocks in the conversation.
    show_thinking: bool,
}

impl GuiState {
    fn new() -> Self {
        GuiState {
            conv: String::new(),
            pending: Vec::new(),
            running: false,
            log_offset: 0,
            status: "idle".into(),
            show_thinking: true,
        }
    }
}

/// Handle to the GTK widgets we mutate from the periodic drain callback.
#[derive(Clone)]
struct GuiWidgets {
    textview: TextView,
    status: Label,
}

/// Run the GTK GUI REPL. Returns `Ok(())` on a clean exit (`/exit` or window
/// close) or an error if the backend couldn't be initialised. Mirrors the TUI:
/// the agent is switched to `quiet` mode and turns run on worker threads; the
/// conversation is rendered by draining the agent's session log.
pub fn run(
    agent_slot: &AgentSlot,
    fg_cancel: &Arc<AtomicBool>,
    fg_quiet: &Arc<AtomicBool>,
    providers: &[Provider],
    bus: &SharedBus,
    _full_auto: bool,
) -> Result<(), String> {
    let app = App::init().map_err(|e| format!("rustxwidgets init: {e}"))?;

    // -- Build the widget tree --
    let window = app.create_window().map_err(|e| format!("create_window: {e}"))?;
    window.set_title("pir");
    window.set_default_size(820, 600);

    // Vertical box: [conversation TextView (expand)] [Entry] [status]
    let vbox = app
        .create_box(Orientation::Vertical, 4)
        .map_err(|e| format!("create_box: {e}"))?;

    let textview = app.create_textview().map_err(|e| format!("create_textview: {e}"))?;
    textview.set_hexpand(true);
    textview.set_vexpand(true);
    // GTK wrap mode: 2 = GTK_WRAP_WORD_CHAR
    textview.set_wrap_mode(2);
    vbox.append(&textview);

    let entry = app.create_entry().map_err(|e| format!("create_entry: {e}"))?;
    entry.set_hexpand(true);
    vbox.append(&entry);

    let status = app.create_label("pir · idle").map_err(|e| format!("create_label: {e}"))?;
    status.set_xalign(0.0); // left-align
    vbox.append(&status);

    window.set_child(&vbox);
    window.present();

    // -- Shared state --
    let state = Arc::new(Mutex::new(GuiState::new()));
    let widgets = GuiWidgets { textview: textview.clone(), status: status.clone() };

    // Initial system line.
    {
        let mut s = state.lock().unwrap();
        s.conv.push_str(&format!(
            "pir · {} · GTK GUI  (/help for commands · Enter to send · ctrl-q to quit)\n",
            providers[0].pid()
        ));
        sync_textview(&widgets, &s);
    }

    let (done_tx, _done_rx) = smol::channel::bounded::<()>(1);

    // -- Entry activate: start a foreground turn (or queue if one is running) --
    let entry_cb_state = state.clone();
    let entry_cb_slot = agent_slot.clone();
    let entry_cb_cancel = fg_cancel.clone();
    let entry_cb_quiet = fg_quiet.clone();
    let entry_cb_providers: Vec<Provider> = providers.to_vec();
    let entry_cb_bus = bus.clone();
    let entry_cb_widgets = widgets.clone();
    let entry_cb_done = done_tx.clone();
    let entry_in_cb = entry.clone();
    let _ = entry.connect_activate(move |_data: *mut c_void| {
        let text = entry_in_cb.get_text().unwrap_or_default();
        entry_in_cb.set_text("");
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }

        if let Some(cmd) = text.strip_prefix('/') {
            handle_command(
                cmd,
                &entry_cb_slot,
                &entry_cb_providers,
                &entry_cb_bus,
                &entry_cb_cancel,
                &entry_cb_state,
                &entry_cb_widgets,
            );
            return;
        }

        // Regular prompt: if a turn is running, queue it; else start it now.
        let mut s = entry_cb_state.lock().unwrap();
        if s.running {
            s.pending.push(text.clone());
            s.status = format!("queued: {}", truncate(&text, 50));
            sync_textview(&entry_cb_widgets, &s);
            return;
        }
        s.running = true;
        s.status = "running…".into();
        s.conv.push_str(&format!("❯ {text}\n"));
        sync_textview(&entry_cb_widgets, &s);
        drop(s);

        // Reset the quiet switch so a previously detached turn's quiet state
        // can't leak.
        entry_cb_quiet.store(false, Ordering::SeqCst);
        spawn_turn(
            &entry_cb_slot,
            &entry_cb_cancel,
            &entry_cb_quiet,
            text,
            entry_cb_done.clone(),
        );
    });

    // -- Periodic drain: refresh the conversation + status from the session log --
    // A recurring glib timeout (~150ms) that drains the agent's log into the
    // conversation pane, giving streaming output a live feel even though the
    // agent writes only to its log. The timer keeps firing for the lifetime of
    // the GTK main loop (until the window is destroyed).
    let drain_state = state.clone();
    let drain_widgets = widgets.clone();
    let drain_slot = agent_slot.clone();
    let drain_cancel = fg_cancel.clone();
    let drain_quiet = fg_quiet.clone();
    let drain_done = done_tx.clone();
    let drain_bus = bus.clone();
    unsafe {
        if let Some(loader) = rustxwidgets::backends::gtk::loader() {
            rustxwidgets::gtk_dynamic_loader::timeout_add_recurring(&loader, 150, Box::new(move || {
                drain_once(
                    &drain_state,
                    &drain_widgets,
                    &drain_slot,
                    &drain_cancel,
                    &drain_quiet,
                    &drain_done,
                    &drain_bus,
                );
            }));
        }
    }

    // Window close (destroy signal) -> quit the GTK main loop.
    unsafe {
        if let Some(loader) = rustxwidgets::backends::gtk::loader() {
            let _ = rustxwidgets::gtk_dynamic_loader::connect_signal(
                &loader.symbols,
                window.raw_handle(),
                "destroy",
                Box::new(move || {
                    let _ = rustxwidgets::backends_gtk_adapter::quit_main_loop();
                }),
                2,
            );
        }
    }

    app.run().map_err(|e| format!("gtk main loop: {e}"))
}

/// One periodic drain pass: pull notifications, read new session-log lines into
/// the conversation, join finished turns, and (re)start the next queued prompt.
fn drain_once(
    state: &Arc<Mutex<GuiState>>,
    widgets: &GuiWidgets,
    agent_slot: &AgentSlot,
    fg_cancel: &Arc<AtomicBool>,
    fg_quiet: &Arc<AtomicBool>,
    done: &smol::channel::Sender<()>,
    bus: &SharedBus,
) {
    let mut s = state.lock().unwrap();

    // Surface notifications from all agents.
    let feed = bus.drain_feed();
    for e in &feed {
        if !matches!(e.kind, crate::notify::EventKind::Idle) {
            s.conv.push_str(&format!("notify: {}\n", e.summary()));
        }
    }

    // Tail the session log.
    if let Some(log) = agent_slot.lock().unwrap().as_ref().and_then(|a| a.log_path.clone()) {
        drain_session_log(log, &mut s);
    }

    // Turn finished? Join it and start the next queued prompt or go idle.
    // We detect completion by checking whether the agent is back in the slot
    // AND `running` is still set (the worker returns the agent on completion).
    if s.running {
        let slot = agent_slot.lock().unwrap();
        let back = slot.as_ref().is_some();
        drop(slot);
        if back {
            // Worker returned the agent => turn finished.
            s.running = false;
            if let Some(a) = agent_slot.lock().unwrap().as_ref() {
                s.status = format!(
                    "{} in / {} out tokens",
                    fmt_tok(a.usage.input),
                    fmt_tok(a.usage.output)
                );
            } else {
                s.status = "idle".into();
            }
            let next = s.pending.drain(..).next();
            if let Some(next) = next {
                s.running = true;
                s.status = "running…".into();
                s.conv.push_str(&format!("❯ {next}\n"));
                sync_textview(widgets, &s);
                drop(s);
                fg_quiet.store(false, Ordering::SeqCst);
                spawn_turn(agent_slot, fg_cancel, fg_quiet, next, done.clone());
                return;
            }
        }
    }

    sync_textview(widgets, &s);
}

/// Spawn a prompt as a foreground turn on a worker thread, *moving* the agent
/// out of `agent_slot` for the duration (the same helper the streaming REPL and
/// TUI use). Wakes `done` when it finishes.
fn spawn_turn(
    agent_slot: &AgentSlot,
    cancel: &Arc<AtomicBool>,
    quiet: &Arc<AtomicBool>,
    prompt: String,
    done: smol::channel::Sender<()>,
) {
    run_foreground_turn(agent_slot, cancel, quiet, prompt, done);
}

/// Drain the agent's session log into `state.conv` (appending only new bytes
/// since the last pass).
fn drain_session_log(log: PathBuf, state: &mut GuiState) {
    use serde_json::Value;
    use std::io::{Read, Seek, SeekFrom};
    let mut f = match std::fs::OpenOptions::new().read(true).open(&log) {
        Ok(f) => f,
        Err(_) => return,
    };
    let _ = f.seek(SeekFrom::Start(state.log_offset)).is_ok();
    let mut buf = String::new();
    if f.read_to_string(&mut buf).is_err() {
        return;
    }
    state.log_offset += buf.len() as u64;
    for line in buf.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let role = v.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let blocks = v.get("blocks").and_then(|b| b.as_array());
        let mut text = String::new();
        if role == "user" {
            for b in blocks.into_iter().flatten() {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                        text.push_str(t);
                        text.push('\n');
                    }
                }
            }
        } else if role == "assistant" {
            for b in blocks.into_iter().flatten() {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            text.push_str(t);
                            text.push('\n');
                        }
                    }
                    Some("thinking") => {
                        if !state.show_thinking {
                            continue;
                        }
                        if let Some(t) = b.get("thinking").and_then(|t| t.as_str()) {
                            let t = t.trim();
                            if !t.is_empty() {
                                text.push_str(&format!("💭 {t}\n"));
                            }
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
        if !text.is_empty() {
            state.conv.push_str(&text);
            state.conv.push('\n');
        }
    }
    // Bound scrollback.
    if state.conv.len() > 200_000 {
        let keep = state.conv.len() - 200_000;
        state.conv = state.conv.split_off(keep);
    }
}

/// Push the current conversation + status into the TextView (and the status
/// Label). Cheap enough to call every drain tick.
fn sync_textview(widgets: &GuiWidgets, state: &GuiState) {
    widgets.textview.set_text(&state.conv);
    widgets.status.set_text(&state.status);
}

/// Handle a slash command inline (a GUI-appropriate subset). Mirrors the
/// streaming REPL's commands but reads/writes the GUI's own state/agent slot.
fn handle_command(
    cmd: &str,
    agent_slot: &AgentSlot,
    providers: &[Provider],
    _bus: &SharedBus,
    cancel: &Arc<AtomicBool>,
    state: &Arc<Mutex<GuiState>>,
    widgets: &GuiWidgets,
) {
    let mut parts = cmd.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    let mut s = state.lock().unwrap();
    match cmd {
        "h" | "help" => {
            s.conv.push_str(
                "commands: /help /model <sel> /models /goal [obj] /continue /clear /undo\n\
                 \x20  /cancel  /thinking  /sessions  /usage  /exit  /quit\n",
            );
        }
        "exit" | "quit" | "q" => {
            s.status = "bye".into();
            drop(s);
            let _ = rustxwidgets::backends_gtk_adapter::quit_main_loop();
            return;
        }
        "cancel" | "c" => {
            if s.running {
                cancel.store(true, Ordering::SeqCst);
                s.conv.push_str("· requesting cancel (turn stops now)\n");
            } else {
                s.conv.push_str("· no turn running to cancel\n");
            }
        }
        "clear" => {
            s.conv.clear();
            s.conv.push_str("pir · GTK GUI\n");
        }
        "thinking" => {
            s.show_thinking = !s.show_thinking;
            let on = s.show_thinking;
            s.conv.push_str(&format!("· thinking display {}\n", if on { "on" } else { "off" }));
        }
        "usage" => {
            let g = agent_slot.lock().unwrap();
            match g.as_ref() {
                Some(a) => s.conv.push_str(&format!(
                    "{} in / {} out tokens this session\n",
                    fmt_tok(a.usage.input),
                    fmt_tok(a.usage.output)
                )),
                None => s.conv.push_str("· agent busy (turn running)\n"),
            }
        }
        "models" => {
            for p in providers {
                s.conv.push_str(&format!("{}\n", p.pid()));
                for m in &p.models {
                    s.conv.push_str(&format!("  {}\n", m.id));
                }
            }
        }
        "model" | "m" => {
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                s.conv.push_str("· agent busy (turn running) — try again when idle\n");
                return;
            };
            if rest.is_empty() {
                s.conv.push_str(&format!("current model: {}\n", agent.label()));
            } else {
                match crate::config::select(providers, &rest.join(" ")) {
                    Ok((p, m)) => match agent.switch(p.clone(), m.clone()) {
                        Ok(()) => s.conv.push_str(&format!("→ {}\n", agent.label())),
                        Err(e) => s.conv.push_str(&format!("{e}\n")),
                    },
                    Err(e) => s.conv.push_str(&format!("{e}\n")),
                }
            }
        }
        "sessions" => {
            s.conv.push_str("use `pir -r` from a terminal to resume sessions\n");
        }
        "goal" => {
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                s.conv.push_str("· agent busy\n");
                return;
            };
            let obj: String = rest.join(" ");
            if obj.trim().is_empty() {
                match agent.goal_snapshot() {
                    Some(g) => s.conv.push_str(&g),
                    None => s.conv.push_str("no goal set — try /goal <objective>\n"),
                }
            } else {
                agent.start_goal(&obj);
                s.conv.push_str(&format!("goal started: {obj}\n"));
            }
        }
        "continue" | "cont" => {
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                s.conv.push_str("· agent busy\n");
                return;
            };
            let lp = agent.log_path.clone();
            if let Some(p) = lp {
                agent.attach_goal(&p);
            }
            let out = agent.continue_goal();
            s.conv.push_str(&out);
        }
        "undo" => {
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                s.conv.push_str("· agent busy\n");
                return;
            };
            let all = rest.first().map(|x| *x == "all").unwrap_or(false);
            s.conv.push_str(&agent.undo(all));
        }
        other => {
            // Try extension-registered slash commands.
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                s.conv.push_str("· agent busy\n");
                return;
            };
            match agent.run_registered_command(other, rest.join(" ").trim()) {
                Some(outcome) => s.conv.push_str(&format!("{}\n", outcome.content)),
                None => s.conv.push_str(&format!("unknown command /{other} — try /help\n")),
            }
        }
    }
    drop(s);
    sync_textview(widgets, &state.lock().unwrap());
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>())
    }
}

fn fmt_tok(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}
