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
use crate::term;
use rustxwidgets::prelude::*;
use std::os::raw::c_void;
use std::path::PathBuf;
use std::rc::Rc;
use std::cell::{RefCell, Cell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::run_foreground_turn;

type AgentSlot = Arc<Mutex<Option<Agent>>>;

/// The concrete `Entry` type returned by `App::create_entry` — the backend
/// adapter `Entry` (nwg on Windows, gtk on Linux). `rustxwidgets::Entry` (the
/// prelude alias) is a thin wrapper, so we name the real type explicitly here.
#[cfg(windows)]
type GuiEntry = rustxwidgets::backends_nwg_adapter::Entry;
#[cfg(all(target_os = "linux", not(windows)))]
type GuiEntry = rustxwidgets::backends_gtk_adapter::Entry;

/// The concrete `Label` type returned by `App::create_label`.
///
/// The prelude re-exports `common::Label` — a thin wrapper struct — but the
/// `App` factory methods hand back the *backend adapter* types, so `GuiWidgets`
/// has to name the adapter type or the assignment does not typecheck.
#[cfg(windows)]
type GuiLabel = rustxwidgets::backends_nwg_adapter::Label;
#[cfg(all(target_os = "linux", not(windows)))]
type GuiLabel = rustxwidgets::backends_gtk_adapter::Label;

/// Are we on the GTK backend?
///
/// NOTE: this deliberately tests `target_os`, *not* `cfg(feature = "gtk")`.
/// A `feature` cfg inside this crate tests *pir's* features, and pir has no
/// feature named `gtk` — the `gtk` feature belongs to the rustxwidgets
/// dependency. So the old `cfg(feature = "gtk")` guards were silently always
/// false, which is why `GuiEntry` had no definition and the whole GUI failed
/// to compile. Gate on the platform, which is what actually picks the adapter.
#[cfg(all(target_os = "linux", not(windows)))]
const ON_GTK: bool = true;
#[cfg(not(all(target_os = "linux", not(windows))))]
const ON_GTK: bool = false;

/// GTK wrap mode: `GTK_WRAP_WORD_CHAR` — break mid-word only when a single
/// word is wider than the view, so code blocks and long paths still wrap.
const GTK_WRAP_WORD_CHAR: i32 = 2;

/// GDK keyval for the Tab key (GDK_KEY_Tab).
const GDK_KEY_TAB: u32 = 0xff09;
/// GDK keyvals for the arrow keys (GDK_KEY_Up / GDK_KEY_Down).
const GDK_KEY_UP: u32 = 0xff52;
const GDK_KEY_DOWN: u32 = 0xff54;
/// GDK keyvals for Ctrl (GDK_KEY_Control_L / _R) and the letter q.
const GDK_KEY_CTRL_L: u32 = 0xffe3;
const GDK_KEY_CTRL_R: u32 = 0xffe4;
const GDK_KEY_Q: u32 = 0x71;

/// Idle-mode Tab completion. Completes `/`-command names (and a couple of
/// argument lists). Returns `Some(completed)` with the new buffer when exactly
/// one (or a common) completion exists, else `None`.
///
/// This list is GUI-local on purpose: `crate::commands::SLASH_COMMANDS` is
/// generated from the streaming REPL's dispatch and includes aliases (`/c`,
/// `/h`, `/m`, …), which would make almost every short fragment ambiguous. The
/// local list is held honest by two tests at the bottom of this file — every
/// entry must be an implemented `handle_command` arm, and every entry must
/// exist in the generated global table. That catches the exact bug this
/// replaces: the old list offered `/fix`, `/bg`, `/jobs` and `/shell` while
/// implementing none of them, and omitted `/finished`.
const COMMANDS: &[&str] = &[
    "help", "model", "models", "goal", "continue", "clear", "undo", "finished",
    "exit", "usage", "thinking", "cancel", "sessions", "reload", "reexec",
    "fix", "shell", "bg", "jobs",
];

/// `/thinking` sub-arguments: the levels `config::ThinkingLevel::parse`
/// accepts, plus the `show`/`hide` visibility toggle.
const THINKING_ARGS: &[&str] =
    &["off", "minimal", "low", "medium", "high", "xhigh", "max", "show", "hide"];

fn complete_idle(buf: &str) -> Option<String> {
    // `/thinking <arg>` sub-argument completion.
    if buf.starts_with("/thinking ") {
        let arg = buf.trim_start_matches("/thinking ").trim_start();
        return complete_one_of(THINKING_ARGS, arg, "/thinking ");
    }
    if buf.starts_with('/') {
        let frag = buf.trim_start_matches('/');
        if frag.contains(' ') {
            return None; // past the command word; nothing to complete
        }
        return complete_one_of(COMMANDS, frag, "/");
    }
    None
}

/// Complete `frag` against `opts`, returning `Some(format!("{prefix}{match}"))`
/// when exactly one (or a common) prefix exists, else `None`.
fn complete_one_of(opts: &[&str], frag: &str, prefix: &str) -> Option<String> {
    let matches: Vec<&str> = opts.iter().copied().filter(|o| o.starts_with(frag)).collect();
    if matches.len() == 1 {
        return Some(format!("{prefix}{}", matches[0]));
    }
    if matches.len() > 1 {
        let lcp = longest_common_prefix(&matches);
        if !lcp.is_empty() && lcp != frag {
            return Some(format!("{prefix}{lcp}"));
        }
    }
    None
}

/// Longest common prefix of a set of strings (empty when empty/inconsistent).
fn longest_common_prefix(strs: &[&str]) -> String {
    if strs.is_empty() {
        return String::new();
    }
    let first = strs[0];
    let mut end = first.len();
    for s in strs.iter().skip(1) {
        let mut i = 0;
        while i < end && i < s.len() && s.as_bytes()[i] == first.as_bytes()[i] {
            i += 1;
        }
        end = i;
        if end == 0 {
            break;
        }
    }
    first[..end].to_string()
}

/// Conversation line kinds, mirroring the TUI's ConvKind. Each renders with a
/// coloured prefix via Pango markup in the conversation text view.
#[derive(Clone, Copy, PartialEq, Debug)]
enum ConvKind {
    User,
    Assistant,
    System,
    Error,
    Thinking,
}

impl ConvKind {
    /// (hex foreground colour, prefix) — same palette as the TUI's `into_line`.
    fn style(self) -> (&'static str, &'static str) {
        match self {
            ConvKind::User => ("#4ec9b0", "❯ "),
            ConvKind::Assistant => ("#d4d4d4", "  "),
            ConvKind::System => ("#569cd6", "· "),
            ConvKind::Error => ("#f14c4c", "✗ "),
            ConvKind::Thinking => ("#c586c0", "💭 "),
        }
    }
}

/// One conversation line (a single visual row in the pane).
#[derive(Clone)]
struct ConvLine {
    kind: ConvKind,
    text: String,
}

/// Escape text for inclusion in Pango markup (ampersand, angle brackets).
fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Render one conversation line as Pango markup with a coloured prefix span.
/// The body text is escaped, so model output containing `<`/`&` stays intact.
fn line_markup(line: &ConvLine) -> String {
    let (color, prefix) = line.kind.style();
    format!(
        "<span foreground=\"{color}\">{}</span>{}",
        escape_markup(prefix),
        escape_markup(&line.text)
    )
}

/// Shared per-UI state, guarded by a Mutex so both the Entry-activate callback
/// (main thread) and the periodic drain callback (also main thread via glib)
/// can see it. Since glib callbacks run on the main loop, contention is trivial.
struct GuiState {
    /// Conversation lines (the transcript so far), each with a colour kind.
    conv: Vec<ConvLine>,
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
    /// Prompt history (most-recent-LAST, same order rustyline keeps). Seeded
    /// from the session's `.history` file at startup and appended on every
    /// submitted prompt, so Arrow-Up recalls previous prompts like the
    /// terminal REPLs.
    history: Vec<String>,
    /// Re-exec broadcast generation seen at startup (durable fallback for a
    /// missed SIGUSR1); updated as newer generations are honoured.
    reexec_seen: u64,
    /// How many conversation lines are already in the backend's text buffer.
    /// `sync_textview` is append-only, so this is its write cursor.
    ///
    /// This lives on the *session*, not in a `static`: with a process-global
    /// cursor, a `/clear` or a scrollback trim in one tab would desync every
    /// other tab's cursor, and panes would stop matching their own models.
    rendered: usize,
    /// Set when the text buffer must be rebuilt from scratch (after `/clear`,
    /// or a trim that dropped lines from the front of the model).
    needs_rebuild: bool,
    /// Background turns started with `/bg` or a trailing `&`.
    bg: Vec<BackgroundTurn>,
    /// Next background-job id (1-based, like the TUI's `next_bg_id`).
    next_bg_id: usize,
    /// Prompts queued by extensions, drained into a new turn when idle. The
    /// TUI does this; without it the GUI silently drops extension follow-ups.
    continuations: Vec<String>,
    /// Braille spinner frame, advanced by the periodic drain each tick.
    spinner: usize,
    /// Last status string handed to the backend, so we only touch the label
    /// when it actually changes (every `set_markup` forces a relayout).
    last_status: String,
    /// The agent's model label, cached for the idle footer (the TUI's
    /// `workspace:`/`model:` line). Refreshed whenever the agent is reachable.
    last_model: Option<String>,
}

/// A detached/background turn: its prompt, its own cancel flag, and the agent
/// it runs (kept out of the foreground slot so the UI stays responsive).
struct BackgroundTurn {
    id: usize,
    prompt: String,
    cancel: Arc<AtomicBool>,
    /// The agent comes back here when the worker finishes.
    slot: AgentSlot,
    join: Option<std::thread::JoinHandle<()>>,
}

/// Run a shell command and capture its combined output for the pane.
///
/// The GUI never hands the terminal to a child (there is no alternate screen to
/// leave and raw mode is not ours to restore), so `/sh` runs the command with
/// piped stdio and shows the result. The abridged helper keeps the pane from
/// filling with a runaway `cat` of a huge file, matching how tool output is
/// echoed everywhere else.
fn run_shell_guarded(cmdline: &str) -> Result<String, String> {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmdline)
        .output()
        .map_err(|e| format!("could not run shell: {e}"))?;
    let mut text = String::new();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stdout.trim().is_empty() {
        text.push_str(&crate::agent::abridge_output(stdout.trim_end()));
    }
    if !stderr.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&crate::agent::abridge_output(stderr.trim_end()));
    }
    if text.trim().is_empty() {
        text.push_str(&format!("(no output, exit {})", out.status.code().unwrap_or(-1)));
    } else if let Some(code) = out.status.code() {
        if code != 0 {
            text.push_str(&format!("\n(exit {code})"));
        }
    }
    Ok(text)
}

