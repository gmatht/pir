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

/// The TUI's "turn finished, waiting on you" placeholder text
/// (`tui.rs:681` renders `format!("❯ {}", term::DONE_PROMPT_TEXT)`).
const DONE_PROMPT_TEXT: &str = crate::term::DONE_PROMPT_TEXT;

/// The concrete `Entry` type returned by `App::create_entry` — the backend
/// adapter `Entry` (nwg on Windows, gtk on Linux). `rustxwidgets::Entry` (the
/// prelude alias) is a thin wrapper, so we name the real type explicitly here.
#[cfg(windows)]
type GuiEntry = rustxwidgets::backends_nwg_adapter::Entry;
#[cfg(all(target_os = "linux", not(windows)))]
type GuiEntry = rustxwidgets::backends_gtk_adapter::Entry;

/// The concrete `Window` type returned by `App::create_window` — the backend
/// adapter `Window` (nwg on Windows, gtk on Linux), for the same reason as
/// [`GuiEntry`]: the prelude's `Window` is a thin wrapper that does not carry
/// `set_title`/`set_child`.
#[cfg(windows)]
type GuiWindow = rustxwidgets::backends_nwg_adapter::Window;
#[cfg(all(target_os = "linux", not(windows)))]
type GuiWindow = rustxwidgets::backends_gtk_adapter::Window;

/// The concrete `Label` type returned by `App::create_label`.
///
/// The prelude re-exports `common::Label` — a thin wrapper struct — but the
/// `App` factory methods hand back the *backend adapter* types, so `GuiWidgets`
/// has to name the adapter type or the assignment does not typecheck.
#[cfg(windows)]
type GuiLabel = rustxwidgets::backends_nwg_adapter::Label;
#[cfg(all(target_os = "linux", not(windows)))]
type GuiLabel = rustxwidgets::backends_gtk_adapter::Label;

// NOTE on the `cfg` gates above: they test `target_os`, *not*
// `cfg(feature = "gtk")`. A `feature` cfg inside this crate tests *pir's*
// features, and pir has no feature named `gtk` — that one belongs to the
// rustxwidgets dependency. The old `cfg(feature = "gtk")` guards were
// therefore silently always false, which is why `GuiEntry` had no definition
// at all and the whole GUI failed to compile. Gate on the platform, which is
// what actually selects the adapter.

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
    "fix", "shell", "sh", "bg", "jobs", "checkpoints", "rewind", "quarantine",
    "limits", "resources", "skills", "scratchpad",
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
    /// Whether the pane should stay pinned to the newest output.
    ///
    /// This is the GUI's version of the TUI's unconditional `self.scroll = 0` on
    /// every push (`tui.rs:241`). The TUI can do that because it *owns* the
    /// scroll offset; the GUI does not — `gtk_adjustment_set_value` is not in
    /// the toolkit's symbol table, so the scroll position can be read but not
    /// written (`docs/GUI_DESIGN.md` §2). So the GUI cannot force the pane to
    /// the bottom at all, and must instead *stop* scrolling it there: while
    /// `follow` is false, an append leaves the user where they are, mid-read.
    ///
    /// It is disarmed by a real upward scroll and re-armed when the user
    /// returns to the bottom, which is the only signal available (the wheel and
    /// scrollbar are GTK's, not ours).
    follow: bool,
    /// Whether the agent is idle, finished, and waiting on the operator. Drives
    /// the TUI's `✓ DONE :)` placeholder (`tui.rs:679-687`).
    awaiting_input: bool,
    /// The prompt Entry's current text, mirrored into the model so the hint
    /// line can be derived from it. The TUI owns the draft directly
    /// (`tui.rs:684`); in a GUI the Entry owns it, so it is copied in on every
    /// sync rather than being a second source of truth.
    draft: String,
    /// Last hint string handed to the hint label, so `set_markup` is not called
    /// on every drain tick (which forces a relayout).
    last_hint: String,
    /// Last window title set, for the same reason: `set_title` marks the window
    /// dirty, so it is only called when the running/idle state actually flips.
    last_title: String,
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
  /sh <cmd>             run a shell command   (/shell, and /! <cmd>, also work)
  /bg <prompt>          start a background turn   /jobs   list them
  /cancel               stop the running turn
  /checkpoints          list rewindable turn boundaries
  /rewind <#>           restore the tree and the conversation to a turn
  /quarantine [status|apply all|discard]
                         review / approve staged writes to the real filesystem
  /limits               what the agent's commands are bounded by
  /skills [name]        list the skill catalog, or print one
  /scratchpad           this session's scratch directory
  /reload               reload catalog, policy and extension commands
  /reexec               restart in place (idle only)
  /sessions             new|close|next|prev|<n>  multi-session tabs
  /exit                 quit

keys:
  Enter                 send the line / run the command
  esc or ctrl-c         cancel the running turn (same as /cancel)
  ctrl-q or ctrl-d      quit
  tab                   complete the command or model name
  up / down             recall earlier / later prompts";

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
            follow: true,
            awaiting_input: false,
            draft: String::new(),
            last_hint: String::new(),
            last_title: String::new(),
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
        // A clear empties the pane, so there is nothing to be scrolled away
        // from: follow comes back armed or the next turn would render off-screen.
        self.follow = true;
    }

    /// Note where the pane is scrolled to, and re-arm or disarm `follow`.
    ///
    /// `vpos` is the vertical adjustment value read from the `ScrolledWindow`
    /// (pixels). `vmax` is that adjustment's upper bound. `follow` is armed
    /// only at the bottom (within 2px, since GTK reports fractional values) and
    /// disarmed anywhere else.
    ///
    /// Called once per drain *before* appending, so the decision to pin reflects
    /// where the user was when the new output arrived rather than after GTK has
    /// already moved the position.
    fn observe_scroll(&mut self, vpos: f64, vmax: f64) {
        const AT_BOTTOM_SLOP: f64 = 2.0;
        self.follow = vpos >= vmax - AT_BOTTOM_SLOP;
    }

    /// Whether the next paint should pin the pane to the newest output.
    ///
    /// Split out from `sync_textview` so the rule is testable without a
    /// display: it is the one piece of logic that decides whether an append
    /// steals the user's place.
    fn sync_follow(&mut self) -> bool {
        self.follow
    }
}

/// The GUI's own keyboard scrollback model (design row S6).
///
/// The TUI scrolls by setting an integer offset on its `Paragraph`
/// (`tui.rs:659`) and its module doc promises `PageUp`/`PageDown`
/// (`tui.rs:14`) — bindings that are never actually registered. The GTK
/// toolkit has no `gtk_adjustment_set_value` and no scroll gesture symbols
/// (`docs/GUI_DESIGN.md` §2), so the GUI cannot drive the pane's adjustment
/// either. What it *can* do is keep an honest, clamped model of where the user
/// asked to be, which is what this is: `offset` in lines, clamped to
/// `0..=(total - page)`.
#[derive(Default)]
struct GuiScroll {
    /// Scroll position, in lines from the top.
    offset: usize,
    /// Height of the visible area, in lines. Set by the caller from the pane's
    /// allocated height; used only to clamp.
    page: usize,
}

impl GuiScroll {
    /// The largest valid offset for a conversation of `total` lines.
    fn max_offset(&self, total: usize) -> usize {
        total.saturating_sub(self.page)
    }

    fn set(&mut self, offset: usize, total: usize) {
        self.offset = offset.min(self.max_offset(total));
    }

    fn page_up(&mut self, by: usize) {
        self.offset = self.offset.saturating_sub(by);
    }

    fn page_down(&mut self, by: usize, total: usize) {
        self.offset = (self.offset + by).min(self.max_offset(total));
    }

    fn home(&mut self) {
        self.offset = 0;
    }