/// Push the current background-job table into the pane (the TUI's `/jobs`).
fn list_jobs(s: &mut GuiState) {
    if s.bg.is_empty() {
        s.push(ConvKind::System, "· no background jobs — start one with /bg <prompt>");
        return;
    }
    let rows: Vec<(usize, bool, String)> = s
        .bg
        .iter()
        .map(|b| (b.id, b.join.as_ref().is_some_and(|h| !h.is_finished()), b.prompt.clone()))
        .collect();
    s.push(ConvKind::System, &format!("background jobs ({}):", rows.len()));
    for (id, running, prompt) in rows {
        s.push(
            ConvKind::System,
            &format!("  #{id} {} {}", if running { "running" } else { "done   " }, truncate(&prompt, 60)),
        );
    }
}

/// The GUI's `/help` body. Kept in step with `commands::SLASH_COMMANDS` by the
/// drift test at the bottom of this file.
const HELP_GUI: &str = "\
commands:
  /help                 this list
  /model <sel>          switch model      /models      list every model
  /usage                tokens this session
  /goal [obj]           show or start the standing goal
  /continue             drive the goal to completion
  /thinking <lvl>       off|minimal|low|medium|high|xhigh|max  (+ show|hide)
  /clear                clear the pane    /undo [all]  rewind the last turn
  /finished             mark this session finished
  /fix                  repair the workspace VCS setup
  /sh <cmd>             run a shell command   (/! <cmd> also works)
  /bg <prompt>          start a background turn   /jobs   list them
  /cancel               stop the running turn
  /reload               reload catalog, policy and extension commands
  /reexec               restart in place (idle only)
  /sessions             new|close|next|prev|<n>  multi-session tabs
  /exit                 quit";

const SPINNER_FRAMES: [&str; 10] =
    ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How many background turns are still running (shown in the status line, the
/// TUI's `#tasks running: N`).
fn tasks_running(s: &GuiState) -> usize {
    s.bg.iter().filter(|b| b.join.as_ref().is_some_and(|h| !h.is_finished())).count()
}

impl GuiState {
    fn new() -> Self {
        GuiState {
            conv: Vec::new(),
            pending: Vec::new(),
            running: false,
            log_offset: 0,
            status: "idle".into(),
            show_thinking: true,
            history: Vec::new(),
            reexec_seen: crate::config::read_reexec_broadcast()
                .map(|b| b.generation)
                .unwrap_or(0),
            rendered: 0,
            needs_rebuild: true,
            bg: Vec::new(),
            next_bg_id: 1,
            continuations: Vec::new(),
            spinner: 0,
            last_status: String::new(),
            last_model: None,
        }
    }

    /// Append a conversation line, splitting on newlines and capping
    /// scrollback so memory stays bounded.
    ///
    /// The cap keeps the model and the buffer in step: trimming from the front
    /// means the next sync must rebuild rather than append, so we bump
    /// `rendered` down with the model and flag `needs_rebuild`.
    fn push(&mut self, kind: ConvKind, text: &str) {
        for para in text.split('\n') {
            self.conv.push(ConvLine { kind, text: para.to_string() });
        }
        const MAX: usize = 4000;
        if self.conv.len() > MAX {
            let drop = self.conv.len() - MAX;
            self.conv.drain(0..drop);
            self.rendered = self.rendered.saturating_sub(drop);
            self.needs_rebuild = true;
        }
    }

    /// Clear the conversation (e.g. `/clear`).
    fn clear(&mut self) {
        self.conv.clear();
        self.rendered = 0;
        self.needs_rebuild = true;
    }
}

/// Arrow-Up/Down navigation state for the prompt Entry, shared between the key
/// controller and the activate handler. `pos` indexes `history` (== len means
/// "past the last entry", i.e. the live draft); `draft` remembers the
/// partially-typed line so Arrow-Down can restore it.
#[derive(Default)]
struct HistoryNav {
    pos: Option<usize>,
    draft: String,
}

impl HistoryNav {
    /// Walk `history` in the given direction (-1 = older / Arrow-Up, +1 =
    /// newer / Arrow-Down) from the current entry text. Returns the text the
    /// Entry should now show, or `None` when the boundary is reached (no-op).
    fn navigate(&mut self, dir: i32, cur: &str, history: &[String]) -> Option<String> {
        if history.is_empty() {
            return None;
        }
        let last = history.len() - 1;
        match dir {
            // Arrow-Up: go one entry older. From the live draft, remember the
            // draft and jump to the newest entry.
            -1 => {
                let new_pos = match self.pos {
                    None => {
                        self.draft = cur.to_string();
                        last
                    }
                    Some(p) if p == 0 => return None, // already oldest
                    Some(p) => p - 1,
                };
                self.pos = Some(new_pos);
                Some(history[new_pos].clone())
            }
            // Arrow-Down: go newer; past the newest entry restores the draft.
            1 => {
                let p = self.pos?; // not navigating: ignore Arrow-Down
                if p >= last {
                    self.pos = None;
                    return Some(std::mem::take(&mut self.draft));
                }
                self.pos = Some(p + 1);
                Some(history[p + 1].clone())
            }
            _ => None,
        }
    }

    /// Reset the navigation state (called after a prompt is submitted).
    fn reset(&mut self) {
        self.pos = None;
        self.draft.clear();
    }
}

/// Handle to the GTK widgets we mutate from the periodic drain callback.
#[derive(Clone)]
struct GuiWidgets {
    textview: TextView,
    status: GuiLabel,
}

/// Everything that is per-session when the GUI shows one tab per session:
/// its own conversation model, widgets, agent slot, turn canceller, prompt
/// history and completion channel. One `SessionView` lives in the `tabs`
/// `TabView` at the same index.
#[derive(Clone)]
struct SessionView {
    state: Arc<Mutex<GuiState>>,
    widgets: GuiWidgets,
    entry: GuiEntry,
    agent_slot: AgentSlot,
    fg_cancel: Arc<AtomicBool>,
    fg_quiet: Arc<AtomicBool>,
    hist_nav: Arc<Mutex<HistoryNav>>,
    done_tx: smol::channel::Sender<()>,
    app: App,
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

    // -- Build the window + horizontal-tab container --
    let window = app.create_window().map_err(|e| format!("create_window: {e}"))?;
    window.set_title("pir");
    window.set_default_size(820, 600);

    let tabs = app.create_tabview().map_err(|e| format!("tabview: {e}"))?;
    window.set_child(&tabs);

    // One SessionView per open tab, indexed identically to the TabView.
    let sessions: Rc<RefCell<Vec<SessionView>>> = Rc::new(RefCell::new(Vec::new()));
    let active: Rc<Cell<usize>> = Rc::new(Cell::new(0));

    // Switching tabs just moves keyboard focus to that tab's prompt.
    {
        let sessions_for_cb = sessions.clone();
        let active_for_cb = active.clone();
        tabs.set_on_tab_changed(Box::new(move |idx: usize| {
            active_for_cb.set(idx);
            let s = sessions_for_cb.borrow();
            if let Some(sv) = s.get(idx) {
                sv.entry.grab_focus();
            }
        }));
    }

    // Open the initial session using the agent the caller handed us.
    {
        let (done_tx, _done_rx) = smol::channel::bounded::<()>(1);
        spawn_session(
            "session",
            agent_slot.clone(),
            fg_cancel.clone(),
            fg_quiet.clone(),
            done_tx,
            &app,
            &tabs,
            providers,
            bus,
            _full_auto,
            &sessions,
        );
    }

    window.present();

    // -- Periodic drain: refresh every session's conversation from its log --
    // A recurring timeout (~150ms) that drains each agent's log into its own
    // conversation pane. Snapshot the views first so we don't hold the sessions
    // lock across the (potentially slow) drain of every session.
    let drain_sessions = sessions.clone();
    let drain_bus = bus.clone();
    #[cfg(all(target_os = "linux", not(windows)))]
    {
        if let Some(loader) = rustxwidgets::backends::gtk::loader() {
            rustxwidgets::gtk_dynamic_loader::timeout_add_recurring(&loader, 150, Box::new(move || {
                for sv in &*drain_sessions.borrow() {
                    drain_once(
                        &sv.state,
                        &sv.widgets,
                        &sv.agent_slot,
                        &sv.fg_cancel,
                        &sv.fg_quiet,
                        &sv.done_tx,
                        &drain_bus,
                    );
                }
            }));
        }
    }

    // Window close (destroy signal) -> quit the GTK main loop.
    #[cfg(all(target_os = "linux", not(windows)))]
    unsafe {
        if let Some(loader) = rustxwidgets::backends::gtk::loader() {
            let app_for_destroy = app.clone();
            let _ = rustxwidgets::gtk_dynamic_loader::connect_signal(
                &loader.symbols,
                window.raw_handle(),
                "destroy",
                Box::new(move || {
                    app_for_destroy.quit();
                }),
                2,
            );
        }
    }

    // Pure-NWG (Windows) periodic drain + window-close handling. The GTK loader
    // paths above are linux-only, so on Windows nothing GTK is compiled.
    #[cfg(windows)]
    {
        let drain_sessions = drain_sessions.clone();
        let drain_bus = drain_bus.clone();
        window.timeout_add_recurring(150, Box::new(move || {
            for sv in &*drain_sessions.borrow() {
                drain_once(
                    &sv.state,
                    &sv.widgets,
                    &sv.agent_slot,
                    &sv.fg_cancel,
                    &sv.fg_quiet,
                    &sv.done_tx,
                    &drain_bus,
                );
            }
        }));
        window.on_close(Box::new(|| {
            rustxwidgets::backends_nwg_adapter::quit_main_loop();
        }));
    }

    app.run().map_err(|e| format!("gtk main loop: {e}"))
}

/// Build one session tab: its own conversation pane + prompt + status line,
/// wired to its own agent slot / canceller / history. Pushes the resulting
/// `SessionView` into `sessions` at the same index the `TabView` created.
fn spawn_session(
    title: &str,
    agent_slot: AgentSlot,
    fg_cancel: Arc<AtomicBool>,
    fg_quiet: Arc<AtomicBool>,
    done_tx: smol::channel::Sender<()>,
    app: &App,
    tabs: &TabView,
    providers: &[Provider],
    bus: &SharedBus,
    _full_auto: bool,
    sessions: &Rc<RefCell<Vec<SessionView>>>,
) -> usize {
    let idx = tabs.add_tab(title).expect("tabview: add_tab");

    // Vertical box: [conversation TextView (expand)] [Entry] [status]
    let vbox = app
        .create_box(Orientation::Vertical, 4)
        .expect("create_box");
    let textview = app.create_textview().expect("create_textview");
    configure_conversation_textview(&textview);
    vbox.append(&textview);

    let entry: GuiEntry = app.create_entry().expect("create_entry");
    entry.set_hexpand(true);
    vbox.append(&entry);

    let status = app.create_label("pir · idle").expect("create_label");
    #[cfg(all(target_os = "linux", not(windows)))]
    status.set_xalign(0.0); // left-align
    vbox.append(&status);

    let panel = tabs.tab_box(idx).expect("tabview: tab_box");
    panel.append(&vbox);
    vbox.set_hexpand(true);
    vbox.set_vexpand(true);

    let state = Arc::new(Mutex::new(GuiState::new()));
    let widgets = GuiWidgets { textview: textview.clone(), status: status.clone() };
    // Seed Arrow-Up history from the session's `.history` file (same store the
    // streaming REPL uses) and greet the user.
    {
        let mut s = state.lock().unwrap();
        s.history = term::load_history_lines();
        s.push(
            ConvKind::System,
            &format!(
                "pir · {} · GTK GUI  (/help · /sessions new|close|next|prev|<n> · Enter to send · ctrl-q to quit)",
                providers[0].pid()
            ),
        );
        sync_textview(&widgets, &mut s);
    }

    let hist_nav = Arc::new(Mutex::new(HistoryNav::default()));
    let app_quit = app.clone();

    // Keep keyboard focus on this tab's Entry (the REPL prompt).
    let entry_focus = entry.clone();
    let _ = entry.connect_focus_out_event(move |_e: *mut c_void| {
        entry_focus.grab_focus();
        1
    });

    // Tab completion + history navigation on this tab's prompt.
    let entry_comp_cb = entry.clone();
    let entry_cb_providers_tab: Vec<Provider> = providers.to_vec();
    let nav_for_keys = hist_nav.clone();
    let hist_for_keys = state.clone();
    let app_for_keys = app.clone();
    let _ = entry.on_key_raw(Box::new(move |keyval: u32, kstate: u32| -> bool {
        const CONTROL_MASK: u32 = 1 << 2;
        match keyval {
            GDK_KEY_Q if (kstate & CONTROL_MASK) != 0 => {
                let _ = app_for_keys.clone().quit();
                true
            }
            GDK_KEY_TAB => {
                let cur = entry_comp_cb.get_text().unwrap_or_default();
                let completed = complete_idle(&cur).or_else(|| {
                    let provs: Vec<Provider> = entry_cb_providers_tab.clone();
                    crate::config::complete_model_buffer(&cur, &provs)
                });
                if let Some(completed) = completed {
                    entry_comp_cb.set_text(&completed);
                }
                true
            }
            GDK_KEY_UP | GDK_KEY_DOWN => {
                let dir = if keyval == GDK_KEY_UP { -1 } else { 1 };
                let cur = entry_comp_cb.get_text().unwrap_or_default();
                let mut nav = nav_for_keys.lock().unwrap();
                let history = hist_for_keys.lock().unwrap().history.clone();
                if let Some(new_text) = nav.navigate(dir, &cur, &history) {
                    entry_comp_cb.set_text(&new_text);
                }
                true
            }
            _ => false,
        }
    }));

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
    let nav_for_activate = hist_nav.clone();
    let app_for_activate = app.clone();
    let sessions_for_activate = sessions.clone();
    let tabs_for_activate = tabs.clone();
    let _ = entry.connect_activate(move |_data: *mut c_void| {
        let text = entry_in_cb.get_text().unwrap_or_default();
        entry_in_cb.set_text("");
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }

        // Record the submitted line in the shared prompt history (in-memory +
        // the session `.history` file) so Arrow-Up recalls it, exactly like
        // the terminal REPLs. Also reset any in-progress Arrow-Up navigation.
        term::push_history(&text);
        entry_cb_state.lock().unwrap().history.push(text.clone());
        nav_for_activate.lock().unwrap().reset();

        if let Some(cmd) = text.strip_prefix('/') {
            // Session management is handled here (it must mutate tabs/sessions).
            let mut parts = cmd.split_whitespace();
            let sub = parts.next().unwrap_or("");
            let rest: Vec<&str> = parts.collect();
            if sub == "sessions" {
                handle_session_command(
                    &rest,
                    &entry_cb_state,
                    &entry_cb_widgets,
                    &sessions_for_activate,
                    &tabs_for_activate,
                    &app_for_activate,
                    &entry_cb_providers,
                    &entry_cb_bus,
                    _full_auto,
                    &entry_cb_slot,
                );
                return;
            }
            handle_command(
                cmd,
                &entry_cb_slot,
                &entry_cb_providers,
                &entry_cb_bus,
                _full_auto,
                &entry_cb_cancel,
                &entry_cb_state,
                &app_for_activate,
                &entry_cb_widgets,
            );
            return;
        }

        // `!cmd` / `!!cmd`: the streaming REPL's shell shorthands.
        if let Some(sh) = text.strip_prefix('!') {
            let sh = sh.strip_prefix('!').unwrap_or(sh).trim();
            if sh.is_empty() {
                entry_cb_state.lock().unwrap().push(ConvKind::Error, "· usage: !<command>");
            } else {
                let msg = match run_shell_guarded(sh) {
                    Ok(out) => out,
                    Err(e) => format!("· {e}"),
                };
                let mut s = entry_cb_state.lock().unwrap();
                s.push(ConvKind::System, &format!("$ {sh}"));
                s.push(ConvKind::System, &msg);
                sync_textview(&entry_cb_widgets, &mut s);
            }
            return;
        }

        // Regular prompt: if a turn is running, queue it; else start it now.
        let mut s = entry_cb_state.lock().unwrap();

        // A trailing `&` runs the turn in the background and frees the prompt
        // immediately — the TUI's `foo &`.
        let (prompt, background) = match text.strip_suffix('&') {
            Some(rest) if !rest.trim().is_empty() => (rest.trim().to_string(), true),
            _ => (text.clone(), false),
        };

        if background {
            s.push(ConvKind::User, &prompt);
            match spawn_background(&prompt, &entry_cb_slot, &entry_cb_providers, &entry_cb_bus, _full_auto, &mut s) {
                Ok(id) => s.push(ConvKind::System, &format!("· started background job #{id}")),
                Err(e) => s.push(ConvKind::Error, &e),
            }
            s.status = idle_status(&s);
            sync_textview(&entry_cb_widgets, &mut s);
            return;
        }

        if s.running {
            s.pending.push(prompt.clone());
            s.status = running_status(&s, s.pending.len());
            sync_textview(&entry_cb_widgets, &mut s);
            return;
        }
        s.running = true;
        s.status = running_status(&s, 0);
        s.push(ConvKind::User, &prompt);
        sync_textview(&entry_cb_widgets, &mut s);
        drop(s);

        // Reset the quiet switch so a previously detached turn's quiet state
        // can't leak.
        entry_cb_quiet.store(false, Ordering::SeqCst);
        spawn_turn(
            &entry_cb_slot,
            &entry_cb_cancel,
            &entry_cb_quiet,
            prompt,
            entry_cb_done.clone(),
        );
    });

    sessions.borrow_mut().push(SessionView {
        state,
        widgets,
        entry,
        agent_slot,
        fg_cancel,
        fg_quiet,
        hist_nav,
        done_tx,
        app: app_quit,
    });
    idx
}