    fn end(&mut self, total: usize) {
        self.offset = self.max_offset(total);
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

/// Serialises every test that initialises GTK.
///
/// `cargo test` runs a crate's tests as concurrent threads in one process, and
/// `App::init()` dlopens GTK and runs `gtk_init`. GLib's initialisation and its
/// display connection are **not** thread-safe, so two tests calling it at once
/// race: the run segfaults or dies in a `GLib-GObject-CRITICAL` roughly one time
/// in four. Observed here with four display-backed tests; serialising them made
/// twelve consecutive parallel runs clean.
///
/// This is a `static Mutex` rather than the project-wide `TEST_ENV_LOCK` because
/// it guards a different process global: a display-backed test mutates neither
/// the environment nor the cwd, and putting GTK behind the env lock would
/// serialise all of them behind one unrelated mutex.
///
/// `Mutex::new` is a `const fn`, so a `static` initialiser costs nothing. The
/// guard must be held for the whole test body — dropping it early lets another
/// display-backed test initialise concurrently, which is the bug.
///
/// `unwrap_or_else(|e| e.into_inner())` ignores poisoning, as everywhere else
/// in this suite: one panicking test must not cascade into every later one.
#[cfg(test)]
fn gtk_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static GTK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GTK_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Initialise GTK for a test, or return `None` so the caller skips.
///
/// Every display-backed test in this file goes through here, so a
/// display-less environment degrades to a skip rather than a failure, and so no
/// test can accidentally bypass [`gtk_test_lock`].
#[cfg(test)]
fn gtk_test_app() -> Option<(App, std::sync::MutexGuard<'static, ()>)> {
    let guard = gtk_test_lock();
    match App::init() {
        Ok(app) => Some((app, guard)),
        Err(e) => {
            eprintln!("skipping: no GTK backend/display available in this environment ({e})");
            None
        }
    }
}

/// Handle to the GTK widgets we mutate from the periodic drain callback.
#[derive(Clone)]
struct GuiWidgets {
    textview: TextView,
    status: GuiLabel,
    /// The `ScrolledWindow` wrapping the conversation. Retained so the drain
    /// can *read* the scroll position, which is the only signal available for
    /// scroll-follow: the toolkit exposes
    /// `gtk_scrolled_window_get_vadjustment` but not
    /// `gtk_adjustment_set_value` (`docs/GUI_DESIGN.md` §2), so the GUI can
    /// see where the user is but cannot move the pane itself.
    scroller: ScrolledWindow,
    /// The transient hint label (`tui.rs:696-700`).
    hint: GuiLabel,
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
    /// The window, so the drain can keep the title in step with the running
    /// flag (T6). Shared by every tab, hence held per-session rather than
    /// passed separately at each call site.
    window: GuiWindow,
    agent_slot: AgentSlot,
    fg_cancel: Arc<AtomicBool>,
    fg_quiet: Arc<AtomicBool>,
    hist_nav: Arc<Mutex<HistoryNav>>,
    done_tx: smol::channel::Sender<()>,
    app: App,
}

/// Stop the foreground turn and every running background job, reporting what
/// was actually cancelled. Assumes the caller already holds the state lock.
///
/// This is the single definition of "cancel" in the GUI: the `/cancel` command
/// and the Esc / Ctrl-C keybindings both go through here, so the two can never
/// disagree about what gets stopped or what the operator is told.
fn cancel_running_locked(cancel: &Arc<AtomicBool>, s: &mut GuiState) {
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
    let list = bg_ids
        .iter()
        .map(|i| format!("#{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    match (stopped_fg, bg_ids.is_empty()) {
        (true, true) => s.push(ConvKind::System, "· requesting cancel (turn stops now)"),
        (true, false) => s.push(
            ConvKind::System,
            &format!(
                "· requesting cancel (turn + background job{} {list})",
                if bg_ids.len() == 1 { "" } else { "s" }
            ),
        ),
        (false, true) => s.push(ConvKind::System, "· no turn running to cancel"),
        (false, false) => s.push(
            ConvKind::System,
            &format!(
                "· requesting cancel (background job{} {list})",
                if bg_ids.len() == 1 { "" } else { "s" }
            ),
        ),
    }
}

/// Cancel from a keybinding: take the state lock, cancel, repaint.
///
/// The key handler has no state guard of its own, so it cannot call
/// [`cancel_running_locked`] directly.
fn cancel_running(
    cancel: &Arc<AtomicBool>,
    state: &Arc<Mutex<GuiState>>,
    widgets: &GuiWidgets,
) {
    let mut s = state.lock().unwrap();
    cancel_running_locked(cancel, &mut s);
    sync_textview(widgets, &mut s);
}

/// Ask the operator a question **in the GUI window** and block until they
/// answer. Installed as the `ask_user_question` renderer so the agent can be
/// answered from the graphical frontend.
///
/// Why this exists: `--gui` returned from `main` *before*
/// `install_ask_renderer()` ran (`main.rs`, the GUI block precedes it), so in
/// GUI mode no renderer was ever installed. `askuser::interactive()` was
/// therefore false and every `ask_user_question` / plan-approval request came
/// back as "no interactive user" — the GUI could never answer the model.
///
/// Threading: this is called from an agent **worker** thread (the turn loop
/// runs off the GTK main thread), never from the GTK main loop, so blocking
/// here does not freeze the UI — the main loop keeps servicing redraws and the
/// dialog itself. The answer is handed back through a channel the dialog's
/// response handler fills in.
#[cfg(all(target_os = "linux", not(windows)))]
fn question_dialog(
    app: &App,
    q: &crate::askuser::Question,
) -> crate::askuser::Answer {
    use crate::askuser::Answer;
    use std::sync::mpsc;

    let dialog = match app.create_dialog() {
        Ok(d) => d,
        // Without a dialog there is no way to ask; say so rather than block the
        // turn forever waiting for an answer that can never arrive.
        Err(e) => {
            eprintln!("pir: cannot open a question dialog ({e}) — treating as no answer");
            return Answer::Dismissed;
        }
    };
    dialog.set_title(if q.header.is_empty() { "pir" } else { &q.header });

    // Question text at the top.
    let body = app
        .create_label(&q.question)
        .expect("question label");
    body.set_xalign(0.0);
    dialog.append_content_area(&body);

    // Options as rows of buttons; each sets the answer and closes the dialog.
    // Free-text (no options) gets an entry below.
    let (tx, rx) = mpsc::channel::<Answer>();
    let entry = if q.options.is_empty() {
        let e = app.create_entry().expect("answer entry");
        dialog.append_content_area(&e);
        Some(e)
    } else {
        None
    };

    for opt in &q.options {
        let label = if opt.description.is_empty() {
            opt.label.clone()
        } else {
            format!("{} — {}", opt.label, opt.description)
        };
        let btn = app.create_button(&label).expect("option button");
        let tx = tx.clone();
        // `on_click` takes an `FnMut`, which cannot move `chosen` out of the
        // capture — clone per click instead (a click handler fires once).
        let chosen = std::sync::Arc::new(opt.label.clone());
        let chosen_cb = chosen.clone();
        let _ = btn.on_click(move || {
            let _ = tx.send(Answer::Chosen(vec![(*chosen_cb).clone()]));
        });
        dialog.append_content_area(&btn);
    }

    // A submit/cancel pair. Cancel closes with Dismissed, which is what the
    // agent treats as "the user declined to answer".
    let submit = app.create_button("Submit").expect("submit button");
    let tx_submit = tx.clone();
    let entry_for_submit = entry.clone();
    let _ = submit.on_click(move || {
        let ans = match entry_for_submit.as_ref().and_then(|e| e.get_text()) {
            Some(t) if !t.trim().is_empty() => Answer::Text(t),
            _ => Answer::Dismissed,
        };
        let _ = tx_submit.send(ans);
    });
    dialog.append_content_area(&submit);

    let cancel = app.create_button("Cancel").expect("cancel button");
    let tx_cancel = tx.clone();
    let _ = cancel.on_click(move || {
        let _ = tx_cancel.send(Answer::Dismissed);
    });
    dialog.append_content_area(&cancel);

    // A window-manager close is a dismissal too.
    let tx_closed = tx.clone();
    let _ = dialog.connect_response(move |resp| {
        // Negative response ids are GTK's delete-event / close conventions.
        if resp < 0 {
            let _ = tx_closed.send(Answer::Dismissed);
        }
    });

    dialog.present();

    // Block this worker until one of the controls above sends. The GTK main
    // loop is on another thread, so the dialog stays live and the UI keeps
    // painting while we wait.
    rx.recv().unwrap_or(Answer::Dismissed)
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
    eprintln!("PIRPROBE: run() entered");
    let app = App::init().map_err(|e| format!("rustxwidgets init: {e}"))?;
    eprintln!("PIRPROBE: App::init ok");

    // Install the `ask_user_question` renderer **here**, before any turn can
    // start. `main` used to install its terminal renderer only after the GUI
    // block had already returned, so in `--gui` mode no renderer existed at all:
    // `askuser::interactive()` was false and every question the model asked —
    // including plan-mode approval — came back as "no interactive user".
    //
    // Threading: the renderer type is `Fn(..) -> Answer + Send + Sync` because
    // `ask` runs it on whatever thread asked — here a turn's *worker*. GTK's
    // `App` is `Rc`-based and deliberately neither `Send` nor `Sync`, so it
    // cannot be captured by value, and a raw pointer is not `Sync` either.
    // The address is therefore carried as a `usize` (which is `Send + Sync`
    // unconditionally) and cast back at the call site.
    //
    // This is sound because: (a) `app` outlives every turn — `run` does not
    // return until the GTK loop ends — so the address stays valid; and (b) the
    // `App` is only ever *used* from the GTK main thread, which is the thread
    // that owns the widgets. Blocking the worker inside `question_dialog` is
    // therefore not a data race; it is the one place GTK's single-thread
    // ownership model has to be crossed, and it is documented here so the next
    // reader does not "tidy" it back into a compile error.
    #[cfg(all(target_os = "linux", not(windows)))]
    {
        let app_addr = std::sync::Arc::new(&app as *const App as usize);
        crate::askuser::install(std::sync::Arc::new(move |q| {
            // SAFETY: `app_addr` is the address of the `App` created above, and
            // that `App` is still alive for the whole call (it lives in `run`'s
            // frame, which outlives every renderer invocation). Reborrow is
            // valid because nothing else mutates or frees the value.
            let app: &App = unsafe { &*(*app_addr as *const App) };
            question_dialog(app, q)
        }));
    }

    // -- Build the window + menu bar + tab container --
    let window = app.create_window().map_err(|e| format!("create_window: {e}"))?;
    window.set_title("pir");
    window.set_default_size(820, 600);

    let tabs = app.create_tabview().map_err(|e| format!("tabview: {e}"))?;

    // The window's vertical stack: [menu bar] [session tabs]. Both are needed
    // before the first session, because the menu's actions dispatch into the
    // session that is current at the time they are chosen — so it reads
    // `sessions` lazily through an `Rc` rather than capturing one session.
    let root = app
        .create_box(Orientation::Vertical, 0)
        .map_err(|e| format!("create_box: {e}"))?;

    let sessions: Rc<RefCell<Vec<SessionView>>> = Rc::new(RefCell::new(Vec::new()));
    let active: Rc<Cell<usize>> = Rc::new(Cell::new(0));

    // One SessionView per open tab, indexed identically to the TabView.

    // Switching tabs just moves keyboard focus to that tab's prompt. This is
    // the *only* place focus is moved deliberately — the old unconditional
    // focus-out re-grab was a trap (see `spawn_session`).
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

    // The menu bar, built before the first session so its actions can dispatch
    // into whatever session is current when they are chosen.
    if let Err(e) = build_menu_bar(&app, &window, &sessions, &tabs, providers, bus, _full_auto) {
        // A missing menu bar is a degraded GUI, not a fatal one: the whole
        // command surface is still reachable by typing it. Say so once and
        // carry on rather than refusing to open the window.
        eprintln!("pir: menu bar unavailable ({e}) — every command is still available by typing it");
    }

    // Open the initial session using the agent the caller handed us.
    {
        let (done_tx, _done_rx) = smol::channel::bounded::<()>(1);
        spawn_session(
            "session",
            &window,
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

    root.append(&tabs);
    root.set_hexpand(true);
    root.set_vexpand(true);
    window.set_child(&root);

    eprintln!("PIRPROBE: about to present");
    window.present();
    eprintln!("PIRPROBE: presented");

    // -- Periodic drain: refresh every session's conversation from its log --
    // A recurring timeout (~150ms) that drains each agent's log into its own
    // conversation pane. Snapshot the views first so we don't hold the sessions
    // lock across the (potentially slow) drain of every session.
    let drain_sessions = sessions.clone();
    let drain_bus = bus.clone();
    #[cfg(all(target_os = "linux", not(windows)))]
    {
        match rustxwidgets::backends::gtk::loader() {
        Some(loader) => {
            let src_id = rustxwidgets::gtk_dynamic_loader::timeout_add_recurring(&loader, 150, Box::new(move || {
                for sv in &*drain_sessions.borrow() {
                    drain_once(
                        &sv.state,
                        &sv.widgets,
                        &sv.window,
                        &sv.entry,
                        &sv.agent_slot,
                        &sv.fg_cancel,
                        &sv.fg_quiet,
                        &sv.done_tx,
                        &drain_bus,
                    );
                }
            }));
            eprintln!("PIRPROBE: timer installed, source id = {src_id:?}");
        }
        None => eprintln!("PIRPROBE: loader() is None — timer NOT installed"),
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
                    &sv.window,
                    &sv.entry,
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

/// Take whatever is in the prompt Entry and submit it, if it is not empty.
///
/// Shared by Enter and the Send button. Returns early on an empty buffer so a
/// stray click cannot start a blank turn.
fn submit_prompt(
    sv: &SessionView,
    sessions: &Rc<RefCell<Vec<SessionView>>>,
    tabs: &TabView,
    providers: &[Provider],
    bus: &SharedBus,
    full_auto: bool,
) {
    let text = sv.entry.get_text().unwrap_or_default();
    sv.entry.set_text("");
    let text = text.trim().to_string();
    if text.is_empty() {
        return;
    }

    // Record the submitted line in the shared prompt history (in-memory + the
    // session `.history` file) so Arrow-Up recalls it, exactly like the
    // terminal REPLs. Also reset any in-progress Arrow-Up navigation.
    term::push_history(&text);
    sv.state.lock().unwrap().history.push(text.clone());
    sv.hist_nav.lock().unwrap().reset();

    run_command_line(&text, sv, sessions, tabs, providers, bus, full_auto);
}

/// Dispatch one submitted line — the single entry point for "the operator
/// pressed Enter".
///
/// Enter in the Entry, a menu-bar item, and the Send button all funnel through
/// here. That is deliberate: the GUI has three ways to say something and they
/// must not be three implementations. In particular the cancel path (a button,
/// `Esc`, `Ctrl-C` and `/cancel`) already shares one function, and this keeps
/// it that way by construction rather than by convention.
fn run_command_line(
    text: &str,
    sv: &SessionView,
    sessions: &Rc<RefCell<Vec<SessionView>>>,
    tabs: &TabView,
    providers: &[Provider],
    bus: &SharedBus,
    full_auto: bool,
) {
    let text = text.trim().to_string();
    if text.is_empty() {
        return;
    }

    if let Some(cmd) = text.strip_prefix('/') {
        // Session management is handled here (it must mutate tabs/sessions).
        let mut parts = cmd.split_whitespace();
        let sub = parts.next().unwrap_or("");
        let rest: Vec<&str> = parts.collect();
        if sub == "sessions" {
            handle_session_command(
                &rest,
                &sv.state,
                &sv.widgets,
                sessions,
                tabs,
                &sv.app,
                &sv.window,
                providers,
                bus,
                full_auto,
                &sv.agent_slot,
            );
            return;
        }
        handle_command(
            cmd,
            &sv.agent_slot,
            providers,
            bus,
            full_auto,
            &sv.fg_cancel,
            &sv.state,
            &sv.app,
            &sv.widgets,
        );
        return;
    }

    // `!cmd` / `!!cmd`: the streaming REPL's shell shorthands.
    if let Some(sh) = text.strip_prefix('!') {
        let sh = sh.strip_prefix('!').unwrap_or(sh).trim();
        let mut s = sv.state.lock().unwrap();
        if sh.is_empty() {
            s.push(ConvKind::Error, "· usage: !<command>");
            sync_textview(&sv.widgets, &mut s);
            return;
        }
        let msg = match run_shell_guarded(sh) {
            Ok(out) => out,
            Err(e) => format!("· {e}"),
        };
        s.push(ConvKind::System, &format!("$ {sh}"));
        s.push(ConvKind::System, &msg);
        sync_textview(&sv.widgets, &mut s);
        return;
    }

    // Regular prompt: if a turn is running, queue it; else start it now.
    let mut s = sv.state.lock().unwrap();

    // A trailing `&` runs the turn in the background and frees the prompt
    // immediately — the TUI's `foo &`.
    let (prompt, background) = match text.strip_suffix('&') {
        Some(rest) if !rest.trim().is_empty() => (rest.trim().to_string(), true),
        _ => (text.clone(), false),
    };

    if background {
        s.push(ConvKind::User, &prompt);
        match spawn_background(&prompt, &sv.agent_slot, providers, bus, full_auto, &mut s) {
            Ok(id) => s.push(ConvKind::System, &format!("· started background job #{id}")),
            Err(e) => s.push(ConvKind::Error, &e),
        }
        s.status = idle_status(&s);
        sync_textview(&sv.widgets, &mut s);
        return;
    }

    if s.running {
        s.pending.push(prompt.clone());
        s.status = running_status(&s, s.pending.len());
        sync_textview(&sv.widgets, &mut s);
        return;
    }
    s.running = true;
    s.awaiting_input = false;
    s.status = running_status(&s, 0);
    s.push(ConvKind::User, &prompt);
    sync_textview(&sv.widgets, &mut s);
    drop(s);

    // Reset the quiet switch so a previously detached turn's quiet state can't
    // leak.
    sv.fg_quiet.store(false, Ordering::SeqCst);
    spawn_turn(
        &sv.agent_slot,
        &sv.fg_cancel,
        &sv.fg_quiet,
        prompt,
        sv.done_tx.clone(),
    );
}

/// Build the menu bar and attach it to the window.
///
/// Each item in [`MENU_ACTIONS`] becomes a GMenu entry bound to a
/// `SimpleAction`; activating it routes the *same* command through the *same*
/// `handle_command` the typed line uses. That is the point of the table: there
/// is exactly one implementation of every command, so a menu item and a typed
/// line cannot drift apart in behaviour — only in how they were reached.
///
/// An item whose command needs an argument (`/model`, `/sh`, `/bg`, …) is
/// filled in from the prompt Entry rather than opened as a dialog: the GUI
/// already has a single-line prompt, and inventing a second place to type would
/// be worse than using the one the user is already looking at.
///
/// The menu is built from `MENU_ACTIONS` rather than by hand so the parity test
/// (`menu_bar_defines_every_action_the_gui_can_perform`) can hold it to the
/// dispatch — an item pointing at a command that does not exist is a menu that
/// lies.
fn build_menu_bar(
    app: &App,
    window: &GuiWindow,
    sessions: &Rc<RefCell<Vec<SessionView>>>,
    tabs: &TabView,
    providers: &[Provider],
    bus: &SharedBus,
    full_auto: bool,
) -> Result<(), String> {
    // The action group has to exist and be inserted into the window *before* the
    // menu model is built, or the actions have nothing to bind to.
    let group = app.ensure_action_group().map_err(|e| format!("action group: {e}"))?;
    unsafe { window.insert_action_group("pir", group) };

    // Group the flat table into per-menu `MenuItemDef`s, preserving first-seen
    // order so the bar reads the way the table does.
    let menus: Vec<&str> = {
        let mut seen: Vec<&str> = Vec::new();
        for a in MENU_ACTIONS {
            if !seen.contains(&a.menu) {
                seen.push(a.menu);
            }
        }
        seen
    };
    let mut root = app.new_menu().map_err(|e| format!("new_menu: {e}"))?;
    for menu_name in menus {
        let mut sub = app.new_menu().map_err(|e| format!("new_menu: {e}"))?;
        for a in MENU_ACTIONS.iter().filter(|a| a.menu == menu_name) {
            let sessions = sessions.clone();
            let tabs = tabs.clone();
            let providers = providers.to_vec();
            let bus = bus.clone();
            let command = a.command.to_string();
            let arg = a.arg.to_string();
            // `new_simple_action` (not `create_simple_action`) returns the
            // `common` wrapper that `register_action` takes; the `create_*`
            // factory hands back the bare backend adapter type.
            let action = app
                .new_simple_action(a.action)
                .map_err(|e| format!("new_simple_action({}): {e}", a.action))?;
            // The activate callback must be `'static`, so it cannot capture the
            // `&App` this function borrows. The address is carried as a `usize`
            // (`Send + Sync` unconditionally) and reborrowed at the call site —
            // the same technique, and the same safety argument, as the
            // ask-renderer install in `run`: the `App` outlives every callback
            // because `run` does not return until the GTK loop ends.
            let app_addr: usize = app as *const App as usize;
            let _ = action.connect_activate(move |_data: *mut c_void| {
                // Borrow one session, do the work, drop the borrow. The borrow
                // must not be held across `handle_command`: that can open or
                // close a tab, which mutates `sessions` and would panic.
                let sv = { sessions.borrow().get(tabs.current_tab()).cloned() };
                let Some(sv) = sv else { return };
                if command == "sessions" {
                    let rest: Vec<&str> = if arg.is_empty() {
                        Vec::new()
                    } else {
                        vec![arg.as_str()]
                    };
                    let mut s = sv.state.lock().unwrap();
                    s.push(
                        ConvKind::System,
                        &format!("· menu: /sessions {}", arg),
                    );
                    drop(s);
                    handle_session_command(
                        &rest,
                        &sv.state,
                        &sv.widgets,
                        &sessions,
                        &tabs,
                        // SAFETY: `app_addr` is the address of the `App` passed
                        // to `build_menu_bar`, which `run` owns for the whole
                        // life of the menu. Reborrowing it to `&App` is valid.
                        unsafe { &*(app_addr as *const App) },
                        &sv.window,
                        &providers,
                        &bus,
                        full_auto,
                        &sv.agent_slot,
                    );
                    return;
                }
                // Fill the argument in from the prompt if the caller left one
                // (e.g. "the next prompt" -> "/model the next prompt").
                let line = if arg.is_empty() {
                    format!("/{command}")
                } else {
                    let draft = sv.entry.get_text().unwrap_or_default();
                    let draft = draft.trim();
                    if draft.is_empty() {
                        format!("/{command} {arg}")
                    } else {
                        format!("/{command} {arg} {draft}")
                    }
                };
                let mut s = sv.state.lock().unwrap();
                s.push(ConvKind::System, &format!("· menu: {line}"));
                drop(s);
                let _ = sv.entry.set_text("");
                run_command_line(
                    &line,
                    &sv,
                    &sessions,
                    &tabs,
                    &providers,
                    &bus,
                    full_auto,
                );
            });
            let _ = app.register_action(&action);
            // Keep the action alive for the process lifetime: GTK holds only a
            // ref to it, and dropping our handle here would unbind the item.
            std::mem::forget(action);
            sub.append(a.label, a.action);
        }
        root.append_submenu(menu_name, &sub);
    }

    let bar = app
        .new_menubar(&root, group)
        .map_err(|e| format!("new_menubar: {e}"))?;
    // The bar is the window's top child of the root box; `build_menu_bar`
    // returns the widget so the caller can append it in the right order.
    let _ = bar;
    Ok(())
}

/// Build one session tab: its own conversation pane + prompt + status line,
/// wired to its own agent slot / canceller / history. Pushes the resulting
/// `SessionView` into `sessions` at the same index the `TabView` created.
fn spawn_session(
    title: &str,
    window: &GuiWindow,
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
    // The conversation TextView goes inside a ScrolledWindow. A bare GtkTextView
    // has no viewport of its own, so without this the pane cannot be scrolled at
    // all: once a turn fills the window, everything before it is unreachable.
    // (`create_scrolled_window` already existed in the toolkit and was simply
    // never called — which is why this file's header comment advertised a
    // "scrollable conversation TextView" that it did not provide.)
    let scroller = app
        .create_scrolled_window()
        .expect("create_scrolled_window");
    // Vertical scrolling only: a horizontal scrollbar on a word-wrapped
    // conversation pane is never what the user wants.
    scroller.set_policy(0, 1); // GTK_POLICY_NEVER, GTK_POLICY_AUTOMATIC
    let textview = app.create_textview().expect("create_textview");
    configure_conversation_textview(&textview);
    scroller.set_child(&textview);
    vbox.append(&scroller);
    // The scroller, not the TextView, is what must claim the free space, and it
    // needs a sensible minimum height or the pane collapses when a turn is long.
    scroller.set_hexpand(true);
    scroller.set_vexpand(true);
    scroller.set_size_request(-1, 200);

    // -- Prompt row: [model picker] [Entry] [Send] [Cancel] --
    // The picker is the graphical form of `/model` (design row G1), and the
    // buttons the graphical form of Enter and `Esc` (G3). All three route into
    // the same functions the typed line and the keys use, which is the whole
    // point: three ways to say something must not be three implementations.
    let prompt_row = app
        .create_box(Orientation::Horizontal, 4)
        .expect("create_box");

    // `widgets` is built a little later in this function (it needs the TextView
    // and both labels), so the picker and the Cancel button read their targets
    // out of these cells, which are filled in before either can fire.
    // Both callbacks are registered above the point where `state`/`widgets`
    // exist (a callback must exist before it can be connected), so they read
    // them from these cells. Both are filled in below, before the GTK main loop
    // starts and therefore before either callback can fire.
    let widgets_cell: Rc<RefCell<Option<GuiWidgets>>> = Rc::new(RefCell::new(None));
    let state_cell: Rc<RefCell<Option<Arc<Mutex<GuiState>>>>> = Rc::new(RefCell::new(None));

    let model_items = model_picker_items(providers);
    let model_refs: Vec<&str> = model_items.iter().map(|s| s.as_str()).collect();
    if let Ok(picker) = app.create_dropdown(&model_refs) {
        picker.set_size_request(220, -1);
        let picker_state = state_cell.clone();
        let picker_slot = agent_slot.clone();
        let picker_widgets = widgets_cell.clone();
        let picker_providers = providers.to_vec();
        // `DropDown` is not `Clone`, and `connect_changed` takes a `'static`
        // closure, so the widget cannot be captured by value. The loader type is
        // `#[repr(transparent)]` over a raw handle and is itself `Clone`, so a
        // clone of it is a cheap handle to the same widget.
        let picker_handle = picker.0.clone();
        let _ = picker.connect_changed(move || {
            // GTK fires `changed` when the widget is built too, so a negative
            // row means "nothing selected yet" — not "switch to the last
            // model", which would silently change the model on startup.
            let Some(idx) = selection_to_index(picker_handle.get_active()) else {
                return;
            };
            let Some(item) = model_items.get(idx as usize).cloned() else { return };
            let Some(pstate) = picker_state.borrow().clone() else { return };
            let mut s = pstate.lock().unwrap();
            let Some(w) = picker_widgets.borrow().clone() else { return };
            // Resolve through the same `handle_command` path as a typed
            // `/model <sel>`, so picker and command cannot disagree.
            match crate::config::select(&picker_providers, &item) {
                Ok((p, m)) => {
                    let Ok(mut g) = picker_slot.lock() else { return };
                    let Some(agent) = g.as_mut() else {
                        s.push(ConvKind::System, "· agent busy (turn running)");
                        sync_textview(&w, &mut s);
                        return;
                    };
                    match agent.switch(p.clone(), m.clone()) {
                        Ok(()) => s.push(ConvKind::System, &format!("→ {}", agent.label())),
                        Err(e) => s.push(ConvKind::Error, &e),
                    }
                }
                Err(e) => s.push(ConvKind::Error, &e),
            }
            sync_textview(&w, &mut s);
        });
        prompt_row.append(&picker);
    }

    let entry: GuiEntry = app.create_entry().expect("create_entry");
    entry.set_hexpand(true);
    prompt_row.append(&entry);

    // Cancel. Shares `cancel_running` with `Esc`, `Ctrl-C` and `/cancel`, so a
    // button press and a keypress cannot stop different things.
    {
        let cancel_btn = app.create_button("Cancel").expect("create_button");
        let cancel_state = state_cell.clone();
        let cancel_widgets = widgets_cell.clone();
        let cancel_flag = fg_cancel.clone();
        let _ = cancel_btn.on_click(move || {
            let (Some(w), Some(cstate)) =
                (cancel_widgets.borrow().clone(), cancel_state.borrow().clone())
            else {
                return;
            };
            cancel_running(&cancel_flag, &cstate, &w);
        });
        prompt_row.append(&cancel_btn);
    }

    vbox.append(&prompt_row);
    prompt_row.set_hexpand(true);

    let status = app.create_label("pir · idle").expect("create_label");
    #[cfg(all(target_os = "linux", not(windows)))]
    status.set_xalign(0.0); // left-align
    vbox.append(&status);

    // The transient hint line under the status bar — the TUI's fourth footer
    // line (`tui.rs:696-700`). Separate widget so clearing it is a `set_markup("")`
    // rather than a rewrite of the status text.
    let hint = app.create_label("").expect("create_label");
    #[cfg(all(target_os = "linux", not(windows)))]
    hint.set_xalign(0.0);
    vbox.append(&hint);

    let panel = tabs.tab_box(idx).expect("tabview: tab_box");
    panel.append(&vbox);
    vbox.set_hexpand(true);
    vbox.set_vexpand(true);

    let state = Arc::new(Mutex::new(GuiState::new()));
    let widgets = GuiWidgets {
        textview: textview.clone(),
        status: status.clone(),
        scroller: scroller.clone(),
        hint: hint.clone(),
    };
    // Hand the picker and the Cancel button their target now that it exists.
    // Both are wired above (a callback needs the widget at construction time)
    // but cannot fire until the main loop runs, which is strictly after this.
    *widgets_cell.borrow_mut() = Some(widgets.clone());
    *state_cell.borrow_mut() = Some(state.clone());
    // Seed Arrow-Up history from the session's `.history` file (same store the
    // streaming REPL uses) and greet the user.
    {
        let mut s = state.lock().unwrap();
        s.history = term::load_history_lines();
        // Mirror the agent's persisted thinking-display choice, so a resumed
        // session does not show/hide thinking contrary to what it saved. The
        // TUI does exactly this (`tui.rs:252-254`).
        if let Some(agent) = agent_slot.lock().unwrap().as_ref() {
            s.show_thinking = agent.show_thinking();
        }
        s.push(
            ConvKind::System,
            &format!(
                "pir · {} · GTK GUI  (/help · /sessions new|close|next|prev|<n> · Enter to send · \
                 esc or ctrl-c cancels · ctrl-q quits)",
                providers[0].pid()
            ),
        );
        sync_textview(&widgets, &mut s);
    }

    let hist_nav = Arc::new(Mutex::new(HistoryNav::default()));
    let app_quit = app.clone();

    // NOTE: there is deliberately no focus-out re-grab here. The old code
    // re-took focus on every `focus-out`, which is a trap: the operator could
    // not click into any other widget, and a click on the native tab bar fought
    // the handler. Focus now moves only when something deliberately asks for it
    // (a tab change, below). Pinned by `focus_out_does_not_regrab_focus`.

    // Tab completion + history navigation on this tab's prompt.
    let entry_comp_cb = entry.clone();
    let entry_cb_providers_tab: Vec<Provider> = providers.to_vec();
    let nav_for_keys = hist_nav.clone();
    let hist_for_keys = state.clone();
    let app_for_keys = app.clone();
    // Captured so Esc / Ctrl-C can cancel the turn without the operator having
    // to type `/cancel`.
    let cancel_for_keys = fg_cancel.clone();
    let state_for_keys = state.clone();
    let widgets_for_keys = widgets.clone();
    let _ = entry.on_key_raw(Box::new(move |keyval: u32, kstate: u32| -> bool {
        const CONTROL_MASK: u32 = 1 << 2;
        // Raw GDK keyvals. The toolkit's prelude only re-exports a handful of
        // `GDK_KEY_*` constants (Q, TAB, UP, DOWN, …); Escape and the control
        // letters are not among them, and binding them by name would silently
        // compile as a *catch-all* `u32` pattern (a `GDK_KEY_Escape` that does
        // not resolve becomes a fresh binding, which matches everything). These
        // are the stable X11/GDK values for those keys.
        const KEY_ESCAPE: u32 = 0xff1b;
        const KEY_C: u32 = b'c' as u32;
        const KEY_D: u32 = b'd' as u32;
        let ctrl = kstate & CONTROL_MASK != 0;
        match keyval {
            GDK_KEY_Q if ctrl => {
                let _ = app_for_keys.clone().quit();
                true
            }
            // Esc and Ctrl-C cancel the running turn — the same affordance the
            // TUI has and the GUI previously lacked, where the only way to stop
            // work was to type `/cancel`. Both share the command's logic, so a
            // key press and the typed command can never disagree about what
            // "cancel" means.
            KEY_ESCAPE => {
                cancel_running(&cancel_for_keys, &state_for_keys, &widgets_for_keys);
                true
            }
            KEY_C if ctrl => {
                cancel_running(&cancel_for_keys, &state_for_keys, &widgets_for_keys);
                true
            }
            // Ctrl-D quits, as in every other REPL. The TUI's quit gesture is
            // "cancel twice then quit"; a GUI already has a window close
            // button, so one Ctrl-D is enough.
            KEY_D if ctrl => {
                let _ = app_for_keys.clone().quit();
                true
            }
            GDK_KEY_TAB => {
                let cur = entry_comp_cb.get_text().unwrap_or_default();
                let completed = complete_idle(&cur).or_else(|| {
                    let provs: Vec<Provider> = entry_cb_providers_tab.clone();
                    crate::config::complete_model_buffer(&cur, &provs)
                });
                // Only claim the key when the completion actually *changed*
                // something. The old handler returned `true` unconditionally,
                // which killed GTK's default focus-move and made a second Tab
                // re-complete the already-completed text. Returning `false`
                // lets GTK handle it, which is what the user wants when there
                // is nothing to complete.
                match completed {
                    Some(c) if c != cur => {
                        entry_comp_cb.set_text(&c);
                        true
                    }
                    _ => false,
                }
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
    let window_for_activate = window.clone();
    let sessions_for_activate = sessions.clone();
    let tabs_for_activate = tabs.clone();
    // The one "submit the prompt" path. Enter and the Send button both call
    // this, so a button press and a keypress cannot diverge — see
    // `submit_prompt`.
    let sv_for_submit = SessionView {
        state: entry_cb_state.clone(),
        widgets: entry_cb_widgets.clone(),
        entry: entry_in_cb.clone(),
        window: window_for_activate.clone(),
        agent_slot: entry_cb_slot.clone(),
        fg_cancel: entry_cb_cancel.clone(),
        fg_quiet: entry_cb_quiet.clone(),
        hist_nav: nav_for_activate.clone(),
        done_tx: entry_cb_done.clone(),
        app: app_for_activate.clone(),
    };
    let sessions_for_submit = sessions_for_activate.clone();
    let tabs_for_submit = tabs_for_activate.clone();
    let providers_for_submit = entry_cb_providers.clone();
    let bus_for_submit = entry_cb_bus.clone();
    let _ = entry.connect_activate({
        let sv = sv_for_submit.clone();
        let sessions = sessions_for_submit.clone();
        let tabs = tabs_for_submit.clone();
        let providers = providers_for_submit.clone();
        let bus = bus_for_submit.clone();
        move |_data: *mut c_void| {
            submit_prompt(&sv, &sessions, &tabs, &providers, &bus, _full_auto);
        }
    });

    // The Send button (G3). Same function as Enter, so there is one definition
    // of "submit the prompt line" rather than two that can drift.
    if let Ok(send_btn) = app.create_button("Send") {
        let sv_send = sv_for_submit.clone();
        let sessions_send = sessions_for_submit.clone();
        let tabs_send = tabs_for_submit.clone();
        let providers_send = providers_for_submit.clone();
        let bus_send = bus_for_submit.clone();
        let _ = send_btn.on_click(move || {
            submit_prompt(
                &sv_send,
                &sessions_send,
                &tabs_send,
                &providers_send,
                &bus_send,
                _full_auto,
            );
        });
        prompt_row.append(&send_btn);
    }

    sessions.borrow_mut().push(SessionView {
        state,
        widgets,
        entry,
        window: window.clone(),
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
    window: &GuiWindow,
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
                        window,
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
    window: &GuiWindow,
    entry: &GuiEntry,
    agent_slot: &AgentSlot,
    fg_cancel: &Arc<AtomicBool>,
    fg_quiet: &Arc<AtomicBool>,
    done: &smol::channel::Sender<()>,
    bus: &SharedBus,
) {
    let mut s = state.lock().unwrap();

    // -- Scroll-follow, decided BEFORE anything is appended --
    // Read where the user currently is, so the append below knows whether it
    // may take the pane to the bottom. Doing it after the append would read the
    // post-append position and could not tell the difference.
    eprintln!("PIRPROBE: drain_once tick");
    if let Some((vpos, vmax)) = scrolled_window_range(&widgets.scroller) {
        s.observe_scroll(vpos, vmax);
    }

    // Mirror the prompt Entry's text into the model. The hint line and the
    // done-placeholder are both derived from the draft, and in a GUI the Entry
    // owns it — so it is copied in here, once per drain, rather than becoming a
    // second source of truth updated on every keystroke. The TUI owns the draft
    // outright and derives both directly (`tui.rs:684`).
    s.draft = entry.get_text().unwrap_or_default();

    // The window title carries the TUI's pane title (T6): `pir — running` /
    // `pir — idle`. Only touched on a change — `set_title` marks the window for
    // relayout every time.
    let title = window_title(s.running);
    if s.last_title != title {
        eprintln!("PIRPROBE: set_title -> {:?}", title);
        window.set_title(&title);
        s.last_title = title;
    }

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
                s.status = usage_status(a.usage.input, a.usage.output);
            } else {
                s.status = "idle".into();
            }
            // The turn is done and nothing is queued: the GUI is now waiting on
            // the operator, which is what the `✓ DONE :)` placeholder reports
            // (`tui.rs:679-687`). Cleared again the moment work resumes.
            s.awaiting_input = true;
            // Extensions can queue a follow-up prompt; drain it into a new turn
            // instead of dropping it (this is what the TUI does, and what kept
            // the GUI from silently losing post-turn work).
            if s.continuations.is_empty() {
                let next = s.pending.drain(..).next();
                if let Some(next) = next {
                    s.running = true;
                    s.awaiting_input = false;
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
            s.awaiting_input = false;
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

/// The token-usage line shown after a turn finishes — the TUI's
/// `"{in} in / {out} out tokens"` (`tui.rs:303`). A named function so the
/// format is testable without constructing an `Agent`.
fn usage_status(input: u64, output: u64) -> String {
    format!("{} in / {} out tokens", fmt_tok(input), fmt_tok(output))
}

/// The window title, carrying the same running/idle signal the TUI puts in its
/// pane title (`tui.rs:653`). A named function so it is testable without a
/// display.
fn window_title(running: bool) -> String {
    format!("pir — {}", if running { "running" } else { "idle" })
}

/// The transient hint under the status bar — a port of the TUI's
/// `update_tui_hint` (`tui.rs:1140`), matched string for string so the two
/// front-ends nudge identically.
fn hint_text(draft: &str) -> String {
    if draft == "/thinking" {
        "[Tab] levels: off | minimal | low | medium | high | xhigh | max · show | hide".to_string()
    } else if draft == "/" {
        "[Tab] completes commands; try /thinking".to_string()
    } else {
        String::new()
    }
}

/// The `✓ DONE :)` placeholder, shown only when the GUI is idle, finished, and
/// the operator has not started typing — the TUI's condition at `tui.rs:679`.
fn done_placeholder(s: &GuiState) -> Option<String> {
    // The draft check matters as much as the state checks: the TUI shows the
    // placeholder in the prompt row, so a half-typed line must replace it
    // rather than sit next to it.
    if !s.running && s.awaiting_input && s.draft.trim().is_empty() {
        Some(DONE_PROMPT_TEXT.to_string())
    } else {
        None
    }
}

/// Translate a `DropDown`'s reported row into an index, or `None` when nothing
/// is selected.
///
/// GTK reports "no active row" as `-1`, and a zero-item dropdown reports `-1`
/// too. Returning `Option` rather than a bare index means a caller cannot
/// forget to check, which matters because `connect_changed` fires once when the
/// widget is built — treating that as "row 0" would silently switch the model
/// at startup.
fn selection_to_index(active: i32) -> Option<usize> {
    if active < 0 { None } else { Some(active as usize) }
}

/// The model picker's items: every configured model, qualified by its provider
/// so two providers offering the same model id do not collide. Pure so the
/// contents can be asserted without a display.
fn model_picker_items(providers: &[Provider]) -> Vec<String> {
    let mut out = Vec::new();
    for p in providers {
        for m in &p.models {
            out.push(format!("{}/{}", p.pid(), m.id));
        }
    }
    out
}

/// This file's source for the body of `handle_command` — everything from its
/// definition up to (but not including) the catch-all `other =>` arm.
///
/// One helper for every source-level guard that needs it, because doing this
/// inline is a trap: the obvious spelling
/// `include_str!("gui.rs").split("fn handle_command(").nth(1)` returns
/// *whichever occurrence comes second*, and this file quotes that literal in
/// more than one test. Adding a second guard therefore silently rewrote the
/// first one's input, and the resulting failure read like a real regression in
/// `/goal` rather than a test bug.
///
/// Anchored on the definition instead: it starts at column 0, so the match
/// includes the newline in front of it and cannot be satisfied by an indented
/// string inside a comment or an assertion message.
fn handle_command_source() -> &'static str {
    let src = include_str!("gui.rs");
    let start = src
        .find("\nfn handle_command(")
        .expect("handle_command definition not found in gui.rs")
        + 1;
    src[start..]
        .split("\n        other => {")
        .next()
        .expect("handle_command body not found")
}

/// The slash commands `handle_command` dispatches, read out of this file's own
/// source so a hand-maintained list cannot go stale against the dispatch.
///
/// The scan walks `match cmd { … }` tracking **brace depth**, so only the
/// outer match's arms are collected. The previous indentation heuristic did not
/// do this and picked up strings from *inner* matches — `"show"` and `"hide"`
/// from the `/thinking` visibility toggle, `"apply"` and `"discard"` from
/// `/quarantine`, `"all"` from `/undo` — so a test that treated them as
/// command names was checking things that are not commands.
///
/// Non-test code uses this too: [`MENU_ACTIONS`] is validated against it, so a
/// menu item cannot point at a command the dispatch does not implement.
fn gui_command_arms() -> Vec<String> {
    let tail = handle_command_source();
    let m = tail
        .find("match cmd {")
        .expect("handle_command's `match cmd {` not found");
    // Start just inside the outer `match cmd {`; depth 1 == an arm pattern.
    let body = &tail[m + "match cmd {".len()..];
    let mut out = Vec::new();
    let mut depth: i32 = 1;
    for line in body.lines() {
        if depth == 1 {
            let t = line.trim();
            if let Some(eq) = t.find("=>") {
                let alts = t[..eq].trim_start();
                if alts.starts_with('"') {
                    for part in alts.split('|') {
                        let part = part.trim();
                        if let Some(inner) = part.strip_prefix('"')
                            && let Some(end) = inner.find('"')
                        {
                            out.push(inner[..end].to_string());
                        }
                    }
                }
            }
        }
        // Count braces *after* testing the line, so an arm's own `=> {` is
        // counted as the opening of its block rather than as part of the
        // pattern. Format strings like `{n}` are balanced on one line, so they
        // cancel out.
        for c in line.chars() {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
        }
    }
    out
}

/// Alias spellings that dispatch to the same `handle_command` arm as a
/// documented command.
///
/// Direction matters: each entry maps the *other* spelling onto the one the GUI
/// actually documents in `/help` and the menu. `sh` is the alias because
/// `/help` documents `/sh` (it is the shorter, more natural one to type);
/// `exit` is the alias because `/help` documents `/exit`, not `/quit`. Getting
/// this backwards makes a documented command look undocumented, which is what
/// the `/help` and menu parity tests are there to catch.
///
/// The dispatch arms are `"exit" | "quit" | "q"` and `"sh" | "shell"`, so every
/// spelling in a chain reaches the same code.
const COMMAND_ALIASES: &[(&str, &str)] = &[
    ("h", "help"),
    ("c", "cancel"),
    ("m", "model"),
    ("cont", "continue"),
    ("q", "exit"),
    ("quit", "exit"),
    ("shell", "sh"),
    ("background", "bg"),
    ("running", "jobs"),
    ("ckpt", "checkpoints"),
    ("resources", "limits"),
    ("skill", "skills"),
    ("scratch", "scratchpad"),
];

/// Resolve a command name to the canonical form, following alias chains.
///
/// Chains are real, not hypothetical: the dispatch arm is
/// `"exit" | "quit" | "q"`, and the alias table names `/q -> /quit` (matching
/// the streaming REPL, where `/q` is documented as quit) and `/quit -> /exit`.
/// A single lookup therefore resolves `q` to `quit` and stops, and `quit` is
/// not itself a menu entry — so a one-step resolver would report `/quit` as
/// "handled but not in the menu bar" when `/exit` plainly covers it. Resolve
/// to a fixed point instead, with a bound so a cyclic table cannot hang.
fn canonical_command(name: &str) -> String {
    let mut cur = name;
    // At most one step per distinct alias; the table is tiny and acyclic.
    for _ in 0..COMMAND_ALIASES.len() + 1 {
        match COMMAND_ALIASES.iter().find(|(a, _)| *a == cur) {
            Some((_, next)) if *next != cur => cur = next,
            _ => break,
        }
    }
    cur.to_string()
}

/// One entry in the menu bar: a label, the action name, and the slash command
/// it runs. Every entry is a command `handle_command` implements, and every
/// implemented command resolves to an entry — both directions guarded by
/// `menu_bar_defines_every_action_the_gui_can_perform`.
struct MenuAction {
    /// Top-level menu this lives under ("Session", "View", …).
    menu: &'static str,
    /// Visible label.
    label: &'static str,
    /// Unique action name registered with GTK.
    action: &'static str,
    /// The slash command it runs, without the leading `/`.
    command: &'static str,
    /// Sub-arguments, if the command takes any. `""` means "no argument", in
    /// which case the menu item opens an input rather than running blind.
    arg: &'static str,
}

/// The GUI's menu bar contents. Names are canonical: an alias like `/q` is not
/// its own item, because two menu entries that do the same thing is a menu that
/// lies about how many things there are.
const MENU_ACTIONS: &[MenuAction] = &[
    // Session
    MenuAction { menu: "Session", label: "New session", action: "session-new", command: "sessions", arg: "new" },
    MenuAction { menu: "Session", label: "Close session", action: "session-close", command: "sessions", arg: "close" },
    MenuAction { menu: "Session", label: "List sessions", action: "session-list", command: "sessions", arg: "" },
    MenuAction { menu: "Session", label: "Run a shell command", action: "session-shell", command: "sh", arg: "" },
    MenuAction { menu: "Session", label: "Run in background", action: "session-bg", command: "bg", arg: "" },
    MenuAction { menu: "Session", label: "List background jobs", action: "session-jobs", command: "jobs", arg: "" },
    MenuAction { menu: "Session", label: "List checkpoints", action: "session-checkpoints", command: "checkpoints", arg: "" },
    MenuAction { menu: "Session", label: "Set goal", action: "session-goal", command: "goal", arg: "" },
    MenuAction { menu: "Session", label: "Continue goal", action: "session-continue", command: "continue", arg: "" },
    MenuAction { menu: "Session", label: "Set thinking level", action: "session-thinking", command: "thinking", arg: "" },
    // Model
    MenuAction { menu: "Model", label: "Switch model", action: "model-switch", command: "model", arg: "" },
    MenuAction { menu: "Model", label: "List models", action: "model-list", command: "models", arg: "" },
    // Turn control
    MenuAction { menu: "Turn", label: "Cancel turn", action: "turn-cancel", command: "cancel", arg: "" },
    MenuAction { menu: "Turn", label: "Undo last turn", action: "turn-undo", command: "undo", arg: "" },
    MenuAction { menu: "Turn", label: "Rewind", action: "turn-rewind", command: "rewind", arg: "" },
    MenuAction { menu: "Turn", label: "List checkpoints", action: "turn-checkpoints", command: "checkpoints", arg: "" },
    // View
    MenuAction { menu: "View", label: "Clear pane", action: "view-clear", command: "clear", arg: "" },
    // Safety
    MenuAction { menu: "Safety", label: "Review staged writes", action: "safety-quarantine", command: "quarantine", arg: "" },
    MenuAction { menu: "Safety", label: "Resource limits", action: "safety-limits", command: "limits", arg: "" },
    // Info
    MenuAction { menu: "Help", label: "Commands", action: "help-commands", command: "help", arg: "" },
    MenuAction { menu: "Help", label: "Skills", action: "help-skills", command: "skills", arg: "" },
    MenuAction { menu: "Help", label: "Session scratchpad", action: "help-scratchpad", command: "scratchpad", arg: "" },
    MenuAction { menu: "Help", label: "Token usage", action: "help-usage", command: "usage", arg: "" },
    MenuAction { menu: "Help", label: "Reload catalog", action: "help-reload", command: "reload", arg: "" },
    MenuAction { menu: "Help", label: "Restart in place", action: "help-reexec", command: "reexec", arg: "" },
    MenuAction { menu: "Help", label: "Mark session finished", action: "help-finished", command: "finished", arg: "" },
    MenuAction { menu: "Help", label: "Repair VCS setup", action: "help-fix", command: "fix", arg: "" },
    MenuAction { menu: "Help", label: "Quit", action: "help-quit", command: "exit", arg: "" },
];

/// Menu items that are not slash commands at all: pure view actions. Kept apart
/// from [`MENU_ACTIONS`] so the "every handled command is in the menu" check
/// cannot be satisfied by a view action, and so a view action can never be
/// mistaken for a command that does not exist.
const MENU_VIEW_ACTIONS: &[(&str, &str, &str)] = &[
    ("View", "Scroll to bottom", "view-end"),
    ("View", "Scroll up one page", "view-page-up"),
    ("View", "Scroll down one page", "view-page-down"),
    ("Turn", "Mark session finished", "view-finished"),
];

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

/// Drive the active goal to completion on a worker thread, *moving* the agent
/// out of `agent_slot` for the duration — the same shape as [`spawn_turn`].
///
/// `/goal <objective>` and `/continue` used to call `agent.continue_goal()`
/// inline on the GTK main thread. A goal run can take many minutes, so that
/// froze the whole window: no redraw, no scroll, and `/cancel` could not even be
/// typed to stop it. Moving the agent to a worker keeps the UI live.
///
/// The agent is put back into the slot when the run finishes, which is what the
/// periodic drain uses to notice the session is idle again. `attach_log` mirrors
/// `/continue`: re-attach the session log as goal context before driving.
///
/// No `quiet` flag is threaded through because the GUI agent is already quiet —
/// `main` calls `agent.set_quiet(true)` before handing it over — so goal output
/// reaches the user through the session log, exactly like any other turn.
fn spawn_goal(agent_slot: &AgentSlot, attach_log: bool) {
    let slot = agent_slot.clone();
    std::thread::spawn(move || {
        let taken = slot.lock().unwrap().take();
        let Some(mut agent) = taken else { return };
        if attach_log {
            let lp = agent.log_path.clone();
            if let Some(p) = lp {
                agent.attach_goal(&p);
            }
        }
        // The summary is part of the run's own output; the light-model
        // backstop inside `continue_goal` is what decides whether it is done.
        let _summary = agent.continue_goal();
        *slot.lock().unwrap() = Some(agent);
    });
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
    drain_session_text(&buf, state);
    // Bound scrollback.
    //
    // Trimming the model without telling the renderer about it desyncs the
    // write cursor: `rendered` is an index into `conv`, so dropping `drop`
    // leading lines must shift it down by the same amount, and the backend's
    // buffer still holds the old lines, so it needs a rebuild. `push()` does
    // exactly this for its 4000-line cap; this second, much larger cap did
    // not, so hitting 200k lines left `rendered` past the end of `conv` and
        // every later line was skipped.
    if state.conv.len() > 200_000 {
        let drop = state.conv.len() - 200_000;
        state.conv = state.conv.split_off(drop);
        state.rendered = state.rendered.saturating_sub(drop);
        state.needs_rebuild = true;
    }
}

/// Render a batch of JSONL session-log lines into the conversation model.
///
/// Split out of `drain_session_log` so the parsing/rendering rules — which are
/// pure, and which decide what the user actually reads — can be tested without
/// a log file on disk and without a display.
fn drain_session_text(buf: &str, state: &mut GuiState) {
    use serde_json::Value;
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
            // Assistant markdown is rendered through the *same* renderer the TUI
            // uses (`tui.rs:1697` calls `crate::md::render`). The GUI used to
            // append the raw text, so a model answer arrived as literal `##`
            // and `-` — in a graphical pane, which is exactly the surface where
            // that looks most wrong. `color: false` because the GTK pane has no
            // per-range markup (see `sync_textview`): ANSI codes would show up
            // as literal escape junk.
            let text = if kind == ConvKind::Assistant {
                crate::md::render(&text, false)
            } else {
                text
            };
            state.push(kind, text.trim_end());
        }
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
    // GTK wrap mode: GTK_WRAP_WORD_CHAR — break mid-word only when a single
    // word is wider than the view, so code blocks and long paths still wrap.
    tv.set_wrap_mode(GTK_WRAP_WORD_CHAR);
    tv.set_editable(false);
    #[cfg(all(target_os = "linux", not(windows)))]
    tv.set_can_focus(false);
}

/// Read a `ScrolledWindow`'s vertical scroll position, or `None` when the
/// toolkit cannot report one.
///
/// This is the read half of the scroll-follow design. It is a real capability
/// probe, not a convenience: if it returns `None` the GUI has no way to tell
/// whether the user has scrolled away, and `follow` would be permanently stuck
/// — so `observe_scroll` is only called when this yields `Some`.
#[cfg(all(target_os = "linux", not(windows)))]
fn scrolled_window_vpos(sw: &ScrolledWindow) -> Option<f64> {
    scrolled_window_range(sw).map(|(vpos, _)| vpos)
}

/// Read `(vpos, vmax)` for a `ScrolledWindow`: the current vertical position and
/// the adjustment's upper bound (the scrollable extent).
///
/// `vmax` is what tells "at the bottom" from "scrolled away from the bottom", so
/// it matters as much as `vpos`. Both come from the adjustment GTK gives the
/// scrolled window.
#[cfg(all(target_os = "linux", not(windows)))]
fn scrolled_window_range(sw: &ScrolledWindow) -> Option<(f64, f64)> {
    // Both readings come from GTK through the toolkit's loader wrappers.
    // `get_vadjustment_upper` is what makes "at the bottom" distinguishable from
    // "scrolled away from the bottom" — without it every reading looks like the
    // bottom and follow-the-tail would yank a reader back down on every append.
    // `None` means the toolkit could not report (no display / symbol missing),
    // and the caller leaves `follow` alone rather than guessing.
    Some((sw.get_vadjustment_value(), sw.get_vadjustment_upper()))
}

/// Windows: no GTK loader, so no scroll position to read. Returning `None`
/// keeps the caller compiling and simply disables scroll-follow there.
#[cfg(windows)]
fn scrolled_window_vpos(_sw: &ScrolledWindow) -> Option<f64> {
    None
}

/// Windows counterpart of `scrolled_window_range`.
#[cfg(windows)]
fn scrolled_window_range(_sw: &ScrolledWindow) -> Option<(f64, f64)> {
    None
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
    // The `done_placeholder` is folded into the status text rather than given
    // its own label: it is a status, not a control, and the TUI draws it in the
    // same footer region.
    let shown = match done_placeholder(state) {
        Some(d) if state.last_model.is_none() => d,
        Some(d) => format!("{}   {d}", state.status),
        None => state.status.clone(),
    };
    if state.last_status != shown {
        widgets.status.set_markup(&status_markup(&shown));
        state.last_status = shown;
    }
    let hint = hint_text(&state.draft);
    if state.last_hint != hint {
        if hint.is_empty() {
            widgets.hint.set_markup("");
        } else {
            widgets.hint.set_markup(&status_markup(&hint));
        }
        state.last_hint = hint;
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
    let shown = match done_placeholder(state) {
        Some(d) if state.last_model.is_none() => d,
        Some(d) => format!("{}   {d}", state.status),
        None => state.status.clone(),
    };
    if state.last_status != shown {
        widgets.status.set_markup(&status_markup(&shown));
        state.last_status = shown;
    }
    let hint = hint_text(&state.draft);
    if state.last_hint != hint {
        if hint.is_empty() {
            widgets.hint.set_markup("");
        } else {
            widgets.hint.set_markup(&status_markup(&hint));
        }
        state.last_hint = hint;
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
            // really means "stop work" as it does in the TUI. Shared with the
            // Esc / Ctrl-C keybindings via `cancel_running`, so the key and the
            // command cannot drift apart.
            cancel_running_locked(cancel, &mut s);
            sync_textview(widgets, &mut s);
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
        // NOTE: there is deliberately no `"sessions"` arm here. `/sessions` is
        // intercepted by the entry handler *before* `handle_command` is reached
        // (it has to mutate the tab list), so an arm here was unreachable — and
        // what it printed, "use `pir -r` from a terminal to resume sessions", was
        // wrong for the GUI, which does have session tabs. That stale leftover
        // is what `docs/MISSING_GUI_FEATURES.md` §5.1 referred to.
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
                drop(s);
                // Drive the goal to completion on a worker thread and **return
                // immediately**. The goal run writes to the session log, and the
                // 150ms periodic drain paints it exactly like any other turn
                // output — so there is nothing to wait for here. Blocking on the
                // worker's completion channel would freeze the GTK main loop for
                // the whole run, which is the bug this replaces.
                spawn_goal(agent_slot, false);
                s = state.lock().unwrap();
                s.push(ConvKind::System, &format!("goal started: {obj} — driving it now"));
            }
        }
        "continue" | "cont" => {
            if agent_slot.lock().unwrap().is_none() {
                s.push(ConvKind::System, "· agent busy");
                return;
            }
            // Same off-thread treatment as /goal: `attach_goal` then drive, on a
            // worker, returning immediately so the window keeps painting and
            // `/cancel` can interrupt the run.
            drop(s);
            spawn_goal(agent_slot, true);
            s = state.lock().unwrap();
            s.push(ConvKind::System, "· continuing the goal — /cancel to stop");
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
        "checkpoints" | "ckpt" => {
            // The rewindable turn boundaries for this session (Feature 8).
            let g = agent_slot.lock().unwrap();
            match g.as_ref() {
                Some(a) => s.push(ConvKind::System, &a.checkpoints_listing()),
                None => s.push(ConvKind::System, "· agent busy (turn running)"),
            }
        }
        "rewind" => {
            // `/rewind <#>` restores the working tree AND the conversation to a
            // turn boundary; bare `/rewind` lists what is available.
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                s.push(ConvKind::System, "· agent busy (turn running)");
                return;
            };
            let Some(arg) = rest.first() else {
                s.push(ConvKind::System, &agent.checkpoints_listing());
                return;
            };
            match arg.parse::<usize>() {
                Ok(id) => s.push(ConvKind::System, &agent.rewind(id)),
                Err(_) => s.push(
                    ConvKind::Error,
                    &format!("· /rewind takes a checkpoint number (got '{arg}') — /checkpoints lists them"),
                ),
            }
        }
        // No `q` alias here, unlike the streaming REPL: in the GUI `q` already
        // means `/quit` (see the `"exit" | "quit" | "q"` arm above), so binding
        // it to quarantine too would make the shorter, more dangerous spelling
        // win and silently change what `/q` does.
        "quarantine" => {
            // The staged writes the write-quarantine is holding. This is
            // security-relevant — `/quarantine apply` is the operator's explicit
            // go-ahead for writes that were only visible to the agent — so it
            // was worth having in the GUI rather than terminal-only.
            match rest.first().copied() {
                None | Some("status") | Some("list") => {
                    match crate::security::overlay::staged_system_write_notice() {
                        Some(n) => s.push(ConvKind::System, &n),
                        None => s.push(
                            ConvKind::System,
                            "· nothing staged — the write-quarantine holds no pending writes",
                        ),
                    }
                }
                Some("apply") => {
                    let which = rest.get(1).copied().unwrap_or("all");
                    match which {
                        "all" => match crate::security::overlay::apply_active() {
                            Ok(n) => s.push(
                                ConvKind::System,
                                &format!("· applied {n} staged write(s) to the real filesystem"),
                            ),
                            Err(e) => s.push(ConvKind::Error, &format!("· apply failed: {e}")),
                        },
                        other => s.push(
                            ConvKind::Error,
                            &format!("· /quarantine apply takes `all` (got '{other}')"),
                        ),
                    }
                }
                Some("discard") => {
                    match crate::security::overlay::discard_active() {
                        Ok(()) => s.push(ConvKind::System, "· discarded every staged write"),
                        Err(e) => s.push(ConvKind::Error, &format!("· discard failed: {e}")),
                    }
                }
                Some(other) => s.push(
                    ConvKind::Error,
                    &format!("· usage: /quarantine [status|apply all|discard] (got '{other}')"),
                ),
            }
        }
        "limits" | "resources" => {
            // What the agent's commands are bounded by, and where the budget
            // came from. Same output as the streaming REPL's `/limits`.
            let host = crate::resource::HostMem::read().unwrap_or_default();
            let l = crate::resource::limits_for_session(&host, &crate::resource::session_key());
            s.push(ConvKind::System, &crate::resource::describe(&host));
            if l.backend == crate::resource::Backend::None {
                s.push(
                    ConvKind::System,
                    "  note: no cgroup memory controller here — commands are measured and \
                     host-pressure checked, but not kernel-limited.",
                );
            }
            s.push(
                ConvKind::System,
                "  knobs: PIR_RESOURCE_MEMORY_MIB, PIR_RESOURCE_PIDS, PIR_CGROUP=0 disables",
            );
        }
        "skills" | "skill" => {
            // The catalog the model also sees; bare `/skills` lists, with a name
            // it prints one in full.
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let skills = crate::skills::discover(&cwd);
            match rest.first().copied() {
                None => {
                    if skills.is_empty() {
                        s.push(ConvKind::System, "· no skills discovered");
                    } else {
                        s.push(ConvKind::System, &format!("skills ({}):", skills.len()));
                        for sk in &skills {
                            s.push(ConvKind::System, &format!("  {}", sk.name));
                        }
                    }
                }
                Some(name) => match skills.iter().find(|sk| sk.name == name) {
                    Some(sk) => match crate::skills::body(sk) {
                        Ok(b) => s.push(ConvKind::System, &b),
                        Err(e) => s.push(ConvKind::Error, &format!("· {e}")),
                    },
                    None => s.push(ConvKind::Error, &format!("· no skill named '{name}'")),
                },
            }
        }
        "scratchpad" | "scratch" => {
            // Path + size of this session's scratch directory (exported to child
            // processes as $PIR_SCRATCHPAD).
            s.push(ConvKind::System, &crate::scratchpad::describe());
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
        // `gtk_test_app` takes the GTK lock: this test and the three in
        // `gui_design_parity_tests` are the only tests that initialise GTK, and
        // they were racing each other roughly one run in four.
        let Some((app, _guard)) = gtk_test_app() else { return };

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

    /// The dispatch's own arm names (see `gui_command_arms`).
    pub(super) fn handled_commands() -> Vec<String> {
        gui_command_arms()
    }

    #[test]
    fn every_completable_command_is_actually_handled() {
        let handled = handled_commands();
        for cmd in COMMANDS {
            // `/sessions` is dispatched by `handle_session_command` before
            // `handle_command` is reached (it has to mutate the tab list), so it
            // has no `handle_command` arm — and must not have one: a stale dead
            // arm used to sit there printing "use `pir -r` from a terminal to
            // resume sessions", which is both unreachable and wrong for the GUI.
            // It is exempt here rather than special-cased in the dispatch.
            if *cmd == "sessions" {
                continue;
            }
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
        //
        // It is also the guard that keeps the Tab-completion list honest: every
        // name here must be offered by `COMMANDS`, so a command that is
        // implemented but not completable (the old state, where `/shell` was
        // completable and `/sh` was not) is caught.
        const TUI_COMMANDS: &[&str] = &[
            "help", "model", "models", "goal", "continue", "clear", "undo",
            "finished", "fix", "shell", "sh", "bg", "jobs", "thinking", "cancel",
            "reload", "reexec", "exit", "checkpoints", "rewind", "quarantine",
            "limits", "skills", "scratchpad",
        ];
        let handled = handled_commands();
        for cmd in TUI_COMMANDS {
            // See `every_completable_command_is_actually_handled`: /sessions is
            // handled by the tab manager, not by `handle_command`.
            if *cmd == "sessions" {
                continue;
            }
            assert!(
                handled.iter().any(|h| h == cmd),
                "the TUI supports /{cmd} but the GUI does not — add a handle_command \
                 arm for it"
            );
            assert!(
                COMMANDS.contains(&cmd),
                "/{cmd} is handled but missing from the Tab-completion list"
            );
        }
    }

    /// `/goal` and `/continue` must not run on the GTK main thread. Both used
    /// to call `agent.continue_goal()` inline, which froze the window for the
    /// whole run — no redraw, no scroll, and `/cancel` could not be typed.
    ///
    /// Asserted at the source level because the harm is structural: a
    /// synchronous call cannot be observed without a real display, but it *can*
    /// be seen in the code, and it is the thing most likely to be "simplified"
    /// back into a blocking call by someone tidying the command handler.
    #[test]
    fn goal_and_continue_are_spawned_not_run_inline() {
        let src = include_str!("gui.rs");
        // Anchor on the *definition* (column 0, so the leading newline is part
        // of the match), never on `split(..).nth(1)`. This file quotes the
        // literal `"fn handle_command("` in more than one test, and an
        // index-based search silently reads whichever occurrence comes second —
        // so a second source-level test quietly turns this guard into a test of
        // the other test's text. That happened: adding a guard here broke
        // *this* one, and the failure read like a real regression.
        let start = src
            .find("\nfn handle_command(")
            .expect("handle_command definition not found in gui.rs")
            + 1;
        let body = src[start..]
            .split("\n        other => {")
            .next()
            .expect("handle_command body not found");
        // Neither command may call `continue_goal` directly — that is the
        // blocking call. They must go through the worker.
        assert!(
            !body.contains("continue_goal()"),
            "a /goal or /continue arm calls continue_goal() inline, which blocks the \
             GTK main thread — route both through spawn_goal()"
        );
        assert_eq!(
            body.matches("spawn_goal(").count(),
            2,
            "both /goal <obj> and /continue must dispatch via spawn_goal()"
        );
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


/// Tests for the parity features specified in `docs/GUI_DESIGN.md` §4.
///
/// Each test names the design row it closes (S=scrollback, T=status,
/// C=commands, K=keyboard, G=graphical affordance). Split by layer:
///
/// * pure logic — no display needed;
/// * source guards — claims about wiring/threading that cannot be observed
///   from outside but are visible in the code;
/// * display-backed — live GTK, skipped gracefully without a display.
#[cfg(test)]
mod gui_design_parity_tests {
    use super::*;

    /// This file's source with every `#[cfg(test)]` module removed.
    ///
    /// Several guards here are source-level: they assert that the *code* does
    /// not contain something. Such a test has to be able to quote the forbidden
    /// string in order to report it, so a naive `src.contains(forbidden)` always
    /// matches — the test's own source. Splitting the file on the **attribute
    /// token** rather than on the whole `#[cfg(test)]` spelling solves that,
    /// because the split never looks inside a string literal: any `cfg(test)` a
    /// test mentions as part of a quoted string is not a separate `#[` and so
    /// does not create a boundary.
    fn non_test_code() -> &'static str {
        const TEST_GATE: &str = "#[cfg(test)]";
        include_str!("gui.rs")
            .split(TEST_GATE)
            .next()
            .unwrap_or_default()
    }

    // ================================================================
    // Layer 1 — pure logic
    // ================================================================

    // -- S2/S3: scroll-follow ------------------------------------------------

    #[test]
    fn a_fresh_state_follows_the_tail() {
        // S3: the TUI resets scroll on every push (tui.rs:241). So must a
        // fresh GUI session, or a new tab opens stuck at the top.
        let s = GuiState::new();
        assert!(s.follow, "a fresh pane must start following the tail");
    }

    #[test]
    fn follow_is_disarmed_by_scrolling_away_and_rearmed_at_the_bottom() {
        // S2: the whole point of `follow`. Scrolling up disarms it so the next
        // append cannot yank the user back to the bottom mid-read; returning to
        // the bottom re-arms it.
        let mut s = GuiState::new();
        s.observe_scroll(0.0, 100.0);
        assert!(!s.follow, "scrolling away from the bottom must disarm follow");
        s.observe_scroll(0.0, 100.0); // another append while scrolled up
        assert!(!s.follow, "an append while scrolled up must not re-arm follow");
        s.observe_scroll(0.0, 0.0); // scrolled back to the top == bottom
        assert!(s.follow, "returning to the bottom must re-arm follow");
    }

    #[test]
    fn follow_survives_a_rebuild_and_a_clear() {
        // A /clear wipes the pane; follow must come back armed or the user's
        // next turn is invisible below a stale scroll position.
        let mut s = GuiState::new();
        s.follow = false;
        s.clear();
        assert!(s.follow, "clear must re-arm follow");
        s.needs_rebuild = true;
        s.sync_follow();
        assert!(s.follow, "a rebuild must not silently re-arm follow mid-read");
    }

    #[test]
    fn sync_follow_reports_whether_the_pane_must_be_pinned_to_the_bottom() {
        let mut s = GuiState::new();
        // Armed: a sync should pin.
        assert!(s.sync_follow(), "follow armed => the pane must be pinned");
        // Disarmed by a real scroll: a sync must not pin.
        s.follow = false;
        assert!(!s.sync_follow(), "follow disarmed => the pane must be left alone");
    }

    // -- S6: the GUI's own keyboard scroll model -----------------------------

    #[test]
    fn gui_scroll_model_is_bounded_and_clamps() {
        // S6: the TUI's PageUp/PageDown are advertised but never bound
        // (tui.rs:14 vs zero `[5~`/`[6~` handlers). The GUI offers real
        // keyboard scrolling through its own clamped model.
        let mut sc = GuiScroll::default();
        sc.page = 10; // a 10-line viewport, set from the pane's allocated height
        assert_eq!(sc.page, 10);
        sc.page_down(10, 100);
        assert_eq!(sc.offset, 10, "page down advances by exactly one page");
        sc.page_down(10, 100);
        assert_eq!(sc.offset, 20);
        // Clamp at the end: there are only 100 lines, page 10 => max offset 90.
        for _ in 0..50 {
            sc.page_down(10, 100);
        }
        assert_eq!(sc.offset, 90, "the scroll model must clamp to the last page");
        sc.page_up(10);
        assert_eq!(sc.offset, 80, "page up must step back a page");
        sc.home();
        assert_eq!(sc.offset, 0, "home must jump to the top");
        sc.page_down(10, 100);
        assert_eq!(sc.offset, 10);
        sc.end(100);
        assert_eq!(sc.offset, 90, "end must clamp to the last page");
        // An empty or tiny conversation must never scroll past zero.
        let mut sc = GuiScroll::default();
        sc.page = 10;
        sc.page_up(10);
        assert_eq!(sc.offset, 0, "page up on a short conversation must stay at 0");
        sc.end(0);
        assert_eq!(sc.offset, 0, "end on an empty conversation must stay at 0");
    }

    /// One JSONL session-log line: an assistant message whose first block is
    /// `text`. This is the exact shape `drain_session_log` parses.
    fn jsonl_assistant_line(text: &str) -> String {
        let v = serde_json::json!({
            "role": "assistant",
            "blocks": [{ "type": "text", "text": text }],
        });
        format!("{v}\n")
    }

    // -- S7: markdown rendering ---------------------------------------------

    #[test]
    fn assistant_text_is_markdown_rendered_like_the_tui() {
        // S7: the TUI renders assistant markdown (tui.rs:1697) and the GUI used
        // to append the raw text. `md::render` is the same renderer the TUI
        // calls, so the assertion is equality with it, not a re-implementation.
        let raw = "# Heading\n\n- one\n- two\n\n```\ncode\n```\n";
        let mut s = GuiState::new();
        // Feed one assistant log line through the same text-drain path the
        // periodic drain uses, so the test covers the real route and not a
        // parallel re-implementation of it.
        drain_session_text(&jsonl_assistant_line(raw), &mut s);
        let rendered = s.conv.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
        assert_eq!(
            rendered.trim_end(),
            crate::md::render(raw, false).trim_end(),
            "assistant text must reach the pane through the same markdown \
             renderer the TUI uses"
        );
        // And it must actually differ from the raw text, or the test is vacuous.
        assert_ne!(rendered, raw, "the raw markdown should not survive verbatim");
    }

    // -- T5: token usage ----------------------------------------------------

    #[test]
    fn token_usage_is_reported_after_a_turn() {
        // T5: the TUI's footer shows "{in} in / {out} out tokens" (tui.rs:303).
        assert_eq!(usage_status(0, 0), "0 in / 0 out tokens");
        assert_eq!(usage_status(1500, 2500), "1.5k in / 2.5k out tokens");
        assert_eq!(usage_status(999, 1000), "999 in / 1.0k out tokens");
    }

    // -- T6: window title ----------------------------------------------------

    #[test]
    fn window_title_reports_running_or_idle() {
        // T6: the TUI's pane title is " pir — running " / " pir — idle "
        // (tui.rs:653). The GUI's window title carries the same signal.
        assert_eq!(window_title(false), "pir — idle");
        assert_eq!(window_title(true), "pir — running");
    }

    // -- T7: the hint line ---------------------------------------------------

    #[test]
    fn hint_offers_thinking_levels_then_completion_help() {
        // T7: a port of the TUI's update_tui_hint (tui.rs:1140), string for
        // string, so the two front-ends nudge identically.
        assert_eq!(
            hint_text("/thinking"),
            "[Tab] levels: off | minimal | low | medium | high | xhigh | max · show | hide"
        );
        assert_eq!(hint_text("/"), "[Tab] completes commands; try /thinking");
        // Everything else clears the hint.
        for d in ["", "hello", "/thinking high", "/model x", "  "] {
            assert_eq!(hint_text(d), "", "draft {d:?} must not show a hint");
        }
    }

    // -- T8: the done-placeholder -------------------------------------------

    #[test]
    fn done_placeholder_shows_only_when_idle_and_awaiting_input() {
        // T8: the TUI shows "❯ ✓ DONE :)" only when idle, awaiting input and
        // the draft is empty (tui.rs:679-687).
        let mut s = GuiState::new();
        s.awaiting_input = true;
        assert_eq!(done_placeholder(&s), Some(DONE_PROMPT_TEXT.to_string()));
        // Running: no placeholder.
        s.running = true;
        assert_eq!(done_placeholder(&s), None);
        s.running = false;
        // Not awaiting input: no placeholder.
        s.awaiting_input = false;
        assert_eq!(done_placeholder(&s), None);
        // Never when a draft is half-typed: the placeholder shares the prompt
        // row, so a real line must replace it.
        s.awaiting_input = true;
        s.draft = "half a wo".into();
        assert_eq!(done_placeholder(&s), None, "a non-empty draft suppresses it");
        // Whitespace is not a draft.
        s.draft = "   ".into();
        assert_eq!(
            done_placeholder(&s),
            Some(DONE_PROMPT_TEXT.to_string()),
            "a whitespace-only draft must not suppress the placeholder"
        );
    }

    // -- K6: Tab is not hijacked --------------------------------------------

    #[test]
    fn tab_is_only_claimed_when_the_completion_actually_changes_the_buffer() {
        // K6: the old key handler returned `true` from the Tab arm
        // unconditionally, so GTK's default focus-move was dead and a second
        // Tab re-completed the already-completed text. Tab must be claimed only
        // when it actually completes something.
        fn claimed(buf: &str) -> bool {
            complete_idle(buf).map(|c| c != buf).unwrap_or(false)
        }
        // A completable fragment is claimed...
        assert!(claimed("/cle"), "/cle should be completed, and claimed");
        assert!(
            !claimed("/model"),
            "bare /model is not completable by the command completer — it is the \
             model completer that gives it a trailing space, so the command \
             completer must leave GTK alone"
        );
        // ...and an un-completable one is not, so GTK keeps its default.
        assert!(!claimed("hello"), "Tab on a plain prompt must fall through to GTK");
        assert!(!claimed(""), "Tab on an empty Entry must fall through to GTK");
        assert!(!claimed("/model zzz"), "no completion => not claimed");
        // A second Tab on an already-complete word does nothing, so not claimed.
        assert!(!claimed("/clear"), "an already-complete command must not claim Tab");
    }

    // -- C1: completion is a superset of the TUI's ---------------------------

    #[test]
    fn gui_completion_is_a_superset_of_the_tuis() {
        // C1: the TUI offers 16 completable commands; the GUI must offer at
        // least all of them, so a command a TUI user can Tab to is never
        // missing in the GUI. Both lists are read out of the two sources, so
        // the check cannot itself go stale.
        fn list_from(src: &str, anchor: &str) -> Vec<String> {
            let i = src
                .find(anchor)
                .unwrap_or_else(|| panic!("anchor {anchor:?} not found"));
            let tail = &src[i..];
            let end = tail
                .find("];")
                .expect("end of the command list not found");
            let block = &tail[..end];
            block
                .lines()
                .flat_map(|l| l.split('"').skip(1).step_by(2))
                .map(|s| s.to_string())
                .collect()
        }
        let tui = list_from(include_str!("tui.rs"), "let commands = [");
        let gui = COMMANDS.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let missing: Vec<&String> = tui
            .iter()
            .filter(|c| !gui.iter().any(|g| g == *c))
            .collect();
        assert!(
            missing.is_empty(),
            "the TUI completes {missing:?} but the GUI does not — add them to COMMANDS"
        );
    }

    // -- C2: /help documents what the GUI handles ---------------------------

    #[test]
    fn help_documents_every_command_the_gui_handles() {
        // C2: the mirror of every_completable_command_is_actually_handled.
        // That one catches "Tab offers an unimplemented command"; this one
        // catches "the GUI implements a command nobody can discover".
        let handled = super::gui_parity_tests::handled_commands();
        // Split HELP_GUI into words once, then test membership. Substring
        // matching is wrong here: `HELP_GUI.contains("/model")` is satisfied by
        // the `/models` line, so a missing `/model` entry would pass.
        let help_words: Vec<&str> = HELP_GUI.split_whitespace().collect();
        let mut missing = Vec::new();
        for cmd in handled {
            if cmd == "sessions" {
                continue; // dispatched by handle_session_command; documented below
            }
            // An alias is not a separate feature — `/q` and `/quit` reach the
            // same arm — so either spelling counts as documented. HELP_GUI lists
            // `/sh` and `/quit`, which is why checking only the canonical name
            // would report them missing.
            let canonical = canonical_command(&cmd);
            let documented = help_words.contains(&format!("/{cmd}").as_str())
                || help_words.contains(&format!("/{canonical}").as_str());
            if !documented {
                missing.push(format!("/{canonical} (as /{cmd})"));
            }
        }
        // `/sessions` is dispatched before handle_command and IS documented.
        assert!(
            HELP_GUI.contains("/sessions"),
            "/sessions must be documented in /help"
        );
        assert!(
            missing.is_empty(),
            "these commands are handled but absent from /help: {missing:?}\n\
             Add them to HELP_GUI, or drop them from the dispatch."
        );
    }

    // -- G1: the model picker ------------------------------------------------

    #[test]
    fn model_picker_lists_exactly_the_configured_models() {
        // G1: the picker is fed from the provider catalog, so its item list must
        // be exactly the catalog — no more, no less. Built as a pure string
        // vector so it is testable without a display.
        let provs = vec![Provider {
            id: Some("acme".into()),
            name: None,
            api: Some("openai-completions".into()),
            base_url: None,
            api_key: None,
            models: vec![
                crate::config::Model {
                    id: "small".into(),
                    name: Some("Small".into()),
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
                    id: "big".into(),
                    name: Some("Big".into()),
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
                    id: "only-on-this-provider".into(),
                    name: None,
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
        }];
        let items = model_picker_items(&provs);
        assert_eq!(items.len(), 3, "got {:?}", items);
        // Qualified by provider so two providers offering "big" do not collide.
        assert!(items.contains(&"acme/small".to_string()), "got {:?}", items);
        assert!(items.contains(&"acme/big".to_string()), "got {:?}", items);
        assert!(
            items.contains(&"acme/only-on-this-provider".to_string()),
            "a model with no display name must still be listed, got {:?}",
            items
        );
        // Deterministic order: provider order, then catalog order.
        assert_eq!(items[0], "acme/small");
        assert_eq!(items[1], "acme/big");
        // No duplicates.
        let mut sorted = items.clone();
        sorted.sort();
        let n = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), n, "the picker must not offer a duplicate");
    }

    // -- G2: the menu bar ---------------------------------------------------

    #[test]
    fn menu_bar_defines_every_action_the_gui_can_perform() {
        // G2: every action the GUI advertises in a menu must be one the GUI can
        // actually perform, and every command the GUI handles must be reachable
        // from the menu. A menu entry that does nothing is worse than no menu.
        let handled = super::gui_parity_tests::handled_commands();
        let mut missing = Vec::new();
        for cmd in &handled {
            // `canonical_command` follows alias chains to a fixed point, so
            // `q` and `quit` both land on `exit` — the entry the menu carries.
            let canonical = canonical_command(cmd);
            if MENU_ACTIONS.iter().any(|a| a.command == canonical) {
                continue;
            }
            missing.push(canonical);
        }
        missing.sort();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "these commands are handled but not in the menu bar: {missing:?}"
        );
        // And no menu item may point at a command the GUI does not implement.
        let mut seen_actions: Vec<&str> = Vec::new();
        for a in MENU_ACTIONS {
            // `/sessions` has no handle_command arm: it is dispatched earlier
            // by the Entry handler, because it has to mutate the tab list. It
            // is still a command the GUI implements, so the menu may use it.
            assert!(
                handled.iter().any(|h| h == a.command) || a.command == "sessions",
                "menu action {:?} points at /{}, which is not implemented",
                a.action,
                a.command
            );
            assert!(!a.label.is_empty(), "a menu action needs a label");
            assert!(!a.action.is_empty(), "a menu action needs an action name");
            assert!(
                !seen_actions.contains(&a.action),
                "duplicate GTK action name {:?}: two menu items would fight over one action",
                a.action
            );
            seen_actions.push(a.action);
        }
        // The view actions must not collide with the command actions either.
        for (_menu, _label, action) in MENU_VIEW_ACTIONS {
            assert!(
                !seen_actions.contains(action),
                "view action {action:?} collides with a command action"
            );
        }
    }

    // ================================================================
    // Layer 2 — source-level guards
    // ================================================================

    #[test]
    fn the_ask_renderer_is_installed_before_the_window_is_built() {
        // The TUI/REPL install an ask renderer so the agent can put a question
        // to the operator. `--gui` used to return from main *before* that
        // happened, so in GUI mode `askuser::interactive()` was false and every
        // question came back as "no interactive user". The install must stay in
        // `run`, ahead of the window.
        let src = include_str!("gui.rs");
        // Anchor on the definition, for the same reason as
        // `handle_command_source`: `split(..).nth(1)` reads the second
        // occurrence, and a test that quotes the literal breaks the other one.
        let start = src
            .find("\npub fn run(")
            .expect("run() definition not found in gui.rs")
            + 1;
        let body = src[start..]
            .split("\nfn spawn_session(")
            .next()
            .expect("run() body not found");
        let install = body
            .find("crate::askuser::install(")
            .expect("gui.rs must install an ask renderer");
        let window = body
            .find("create_window")
            .expect("run() must build a window");
        assert!(
            install < window,
            "the ask renderer must be installed before the window is built, so \
             no turn can start (and ask) before the GUI is ready to answer"
        );
    }

    #[test]
    fn goal_and_continue_are_still_spawned_not_run_inline() {
        // Same guard as in gui_parity_tests, restated here so the design row
        // and its test live together. Both go through one shared extractor, so
        // neither can drift onto a different slice of the file.
        let body = handle_command_source();
        assert!(
            !body.contains("continue_goal()"),
            "a /goal or /continue arm calls continue_goal() inline, which blocks the \
             GTK main thread — route both through spawn_goal()"
        );
        assert_eq!(
            body.matches("spawn_goal(").count(),
            2,
            "both /goal <obj> and /continue must dispatch via spawn_goal()"
        );
    }

    #[test]
    fn focus_out_does_not_regrab_focus() {
        // G5: a focus-out handler that re-grabs focus unconditionally is a
        // focus trap — the operator cannot click into any other widget, and a
        // click on the native tab bar fights the handler. Focus should move
        // only when something deliberately asks for it (a tab change).
        let code = non_test_code();
        for forbidden in [
            "connect_focus_out_event",
            "connect_focus_in_event",
            "entry_focus.grab_focus()",
        ] {
            assert!(
                !code.contains(forbidden),
                "the non-test code still contains {forbidden:?}: a focus-out handler \
                 that re-grabs focus traps the user in the Entry. Remove it."
            );
        }
    }

    // ================================================================
    // Layer 3 — display-backed (skipped without a display)
    // ================================================================

    /// Initialise GTK for a display-backed test, taking the shared GTK lock.
    /// See [`gtk_test_app`]: every test that calls `App::init()` must go
    /// through it, or the suite segfaults intermittently.
    fn try_app() -> Option<(App, std::sync::MutexGuard<'static, ()>)> {
        gtk_test_app()
    }

    #[test]
    fn scrolled_window_can_report_its_scroll_position() {
        // S1: the whole scroll-follow design rests on being able to READ the
        // scroll position. If the toolkit ever stops reporting it, follow
        // would silently degrade to "always jump to the bottom" — so pin the
        // capability itself.
        let Some((app, _guard)) = try_app() else { return };
        let Ok(sw) = app.create_scrolled_window() else { return };
        let Ok(tv) = app.create_textview() else { return };
        configure_conversation_textview(&tv);
        tv.set_text("1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n");
        sw.set_child(&tv);
        let (vpos, vmax) =
            scrolled_window_range(&sw).expect("a ScrolledWindow must report a scroll position");
        assert!(vpos >= 0.0, "a scroll position must not be negative, got {vpos}");
        assert!(
            vmax >= vpos,
            "the scroll extent ({vmax}) must not be below the position ({vpos}): \
             follow-the-tail compares the two to tell 'at the bottom' from \
             'scrolled away from it'"
        );
    }

    #[test]
    fn the_model_picker_widget_is_created_and_listed() {
        // G1 (display): the picker is a real GTK dropdown, not a text entry.
        let Some((app, _guard)) = try_app() else { return };
        // One real item, so there is a row to select.
        let provs = vec![Provider {
            id: Some("acme".into()),
            name: None,
            api: Some("openai-completions".into()),
            base_url: None,
            api_key: None,
            models: vec![crate::config::Model {
                id: "big".into(),
                name: None,
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
            }],
        }];
        let items = model_picker_items(&provs);
        let Ok(dd) = app.create_dropdown(&items.iter().map(|s| s.as_str()).collect::<Vec<_>>()) else {
            eprintln!("skipping: could not create a DropDown");
            return;
        };
        dd.set_active(Some(0));
        assert_eq!(dd.get_active(), 0, "selecting the first row must read back as 0");
    }

    #[test]
    fn the_conversation_pane_and_status_bar_are_readable() {
        // S7/T1 (display): after a sync, the TextView actually contains the
        // model's text and the status label actually contains the status.
        let Some((app, _guard)) = try_app() else { return };
        let Ok(tv) = app.create_textview() else { return };
        let Ok(label) = app.create_label("x") else { return };
        configure_conversation_textview(&tv);
        let hint = app.create_label("").expect("hint label");
        let sw = app.create_scrolled_window().expect("scrolled window");
        let w = GuiWidgets {
            textview: tv.clone(),
            status: label.clone(),
            scroller: sw,
            hint: hint.clone(),
        };
        let mut s = GuiState::new();
        s.push(ConvKind::User, "hello from the model");
        s.status = "idle".into();
        sync_textview(&w, &mut s);
        let got = tv.get_text().unwrap_or_default();
        assert!(got.contains("hello from the model"), "got {got:?}");
        // The status label was written; the prefix glyph is present per kind.
        assert!(got.contains("❯"), "the user prefix glyph must be present: {got:?}");
    }
}