/// Handle `/sessions` sub-commands: `new` (open a tab with a fresh agent),
/// `close` (close the current tab, keeping at least one), `next`/`prev` and
/// `<n>` (switch), and (no arg) list the open sessions. Feedback is written to
/// the *current* session's conversation pane.
fn handle_session_command(
    rest: &[&str],
    state: &Arc<Mutex<GuiState>>,
    widgets: &GuiWidgets,
    sessions: &Rc<RefCell<Vec<SessionView>>>,
    tabs: &TabView,
    app: &App,
    providers: &[Provider],
    bus: &SharedBus,
    _full_auto: bool,
    sv_slot: &AgentSlot,
) {
    let mut s = state.lock().unwrap();
    if rest.is_empty() {
        let cur = tabs.current_tab();
        let titles: Vec<String> = (0..tabs.tab_count())
            .map(|i| tabs.tab_title(i).unwrap_or_default())
            .collect();
        s.push(ConvKind::System, &format!("sessions ({} open, * = active):", titles.len()));
        for (i, t) in titles.iter().enumerate() {
            s.push(ConvKind::System, &format!("  {}{} {}", if i == cur { "*" } else { " " }, i, t));
        }
        s.push(ConvKind::System, "use: /sessions new | close | next | prev | <n>");
        sync_textview(widgets, &mut s);
        return;
    }
    match rest[0] {
        "new" => {
            drop(s);
            // Mirror the current session's model for the new agent (fall back to
            // the default selection if the agent is mid-turn and unavailable).
            let (provider, model) = {
                let g = sv_slot.lock().unwrap();
                match g.as_ref() {
                    Some(a) => (a.provider(), a.model()),
                    None => match crate::config::select(providers, "") {
                        Ok((p, m)) => (p.clone(), m.clone()),
                        Err(_) => {
                            let m = providers[0]
                                .models
                                .first()
                                .cloned()
                                .unwrap_or_else(|| providers[0].models[0].clone());
                            (providers[0].clone(), m)
                        }
                    },
                }
            };
            let cancel = Arc::new(AtomicBool::new(false));
            let quiet = Arc::new(AtomicBool::new(false));
            let (done_tx, _rx) = smol::channel::bounded::<()>(1);
            match Agent::new(
                provider,
                model,
                _full_auto,
                true,
                bus.clone(),
                None,
                cancel.clone(),
                Arc::new(Mutex::new(String::new())),
            ) {
                Ok(agent) => {
                    let slot: AgentSlot = Arc::new(Mutex::new(Some(agent)));
                    let n = spawn_session(
                        &format!("session {}", sessions.borrow().len()),
                        slot,
                        cancel,
                        quiet,
                        done_tx,
                        app,
                        tabs,
                        providers,
                        bus,
                        _full_auto,
                        sessions,
                    );
                    tabs.set_active(n);
                }
                Err(e) => {
                    let mut s2 = state.lock().unwrap();
                    s2.push(ConvKind::Error, &format!("could not start new session: {e}"));
                    sync_textview(widgets, &mut s2);
                }
            }
        }
        "close" => {
            let cur = tabs.current_tab();
            if sessions.borrow().len() <= 1 {
                s.push(ConvKind::System, "only one session open — not closing");
                sync_textview(widgets, &mut s);
                return;
            }
            drop(s);
            tabs.close_tab(cur);
            sessions.borrow_mut().remove(cur);
        }
        "next" | "prev" => {
            let n = sessions.borrow().len();
            let cur = tabs.current_tab();
            let target = if rest[0] == "next" {
                (cur + 1) % n
            } else {
                (cur + n - 1) % n
            };
            tabs.set_active(target);
        }
        other => {
            if let Ok(i) = other.parse::<usize>() {
                let n = sessions.borrow().len();
                if i < n {
                    tabs.set_active(i);
                    return;
                }
            }
            s.push(
                ConvKind::Error,
                &format!("unknown /sessions subcommand '{other}' (new|close|next|prev|<n>|list)"),
            );
            sync_textview(widgets, &mut s);
        }
    }
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
            s.push(ConvKind::System, &format!("notify: {}", e.summary()));
        }
    }

    // Tail the session log, and cache the model label for the idle footer.
    {
        let g = agent_slot.lock().unwrap();
        if let Some(a) = g.as_ref() {
            let label = a.label();
            s.last_model = Some(label);
            if let Some(log) = a.log_path.clone() {
                drain_session_log(log, &mut s);
            }
        }
    }

    // Turn finished? Join it and start the next queued prompt or go idle.
    // We detect completion by checking whether the agent is back in the slot
    // AND `running` is still set (the worker returns the agent on completion).
    // Deferred self re-exec (SIGUSR1 from `/reexec all`, or the reexec
    // broadcast file): only when idle (agent back in slot, no turn running).
    // GTK has no exec-safe teardown issue here — `app.quit()` unwinds the
    // main loop first would be ideal, but exec replaces the image anyway.
    #[cfg(unix)]
    if !s.running {
        let mut wants_reexec = false;
        if crate::reexec_requested() {
            crate::clear_reexec_request();
            wants_reexec = true;
        } else if let Some(b) = crate::config::read_reexec_broadcast()
            && b.generation > s.reexec_seen
            && b.generation != 0
            && b.by_pid != std::process::id() as u64 {
                s.reexec_seen = b.generation;
                wants_reexec = true;
            }
        if wants_reexec {
            s.push(ConvKind::System, "· reexec requested — restarting…");
            sync_textview(widgets, &mut s);
            crate::reexec_self();
        }
    }
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
            // Extensions can queue a follow-up prompt; drain it into a new turn
            // instead of dropping it (this is what the TUI does, and what kept
            // the GUI from silently losing post-turn work).
            if s.continuations.is_empty() {
                let next = s.pending.drain(..).next();
                if let Some(next) = next {
                    s.running = true;
                    s.status = running_status(&s, 0);
                    s.push(ConvKind::User, &next);
                    sync_textview(widgets, &mut s);
                    drop(s);
                    fg_quiet.store(false, Ordering::SeqCst);
                    spawn_turn(agent_slot, fg_cancel, fg_quiet, next, done.clone());
                    return;
                }
            }
        }
    }

    // -- Idle: start any continuation the agent queued during the last turn --
    if !s.running {
        let conts = {
            let mut g = agent_slot.lock().unwrap();
            match g.as_mut() {
                Some(a) => a.take_continuations(),
                None => Vec::new(),
            }
        };
        for c in conts {
            s.continuations.push(c);
        }
        if let Some(next) = s.continuations.first().cloned() {
            s.continuations.remove(0);
            s.running = true;
            s.status = running_status(&s, 0);
            s.push(ConvKind::User, &next);
            sync_textview(widgets, &mut s);
            drop(s);
            fg_quiet.store(false, Ordering::SeqCst);
            spawn_turn(agent_slot, fg_cancel, fg_quiet, next, done.clone());
            return;
        }
    }

    // -- Reap finished background turns and report them (the TUI's reap_bg) --
    reap_background(&mut s);

    // -- Spinner + status parity with the TUI's footer --
    if s.running {
        s.spinner = (s.spinner + 1) % SPINNER_FRAMES.len();
        s.status = running_status(&s, 0);
    } else {
        s.status = idle_status(&s);
    }

    sync_textview(widgets, &mut s);
}

/// The status line while a turn runs: a braille spinner, the queued-prompt
/// count, and the number of background jobs — the GUI's stand-in for the
/// TUI's `⠋ thinking…` status plus its `#tasks running: N` footer.
fn running_status(s: &GuiState, queued: usize) -> String {
    let spin = SPINNER_FRAMES[s.spinner % SPINNER_FRAMES.len()];
    let mut st = format!("{spin} thinking…");
    if queued > 0 {
        st.push_str(&format!(" · {} queued", queued));
    }
    let bg = tasks_running(s);
    if bg > 0 {
        st.push_str(&format!(" · #{bg} background"));
    }
    st
}

/// The status line when idle: the TUI's footer shows the workspace and model,
/// so the GUI shows the same pair plus a background-job count when any remain.
fn idle_status(s: &GuiState) -> String {
    let mut parts = vec![format!("workspace: {}", crate::workspace_label())];
    if let Some(a) = s.last_model.as_deref() {
        parts.push(format!("model: {a}"));
    }
    let bg = tasks_running(s);
    if bg > 0 {
        parts.push(format!("#{bg} background"));
    }
    parts.join("   ")
}

/// Join background turns that have finished, dropping each from the job list
/// (the TUI's `reap_bg`). Their output still reaches the pane: a background
/// agent writes to its own log, which `drain_once` tails like any other.
fn reap_background(s: &mut GuiState) {
    let mut i = 0;
    while i < s.bg.len() {
        let done = s.bg[i]
            .join
            .as_ref()
            .map(|h| h.is_finished())
            .unwrap_or(true);
        if done {
            let job = s.bg.remove(i);
            if let Some(h) = job.join {
                let _ = h.join();
            }
            let still = tasks_running(s);
            let note = if still == 0 {
                String::new()
            } else {
                format!(" · {still} still running")
            };
            s.push(
                ConvKind::System,
                &format!("· background job #{} done{note}", job.id),
            );
        } else {
            i += 1;
        }
    }
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

/// Start `prompt` as a background turn on its own agent, so the UI stays
/// responsive (the TUI's `spawn_background`, reachable here via `/bg <text>`
/// and a trailing `&`).
///
/// The background agent mirrors the current one (same provider + model) but
/// gets its own slot and cancel flag, so it cannot collide with the
/// foreground turn and `/cancel` can stop it independently.
fn spawn_background(
    prompt: &str,
    fg_slot: &AgentSlot,
    providers: &[Provider],
    bus: &SharedBus,
    full_auto: bool,
    s: &mut GuiState,
) -> Result<usize, String> {
    let (provider, model) = {
        let g = fg_slot.lock().unwrap();
        match g.as_ref() {
            Some(a) => (a.provider().clone(), a.model().clone()),
            None => match crate::config::select(providers, "") {
                Ok((p, m)) => (p.clone(), m.clone()),
                Err(e) => return Err(format!("no model available for a background turn: {e}")),
            },
        }
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let agent = Agent::new(
        provider,
        model,
        full_auto,
        true,
        bus.clone(),
        None,
        cancel.clone(),
        Arc::new(Mutex::new(String::new())),
    )
    .map_err(|e| format!("could not start a background turn: {e}"))?;

    let slot: AgentSlot = Arc::new(Mutex::new(Some(agent)));
    let id = s.next_bg_id;
    s.next_bg_id += 1;

    // Detach the agent onto its own worker; the join handle lets the periodic
    // drain reap it once the turn completes.
    let worker_slot = slot.clone();
    let worker_cancel = cancel.clone();
    let worker_bus = bus.clone();
    let worker_prompt = prompt.to_string();
    let join = std::thread::spawn(move || {
        if let Some(mut a) = worker_slot.lock().unwrap().take() {
            // A background turn reports its own failure: the pane is not being
            // driven by this worker, so `notify_on_exit`/`error_event` are how
            // the user finds out it failed.
            match a.turn(&worker_prompt) {
                Ok(()) => {
                    a.notify_on_exit(a.turn_done_event());
                }
                Err(e) => {
                    let _ = a.error_event(e);
                }
            }
            *worker_slot.lock().unwrap() = Some(a);
        }
        let _ = worker_bus;
        // Clear the flag so a later `/bg` on the same slot isn't born
        // pre-cancelled.
        worker_cancel.store(false, std::sync::atomic::Ordering::SeqCst);
    });

    s.bg.push(BackgroundTurn {
        id,
        prompt: prompt.to_string(),
        cancel,
        slot,
        join: Some(join),
    });
    Ok(id)
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
        let mut kind = ConvKind::Assistant;
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
                                state.push(ConvKind::Thinking, &format!("💭 {t}"));
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
                        // Same abridged multi-line echo as the live REPL, so a
                        // resumed session shows command output, not just one
                        // line of it.
                        for line in crate::agent::abridge_output(c).lines() {
                            text.push_str(&format!("  {line}\n"));
                        }
                    }
                    _ => {}
                }
            }
        } else {
            continue;
        }
        let text = text.trim_end().to_string();
        if !text.is_empty() {
            state.push(kind, &text);
                    }
    }
    // Bound scrollback.
    if state.conv.len() > 200_000 {
        let keep = state.conv.len() - 200_000;
        state.conv = state.conv.split_off(keep);
    }
}

/// Configure the conversation pane's TextView. It must never capture keyboard
/// focus or accept input: read-only + non-focusable, so clicking it (or a
/// stray Tab) can't steal typing from the prompt Entry. Otherwise a half-typed
/// word lands in the text view and the next periodic drain's `set_text` wipes
/// it ("I typed half a word and it just disappeared").
fn configure_conversation_textview(tv: &TextView) {
    #[cfg(all(target_os = "linux", not(windows)))]
    tv.set_hexpand(true);
    #[cfg(all(target_os = "linux", not(windows)))]
    tv.set_vexpand(true);
    // GTK wrap mode: 2 = GTK_WRAP_WORD_CHAR
    tv.set_wrap_mode(2);
    tv.set_editable(false);
    #[cfg(all(target_os = "linux", not(windows)))]
    tv.set_can_focus(false);
}

/// Render the status text as Pango markup: running/queued states in cyan,
/// error-ish ("error", "cancel") in red, everything else dimmed grey.
fn status_markup(status: &str) -> String {
    let color = if status.contains("running") || status.contains("queued") {
        "#569cd6" // cyan: work in flight
    } else if status.to_lowercase().contains("error") || status.contains("cancel") {
        "#f14c4c" // red: trouble
    } else {
        "#9e9e9e" // dim grey: idle / usage
    };
    format!("<span foreground=\"{color}\">{}</span>", escape_markup(status))
}

/// Sync the backend's text buffer with the conversation model.
///
/// The buffer is append-only between rebuilds: each new line is appended
/// (`append_text` on GTK) rather than replacing the whole buffer. The old code
/// called `set_text("")` at the end of the loop, which on GTK goes to
/// `gtk_text_buffer_set_text` — a *replace* — so the pane ended up blank.
///
/// `sync_textview` takes `&mut GuiState` because the per-session write cursor
/// lives in the state; it must not hold the state's lock while it runs.
#[cfg(all(target_os = "linux", not(windows)))]
fn sync_textview(widgets: &GuiWidgets, state: &mut GuiState) {
    if state.needs_rebuild {
        widgets.textview.set_text("");
        state.rendered = 0;
        state.needs_rebuild = false;
    }
    if state.conv.len() > state.rendered {
        for line in &state.conv[state.rendered..] {
            // The backend has no per-range markup, so the line goes in as
            // plain text; the conversation pane is monochrome. Colour lives in
            // the per-line prefix glyph and in the status label.
            let (_, prefix) = line.kind.style();
            widgets.textview.append_text(&format!("{prefix}{}\n", line.text));
        }
        state.rendered = state.conv.len();
    }
    if state.last_status != state.status {
        widgets.status.set_markup(&status_markup(&state.status));
        state.last_status = state.status.clone();
    }
}

#[cfg(windows)]
fn sync_textview(widgets: &GuiWidgets, state: &mut GuiState) {
    if state.needs_rebuild {
        widgets.textview.set_text("");
        state.rendered = 0;
        state.needs_rebuild = false;
    }
    if state.conv.len() > state.rendered {
        for line in &state.conv[state.rendered..] {
            // The nwg backend's `TextView::set_markup` already appends the
            // (markup-stripped) line plus a newline, so no separate newline.
            widgets.textview.set_markup(&line_markup(line));
        }
        state.rendered = state.conv.len();
    }
    if state.last_status != state.status {
        widgets.status.set_markup(&status_markup(&state.status));
        state.last_status = state.status.clone();
    }
}

/// Handle a slash command inline (a GUI-appropriate subset). Mirrors the
/// streaming REPL's commands but reads/writes the GUI's own state/agent slot.
fn handle_command(
    cmd: &str,
    agent_slot: &AgentSlot,
    providers: &[Provider],
    bus: &SharedBus,
    full_auto: bool,
    cancel: &Arc<AtomicBool>,
    state: &Arc<Mutex<GuiState>>,
    app: &App,
    widgets: &GuiWidgets,
) {
    let mut parts = cmd.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    let mut s = state.lock().unwrap();
    match cmd {
        "h" | "help" => {
            s.push(ConvKind::System, HELP_GUI);
        }
        "exit" | "quit" | "q" => {
            s.status = "bye".into();
            drop(s);
            app.quit();
            return;
        }
        "cancel" | "c" => {
            // Stop the foreground turn and every background job, so `/cancel`
            // really means "stop work" as it does in the TUI.
            let mut stopped_fg = false;
            if s.running {
                cancel.store(true, Ordering::SeqCst);
                stopped_fg = true;
            }
            let bg_ids: Vec<usize> = s
                .bg
                .iter()
                .filter(|b| b.join.as_ref().is_some_and(|h| !h.is_finished()))
                .map(|b| {
                    b.cancel.store(true, Ordering::SeqCst);
                    b.id
                })
                .collect();
            match (stopped_fg, bg_ids.is_empty()) {
                (true, true) => s.push(ConvKind::System, "· requesting cancel (turn stops now)"),
                (true, false) => s.push(
                    ConvKind::System,
                    &format!(
                        "· requesting cancel (turn + background job{} {})",
                        if bg_ids.len() == 1 { "" } else { "s" },
                        bg_ids.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(", ")
                    ),
                ),
                (false, true) => s.push(ConvKind::System, "· no turn running to cancel"),
                (false, false) => s.push(
                    ConvKind::System,
                    &format!(
                        "· requesting cancel (background job{} {})",
                        if bg_ids.len() == 1 { "" } else { "s" },
                        bg_ids.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(", ")
                    ),
                ),
            }
        }
        "clear" => {
            s.clear();
            s.push(ConvKind::System, "pir · GTK GUI");
        }
        "reload" => {
            // Same in-process refresh as the streaming REPL: catalog +
            // policy + ext commands, no build/exec (see main handle_command).
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                s.push(ConvKind::System, "· agent busy (turn running) — try again when idle");
                return;
            };
            let (n, refreshed) = agent.reload_catalog();
            let label = agent.label();
            let policy = crate::security::load_policy();
            let policy_note = agent.apply_security_policy(&policy);
            let cmds = agent.registry_command_names();
            crate::term::set_extension_commands(cmds);
            s.push(
                ConvKind::System,
                &format!("· reloaded: {n} provider(s), model {label}{} | policy: {policy_note}",
                    if refreshed { " (refreshed)" } else { " (kept — no longer in catalog)" }),
            );
        }
        "thinking" => {
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                s.push(ConvKind::System, "· agent busy (turn running) — try again when idle");
                return;
            };
            let arg = rest.join(" ");
            if arg.trim().is_empty() {
                s.push(ConvKind::System, &format!(
                    "· thinking level: {}  (/thinking <off|minimal|low|medium|high|xhigh|max> [show|hide])",
                    agent.thinking_level().as_str()
                ));
                return;
            }
            let mut show: Option<bool> = None;
            let mut words: Vec<&str> = arg.split_whitespace().collect();
            if let Some(last) = words.last().copied() {
                match last {
                    "show" => {
                        show = Some(true);
                        words.pop();
                    }
                    "hide" => {
                        show = Some(false);
                        words.pop();
                    }
                    _ => {}
                }
            }
            let level_arg = words.join(" ");
            if !level_arg.is_empty() {
                match crate::config::ThinkingLevel::parse(&level_arg) {
                    Some(lvl) => s.push(ConvKind::System, &format!("· {}", agent.set_thinking(lvl))),
                    None => {
                        s.push(
                            ConvKind::Error,
                            &format!("· usage: /thinking [<off|minimal|low|medium|high|xhigh|max>] [show|hide] (got '{level_arg}')"),
                        );
                        return;
                    }
                }
            }
            if let Some(on) = show {
                s.show_thinking = on;
                s.push(ConvKind::System, &format!("· {}", agent.set_show_thinking(on)));
            }
        }
        "usage" => {
            let g = agent_slot.lock().unwrap();
            match g.as_ref() {
                Some(a) => s.push(ConvKind::System, &format!(
                    "{} in / {} out tokens this session",
                    fmt_tok(a.usage.input),
                    fmt_tok(a.usage.output)
                )),
                None => s.push(ConvKind::System, "· agent busy (turn running)"),
            }
        }
        "models" => {
            for p in providers {
                s.push(ConvKind::System, &p.pid());
                for m in &p.models {
                    s.push(ConvKind::System, &format!("  {}", m.id));
                }
            }
        }
        "model" | "m" => {
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                s.push(ConvKind::System, "· agent busy (turn running) — try again when idle");
                return;
            };
            if rest.is_empty() {
                s.push(ConvKind::System, &format!("current model: {}", agent.label()));
            } else {
                match crate::config::select(providers, &rest.join(" ")) {
                    Ok((p, m)) => match agent.switch(p.clone(), m.clone()) {
                        Ok(()) => s.push(ConvKind::System, &format!("→ {}", agent.label())),
                        Err(e) => s.push(ConvKind::Error, &format!("{e}")),
                    },
                    Err(e) => s.push(ConvKind::Error, &format!("{e}")),
                }
            }
        }
        "sessions" => {
            s.push(ConvKind::System, "use `pir -r` from a terminal to resume sessions");
        }
        "goal" => {
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                s.push(ConvKind::System, "· agent busy");
                return;
            };
            let obj: String = rest.join(" ");
            if obj.trim().is_empty() {
                match agent.goal_snapshot() {
                    Some(g) => s.push(ConvKind::System, &g),
                    None => s.push(ConvKind::System, "no goal set — try /goal <objective>"),
                }
            } else {
                agent.start_goal(&obj);
                s.push(ConvKind::System, &format!("goal started: {obj}"));
                // Drive the goal to completion (same as /continue); the cheap
                // light model is a completion backstop. Blocks the GUI main
                // thread for the duration, like /continue.
                let out = agent.continue_goal();
                s.push(ConvKind::System, &out);
            }
        }
        "continue" | "cont" => {
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                s.push(ConvKind::System, "· agent busy");
                return;
            };
            let lp = agent.log_path.clone();
            if let Some(p) = lp {
                agent.attach_goal(&p);
            }
            let out = agent.continue_goal();
            s.push(ConvKind::System, &out);
        }
        "undo" => {
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                s.push(ConvKind::System, "· agent busy");
                return;
            };
            let all = rest.first().map(|x| *x == "all").unwrap_or(false);
            s.push(ConvKind::System, &agent.undo(all));
        }
        "finished" => {
            // Same as the TUI: stamp the session log as finished so it stops
            // showing up in "unfinished" listings.
            let log = agent_slot.lock().unwrap().as_ref().and_then(|a| a.log_path.clone());
            match log {
                Some(p) => {
                    crate::session::mark_finished(&p);
                    s.push(ConvKind::System, "· session marked finished");
                }
                None => s.push(ConvKind::System, "· no session log to mark finished"),
            }
        }
        "fix" => {
            // Repair the workspace's VCS setup (the TUI's `/fix`).
            let repo = std::env::current_dir().unwrap_or_default();
            if crate::project::is_git_repo(&repo) {
                s.push(ConvKind::System, &format!("· {}", crate::project::fix_git_setup(&repo)));
            } else {
                s.push(ConvKind::System, "· not a git repository — nothing to fix");
            }
        }
        "sh" | "shell" => {
            // Run a shell command. The GUI has no alternate screen to leave, so
            // unlike the TUI's `/sh` we cannot hand over the terminal; we run it
            // and stream its output into the pane instead.
            let cmdline = rest.join(" ");
            if cmdline.trim().is_empty() {
                s.push(ConvKind::Error, "· usage: /sh <command>   (/! <command> is a shorthand)");
            } else {
                match run_shell_guarded(&cmdline) {
                    Ok(out) => s.push(ConvKind::System, &out),
                    Err(e) => s.push(ConvKind::Error, &format!("· {e}")),
                }
            }
        }
        "reexec" => {
            // Re-exec in place, but only when idle: mid-turn this would swap
            // the image out from under a running worker. The periodic drain
            // already honours the same request, so refuse rather than fork a
            // second copy that would race it.
            if s.running || tasks_running(&s) > 0 {
                s.push(
                    ConvKind::System,
                    "· /reexec only runs when idle — /cancel first, or wait for the turn",
                );
            } else {
                s.push(ConvKind::System, "· reexec requested — restarting…");
                sync_textview(widgets, &mut s);
                crate::reexec_self();
            }
        }
        "bg" | "background" | "running" => {
            // `/bg` with no prompt lists (like the TUI's `/jobs`); with a
            // prompt it starts a detached turn.
            let prompt = rest.join(" ");
            if prompt.trim().is_empty() {
                list_jobs(&mut s);
            } else {
                match spawn_background(&prompt, agent_slot, providers, bus, full_auto, &mut s) {
                    Ok(id) => s.push(
                        ConvKind::System,
                        &format!("· started background job #{id}: {}", truncate(&prompt, 60)),
                    ),
                    Err(e) => s.push(ConvKind::Error, &e),
                }
            }
        }
        "jobs" => {
            list_jobs(&mut s);
        }
        other => {
            // Try extension-registered slash commands.
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                s.push(ConvKind::System, "· agent busy");
                return;
            };
            match agent.run_registered_command(other, rest.join(" ").trim()) {
                Some(outcome) => s.push(ConvKind::System, &outcome.content),
                None => s.push(ConvKind::Error, &format!("unknown command /{other} — try /help")),
            }
        }
    }
    drop(s);
    sync_textview(widgets, &mut state.lock().unwrap());
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

#[cfg(test)]
mod gui_completion_tests {
    use super::*;

    #[test]
    fn tab_completes_unique_command() {
        assert_eq!(complete_idle("/cle"), Some("/clear".to_string()));
    }

    #[test]
    fn tab_completes_to_common_prefix() {
        assert_eq!(complete_idle("/mod"), Some("/model".to_string()));
        assert_eq!(complete_idle("/m"), Some("/model".to_string())); // model+models share LCP "model"
    }

    #[test]
    fn tab_does_not_complete_past_command_word() {
        assert_eq!(complete_idle("/model xyz"), None);
    }

    #[test]
    fn tab_completes_model_args_end_to_end() {
        // The GUI chains `complete_idle(...).or_else(complete_model_buffer)`:
        // command-name completion handles the command word, the model buffer
        // handles the argument. Pin the combined behaviour with the live muse
        // ids so `/model mu` keeps expanding to the qualified prefix.
        fn muse_providers() -> Vec<Provider> {
            vec![Provider {
                id: Some("opencode-go".into()),
                name: None,
                api: Some("openai-completions".into()),
                base_url: Some("https://opencode.ai/zen/go/v1".into()),
                api_key: None,
                models: vec![
                    crate::config::Model {
                        id: "muse-spark-1.2".into(),
                        name: Some("Muse Spark 1.2".into()),
                        context: Some(1000),
                        max_tokens: None,
                        api_override: None,
                        url_override: None,
                        no_reasoning_effort: false,
                        reasoning: false,
                        thinking_format: None,
                        supports_reasoning_effort: None,
                        session_affinity_format: None,
                        thinking_level_map: Default::default(),
                        price_per_1k: None,
                    },
                    crate::config::Model {
                        id: "muse-spark-1.3".into(),
                        name: Some("Muse Spark 1.3".into()),
                        context: Some(1000),
                        max_tokens: None,
                        api_override: None,
                        url_override: None,
                        no_reasoning_effort: false,
                        reasoning: false,
                        thinking_format: None,
                        supports_reasoning_effort: None,
                        session_affinity_format: None,
                        thinking_level_map: Default::default(),
                        price_per_1k: None,
                    },
                ],
            }]
        }
        let provs = muse_providers();
        let tab = |buf: &str| {
            complete_idle(buf).or_else(|| crate::config::complete_model_buffer(buf, &provs))
        };
        assert_eq!(tab("/model mu"), Some("/model opencode-go/muse-spark-1.".to_string()));
        assert_eq!(tab("/m mu"), Some("/m opencode-go/muse-spark-1.".to_string()));
        assert_eq!(tab("/model"), Some("/model ".to_string()));
        assert_eq!(tab("/model zzz"), None);
        // Command-word completion still wins before the argument.
        assert_eq!(tab("/mod"), Some("/model".to_string()));
    }

    #[test]
    fn tab_completes_thinking_arg() {
        assert_eq!(complete_idle("/thinking hig"), Some("/thinking high".to_string()));
        assert_eq!(complete_idle("/thinking off"), Some("/thinking off".to_string())); // full match
        assert_eq!(complete_idle("/thinking mini"), Some("/thinking minimal".to_string()));
    }

    #[test]
    fn tab_does_nothing_on_plain_prompt() {
        assert_eq!(complete_idle("hello"), None);
        assert_eq!(complete_idle(""), None);
    }

    #[test]
    fn longest_common_prefix_basic() {
        assert_eq!(longest_common_prefix(&["show", "hide", "on", "off"]), "");
        assert_eq!(longest_common_prefix(&["model", "models"]), "model");
        assert_eq!(longest_common_prefix(&[]), "");
    }
}

#[cfg(test)]
mod gui_focus_tests {
    use super::*;

    /// Reproducer for "the text area can capture the keyboard and a half-typed
    /// word disappears": a bare GTK TextView (what the conversation pane used
    /// before the fix) is editable AND can-take-focus by default, so clicking
    /// it steals the keyboard from the prompt Entry — and the next drain tick's
    /// `set_text` then wipes whatever was typed into it. Requires a display
    /// (GTK init); skipped gracefully when no GTK backend can load.
    #[test]
    fn conversation_textview_must_not_capture_keyboard() {
        // TextViews are created through the backend factory (App::init +
        // create_textview), which also runs gtk_init inside Loader::new().
        let Ok(app) = App::init() else {
            eprintln!("skipping: no GTK backend/display available in this environment");
            return;
        };

        // A TextView configured the way the GUI used to build it (no
        // editable/can-focus constraints) exhibits the bug.
        let Ok(buggy) = app.create_textview() else {
            eprintln!("skipping: could not create textview");
            return;
        };
        assert!(
            buggy.get_editable() || buggy.get_can_focus(),
            "precondition: a default TextView must be editable or focusable (that was the bug)"
        );

        // The fix: the GUI's configuration makes it inert to the keyboard.
        let Ok(fixed) = app.create_textview() else {
            eprintln!("skipping: could not create textview");
            return;
        };
        configure_conversation_textview(&fixed);
        assert!(!fixed.get_editable(), "conversation TextView must be read-only");
        assert!(!fixed.get_can_focus(), "conversation TextView must not take keyboard focus");
    }
}

#[cfg(test)]
mod gui_history_tests {
    use super::*;

    fn h(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn arrow_up_recalls_newest_then_older() {
        let hist = h(&["first", "second"]);
        let mut nav = HistoryNav::default();
        assert_eq!(nav.navigate(-1, "", &hist), Some("second".into()));
        assert_eq!(nav.navigate(-1, "second", &hist), Some("first".into()));
        // Already at the oldest entry: no-op.
        assert_eq!(nav.navigate(-1, "first", &hist), None);
    }

    #[test]
    fn arrow_down_returns_toward_draft() {
        let hist = h(&["first", "second"]);
        let mut nav = HistoryNav::default();
        assert_eq!(nav.navigate(-1, "", &hist), Some("second".into()));
        assert_eq!(nav.navigate(-1, "second", &hist), Some("first".into()));
        assert_eq!(nav.navigate(1, "first", &hist), Some("second".into()));
        // Past the newest entry: the remembered draft (empty here) returns.
        assert_eq!(nav.navigate(1, "second", &hist), Some(String::new()));
    }

    #[test]
    fn arrow_down_preserves_partially_typed_draft() {
        let hist = h(&["old prompt"]);
        let mut nav = HistoryNav::default();
        // Half-typed a draft, then Arrow-Up.
        assert_eq!(nav.navigate(-1, "half a wo", &hist), Some("old prompt".into()));
        // Arrow-Down restores exactly the draft.
        assert_eq!(nav.navigate(1, "old prompt", &hist), Some("half a wo".into()));
    }

    #[test]
    fn empty_history_is_a_noop() {
        let hist: Vec<String> = Vec::new();
        let mut nav = HistoryNav::default();
        assert_eq!(nav.navigate(-1, "", &hist), None);
        assert_eq!(nav.navigate(1, "", &hist), None);
    }

    #[test]
    fn arrow_down_without_up_is_ignored() {
        let hist = h(&["x"]);
        let mut nav = HistoryNav::default();
        // Down before any Up: not navigating, keep the current text.
        assert_eq!(nav.navigate(1, "live", &hist), None);
    }

    #[test]
    fn reset_clears_navigation() {
        let hist = h(&["a", "b"]);
        let mut nav = HistoryNav::default();
        assert_eq!(nav.navigate(-1, "", &hist), Some("b".into()));
        nav.reset();
        // After reset we're back at the live draft: Up jumps to newest again.
        assert_eq!(nav.navigate(-1, "draft", &hist), Some("b".into()));
        assert_eq!(nav.draft, "draft");
    }
}

#[cfg(test)]
mod gui_colour_tests {
    use super::*;

    #[test]
    fn escapes_markup_metacharacters() {
        assert_eq!(escape_markup("a & b"), "a &amp; b");
        assert_eq!(escape_markup("<tag>"), "&lt;tag&gt;");
        assert_eq!(escape_markup("x < y > z & w"), "x &lt; y &gt; z &amp; w");
        assert_eq!(escape_markup("plain"), "plain");
    }

    #[test]
    fn model_markup_is_escaped_so_spans_cannot_inject() {
        // Model output containing angle brackets must render literally,
        // never become a Pango tag.
        let line = ConvLine { kind: ConvKind::Assistant, text: "<b>not bold</b> & <i>x</i>".into() };
        let m = line_markup(&line);
        assert!(m.contains("&lt;b&gt;not bold&lt;/b&gt;"));
        assert!(!m.contains("<b>"));
        assert!(m.contains("&amp;"));
    }

    #[test]
    fn kind_colours_match_the_tui_palette() {
        let (user_c, user_p) = ConvKind::User.style();
        assert_eq!(user_c, "#4ec9b0");
        assert_eq!(user_p, "❯ ");
        let (err_c, err_p) = ConvKind::Error.style();
        assert_eq!(err_c, "#f14c4c");
        assert_eq!(err_p, "✗ ");
        let (think_c, think_p) = ConvKind::Thinking.style();
        assert_eq!(think_c, "#c586c0");
        assert_eq!(think_p, "💭 ");
        let (sys_c, _) = ConvKind::System.style();
        assert_eq!(sys_c, "#569cd6");
    }

    #[test]
    fn line_markup_wraps_prefix_in_coloured_span() {
        let line = ConvLine { kind: ConvKind::User, text: "hello".into() };
        let m = line_markup(&line);
        assert!(m.starts_with("<span foreground=\"#4ec9b0\">"));
        assert!(m.contains("❯ </span>hello"));
    }

    #[test]
    fn status_markup_colours_by_state() {
        assert!(status_markup("running…").contains("#569cd6"));
        assert!(status_markup("queued: foo").contains("#569cd6"));
        assert!(status_markup("error: boom").contains("#f14c4c"));
        assert!(status_markup("idle").contains("#9e9e9e"));
        assert!(status_markup("12 in / 34 out tokens").contains("#9e9e9e"));
        // The text itself is preserved (escaped).
        assert!(status_markup("running <now>").contains("running &lt;now&gt;"));
    }

    #[test]
    fn push_splits_newlines_into_lines_and_clear_resets() {
        let mut s = GuiState::new();
        s.push(ConvKind::System, "one\ntwo");
        assert_eq!(s.conv.len(), 2);
        assert_eq!(s.conv[0].text, "one");
        assert_eq!(s.conv[1].text, "two");
        s.clear();
        assert!(s.conv.is_empty());
    }

    #[test]
    fn tab_completion_still_works_with_colour_model() {
        // The colour rework must not affect the completion paths.
        assert_eq!(complete_idle("/cle"), Some("/clear".to_string()));
        assert_eq!(complete_idle("/thinking hig"), Some("/thinking high".to_string()));
        assert_eq!(complete_idle("plain"), None);
    }
}

/// Drift guards. These exist because the GUI's completion list and its
/// `handle_command` arms are maintained by hand, and they had already drifted
/// apart: the old list offered `/fix`, `/bg`, `/jobs` and `/shell` while the
/// GUI implemented none of them (so Tab suggested a command that then answered
/// "unknown command"), and it omitted `/finished` entirely.
#[cfg(test)]
mod gui_parity_tests {
    use super::*;

    /// Every command the GUI's `handle_command` dispatches, read out of this
    /// file's own source so the check cannot itself go stale.
    ///
    /// Only top-level match arms are collected: the scan stops at the catch-all
    /// `other =>` arm, which is the last arm of the outer match, so the inner
    /// matches inside an arm (e.g. the `show`/`hide` toggle inside `thinking`)
    /// are never reached.
    fn handled_commands() -> Vec<String> {
        let src = include_str!("gui.rs");
        let body = src
            .split("fn handle_command(")
            .nth(1)
            .expect("handle_command not found in gui.rs")
            .split("\n        other => {")
            .next()
            .expect("handle_command body not found");
        let mut out = Vec::new();
        for line in body.lines() {
            let line = line.trim();
            // A top-level arm is written as `"a" | "b" | "c" => {` or
            // `"a" => {`, at 8 spaces of indentation.
            if !line.starts_with('"') || line.starts_with("            ") {
                continue;
            }
            // Collect every string alternative before the `=>`.
            let (alts, tail) = match line.find("=>") {
                Some(i) => (&line[..i], &line[i..]),
                None => continue,
            };
            let _ = tail;
            for part in alts.split('|') {
                let part = part.trim();
                if let Some(inner) = part.strip_prefix('"') {
                    if let Some(end) = inner.find('"') {
                        out.push(inner[..end].to_string());
                    }
                }
            }
        }
        out
    }

    #[test]
    fn every_completable_command_is_actually_handled() {
        let handled = handled_commands();
        for cmd in COMMANDS {
            assert!(
                handled.iter().any(|h| h == cmd),
                "Tab offers /{cmd} but gui.rs has no handle_command arm for it.\n\
                 Add the arm, or drop it from COMMANDS.\n\
                 GUI currently handles: {handled:?}"
            );
        }
    }

    #[test]
    fn every_completable_command_exists_in_the_global_table() {
        // `crate::commands::SLASH_COMMANDS` is generated by build.rs from the
        // streaming REPL's dispatch, so a name in it is a real command
        // somewhere in pir. A GUI-local name that is *not* there is either a
        // typo or a GUI-only command that should be documented as such.
        let global: Vec<&str> = crate::commands::SLASH_COMMANDS.to_vec();
        for cmd in COMMANDS {
            let with_slash = format!("/{cmd}");
            assert!(
                global.contains(&with_slash.as_str()),
                "/{cmd} is completed by the GUI but is not in the generated \
                 SLASH_COMMANDS table — it is not a real pir command"
            );
        }
    }

    #[test]
    fn thinking_completion_matches_the_levels_the_parser_accepts() {
        // `/thinking` must not offer a level `ThinkingLevel::parse` rejects,
        // nor omit one it accepts — that was a second, subtler drift.
        for arg in THINKING_ARGS {
            if matches!(*arg, "show" | "hide") {
                continue; // visibility toggles, not levels
            }
            assert!(
                crate::config::ThinkingLevel::parse(arg).is_some(),
                "/thinking offers '{arg}' but ThinkingLevel::parse rejects it"
            );
        }
        for lvl in ["off", "minimal", "low", "medium", "high", "xhigh", "max"] {
            assert!(
                THINKING_ARGS.contains(&lvl),
                "/thinking does not offer the '{lvl}' level"
            );
        }
    }

    #[test]
    fn gui_implements_every_tui_command_the_gui_advertises() {
        // The TUI's canonical command set, restricted to what a graphical
        // front-end can meaningfully offer. The GUI must handle each one; this
        // is the "GUI supports all TUI features" guard.
        const TUI_COMMANDS: &[&str] = &[
            "help", "model", "models", "goal", "continue", "clear", "undo",
            "finished", "fix", "shell", "bg", "jobs", "thinking", "cancel",
            "reload", "reexec", "exit",
        ];
        let handled = handled_commands();
        for cmd in TUI_COMMANDS {
            assert!(
                handled.iter().any(|h| h == cmd),
                "the TUI supports /{cmd} but the GUI does not — add a handle_command \
                 arm for it"
            );
        }
    }
}

/// The status/footer helpers. Pure string building, so they are testable
/// without a display.
#[cfg(test)]
mod gui_status_tests {
    use super::*;

    #[test]
    fn running_status_shows_a_spinner_and_queued_count() {
        let mut s = GuiState::new();
        s.spinner = 0;
        let st = running_status(&s, 0);
        assert!(st.starts_with(SPINNER_FRAMES[0]), "got {st:?}");
        assert!(st.contains("thinking"));
        // No queued prompts => no queued suffix.
        assert!(!st.contains("queued"));

        let st = running_status(&s, 3);
        assert!(st.contains("3 queued"), "got {st:?}");
    }

    #[test]
    fn spinner_frame_advances_and_wraps() {
        let s = GuiState::new();
        for i in 0..SPINNER_FRAMES.len() * 2 {
            let mut s2 = GuiState::new();
            s2.spinner = i;
            let expected = SPINNER_FRAMES[i % SPINNER_FRAMES.len()];
            assert!(running_status(&s2, 0).starts_with(expected));
        }
        // Never panics on an out-of-range frame.
        let mut s3 = s;
        s3.spinner = usize::MAX;
        let _ = running_status(&s3, 0);
    }

    #[test]
    fn idle_status_carries_workspace_and_model_like_the_tui_footer() {
        let mut s = GuiState::new();
        s.last_model = Some("opencode-go/muse-spark-1.2".into());
        let st = idle_status(&s);
        assert!(st.starts_with("workspace: "), "got {st:?}");
        assert!(st.contains("model: opencode-go/muse-spark-1.2"), "got {st:?}");
    }

    #[test]
    fn idle_status_omits_model_until_it_is_known() {
        let s = GuiState::new();
        let st = idle_status(&s);
        assert!(st.starts_with("workspace: "), "got {st:?}");
        assert!(!st.contains("model: "), "model must wait for the first drain: {st:?}");
    }
}

/// Scrollback bookkeeping. The original code kept this in a process-global
/// `static`, which desynced every tab from every other one; these pin the
/// per-session behaviour.
#[cfg(test)]
mod gui_render_cursor_tests {
    use super::*;

    #[test]
    fn clear_resets_the_write_cursor_and_flags_a_rebuild() {
        let mut s = GuiState::new();
        s.push(ConvKind::System, "one\ntwo\nthree");
        s.rendered = 3;
        s.needs_rebuild = false;
        s.clear();
        assert_eq!(s.rendered, 0);
        assert!(s.needs_rebuild, "clear must force a full buffer rebuild");
        assert!(s.conv.is_empty());
    }

    #[test]
    fn a_fresh_state_needs_a_rebuild() {
        let s = GuiState::new();
        assert!(s.needs_rebuild, "the first sync must paint the pane");
        assert_eq!(s.rendered, 0);
    }

    #[test]
    fn scrolling_back_keeps_the_cursor_inside_the_buffer() {
        // Push past the 4000-line cap and check the cursor can never point past
        // the end of the model, which is what makes an append write garbage.
        let mut s = GuiState::new();
        for i in 0..4200 {
            s.push(ConvKind::System, &format!("line {i}"));
        }
        assert_eq!(s.conv.len(), 4000, "the cap must hold");
        assert!(
            s.rendered <= s.conv.len(),
            "rendered {} > conv {} — an append would index out of range",
            s.rendered,
            s.conv.len()
        );
        assert!(s.needs_rebuild, "a trim must force a rebuild");

        // Simulate having rendered everything, then overflow again.
        s.rendered = s.conv.len();
        s.needs_rebuild = false;
        for i in 4200..4300 {
            s.push(ConvKind::System, &format!("line {i}"));
        }
        assert!(
            s.rendered <= s.conv.len(),
            "cursor {} escaped the trimmed model of {}",
            s.rendered,
            s.conv.len()
        );
    }

    #[test]
    fn each_session_keeps_its_own_cursor() {
        // The regression: two sessions, one clears, the other's cursor must be
        // untouched. With the old process-global statics this desynced.
        let mut a = GuiState::new();
        let mut b = GuiState::new();
        a.push(ConvKind::User, "a1");
        a.push(ConvKind::User, "a2");
        b.push(ConvKind::User, "b1");
        a.rendered = 2;
        b.rendered = 1;
        a.clear();
        assert_eq!(b.rendered, 1, "clearing session A must not touch B's cursor");
        assert_eq!(b.conv.len(), 1);
    }
}

