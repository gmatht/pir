use std::cell::RefCell;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::Mutex;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustyline::completion::Completer;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{CompletionType, Config, Context, Editor};

static COLOR: OnceLock<bool> = OnceLock::new();

/// Providers, set once at startup, so the line-editor helper can offer
/// `/model` tab-completion.
static MODEL_PROVIDERS: OnceLock<Vec<crate::config::Provider>> = OnceLock::new();

/// Register the providers for `/model` tab-completion. Call once after
/// models.json has been loaded.
pub fn set_model_providers(providers: &[crate::config::Provider]) {
    let _ = MODEL_PROVIDERS.set(providers.to_vec());
}

pub fn set_color(on: bool) {
    let _ = COLOR.set(on);
}

fn color() -> bool {
    *COLOR.get_or_init(|| io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
}

/// Whether stdout is attached to an interactive terminal (used by the
/// notification policy to decide whether the user is "watching").
pub fn is_terminal() -> bool {
    io::stdout().is_terminal()
}

/// Write `s` to stdout, never panicking and never busy-spinning on a slow or
/// full pipe. `print!`/`write!` to a non-blocking or full stdout return
/// `EAGAIN` ("Resource temporarily unavailable"), and Rust's std macros *panic*
/// on any write error — which previously killed the whole process mid-turn.
///
/// We drain the bytes with a bounded retry, and when the fd would block we
/// wait for it to become writable via the smol reactor (the same event-driven
/// mechanism the input path uses) instead of sleeping-and-retrying in a hot
/// loop. A genuinely broken pipe is ignored silently.
pub fn out(s: &str) {
    use std::os::unix::io::{AsRawFd, FromRawFd};
    let bytes = s.as_bytes();
    let mut written = 0usize;
    // Bound the total work so a persistent stall can't spin forever.
    for _ in 0..1024 {
        let mut stdout = io::stdout();
        match stdout.write_all(&bytes[written..]) {
            Ok(()) => {
                let _ = stdout.flush();
                return;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // EAGAIN: make stdout non-blocking (idempotent) and then block
                // until it is writable, event-driven via the smol reactor
                // (~0% CPU), instead of polling. We keep the fd non-blocking
                // afterward (harmless for stdout) so the wait is always correct.
                let fd = io::stdout().as_raw_fd();
                unsafe {
                    let flags = libc::fcntl(fd, libc::F_GETFL);
                    libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
                let block = async {
                    if let Ok(a) = smol::Async::new(unsafe { std::fs::File::from_raw_fd(fd) }) {
                        let _ = a.writable().await;
                        std::mem::forget(a); // we only borrowed fd 1; never close it
                    }
                };
                smol::block_on(block);
                continue;
            }
            Err(_) => return, // broken/closed pipe: give up silently
        }
    }
}

/// Flush stdout, ignoring any error (so a flush failure can't panic either).
pub fn out_flush() {
    let _ = io::stdout().flush();
}

/// Width of the terminal in columns (used to size the REPL hrule). Falls back
/// to 80 when the size can't be queried (e.g. a pipe or a non-tty).
pub fn terminal_width() -> usize {
    #[cfg(unix)]
    {
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws as *mut libc::winsize) == 0
                && ws.ws_col > 0
            {
                return ws.ws_col as usize;
            }
        }
    }
    80
}

/// A horizontal rule across the full terminal width (dimmed when color is on).
/// Drawn between the "thinking" spinner and the live REPL prompt while a turn
/// runs, so the REPL is visually "under" the spinner.
pub fn hrule() -> String {
    let w = terminal_width();
    let bar = "─".repeat(w.min(200));
    dim(&bar)
}

/// The live REPL prompt shown under the spinner while a turn runs. Mirrors the
/// idle rustyline prompt (`❯ `) so typed-ahead input looks like a normal line.
pub fn repl_prompt() -> String {
    format!("{} ", cyan("❯"))
}

fn paint(code: &str, s: &str) -> String {
    if color() { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
}

pub fn dim(s: &str) -> String { paint("2", s) }
pub fn bold(s: &str) -> String { paint("1", s) }
pub fn red(s: &str) -> String { paint("31", s) }
pub fn yellow(s: &str) -> String { paint("33", s) }
pub fn cyan(s: &str) -> String { paint("36", s) }

pub fn read_answer(prompt: &str) -> String {
    eprint!("{prompt} ");
    let _ = io::stderr().flush();
    let mut s = String::new();
    let _ = io::stdin().read_line(&mut s);
    s.trim().to_lowercase()
}

pub fn epoch() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// PID of the shell (bash) that launched pir, taken from the `PIR_PARENT_PID`
/// env var if present, else the real parent process id. Used to tag sessions
/// so `pir -r` can group/resume sessions from the same shell.
pub fn parent_shell_pid() -> u32 {
    if let Ok(v) = std::env::var("PIR_PARENT_PID") {
        if let Ok(n) = v.parse::<u32>() {
            if n != 0 {
                return n;
            }
        }
    }
    parent_pid()
}

#[cfg(target_os = "linux")]
fn parent_pid() -> u32 {
    // /proc/self/stat: pid (1) (ppid 4) ...
    if let Ok(s) = std::fs::read_to_string("/proc/self/stat") {
        if let Some(ppid) = s.split_whitespace().nth(3) {
            if let Ok(n) = ppid.parse() {
                return n;
            }
        }
    }
    std::process::id()
}

#[cfg(not(target_os = "linux"))]
fn parent_pid() -> u32 {
    std::process::id()
}

pub fn date_string() -> String {
    let (y, m, d, _, _, _) = utc_parts(epoch());
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn timestamp_compact() -> String {
    let (y, mo, d, h, mi, s) = utc_parts(epoch());
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

fn utc_parts(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let sod = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    (y, m, d, (sod / 3600) as u32, ((sod % 3600) / 60) as u32, (sod % 60) as u32)
}

/// Howard Hinnant's civil-from-days (no chrono dependency).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + i64::from(m <= 2), m, d)
}

/// rustyline helper: offers `/model` tab-completion plus a live preview
/// (up to ten matching `provider/model` labels) of the typed prefix.
struct PirHelper;

impl rustyline::Helper for PirHelper {}

impl Completer for PirHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        let providers = MODEL_PROVIDERS.get().map(|v| v.as_slice()).unwrap_or(&[]);
        let left = &line[..pos];
        let start_idx = left.find(|c: char| !c.is_whitespace()).unwrap_or(0);
        let rest = &left[start_idx..];
        let cmd_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let cmd = &rest[..cmd_end];
        if cmd != "/model" && cmd != "/m" {
            return Ok((0, Vec::new()));
        }
        let after = &rest[cmd_end..];
        if after.is_empty() {
            // command typed but no trailing space yet — nothing to complete
            return Ok((0, Vec::new()));
        }
        let arg_lead = after.find(|c: char| !c.is_whitespace()).unwrap_or(after.len());
        let arg_start = start_idx + cmd_end + arg_lead;
        let prefix = &left[arg_start..];
        let matches = crate::config::match_models(providers, prefix, 10);
        Ok((arg_start, matches))
    }
}

impl Hinter for PirHelper {
    type Hint = String;
    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        // Show the first matching model as an inline preview while typing a
        // `/model` argument, so the user sees a suggestion to the right.
        let providers = MODEL_PROVIDERS.get().map(|v| v.as_slice()).unwrap_or(&[]);
        if providers.is_empty() {
            return None;
        }
        let left = &line[..pos];
        let start_idx = left.find(|c: char| !c.is_whitespace()).unwrap_or(0);
        let rest = &left[start_idx..];
        let cmd_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let cmd = &rest[..cmd_end];
        if cmd != "/model" && cmd != "/m" {
            return None;
        }
        let after = &rest[cmd_end..];
        if after.is_empty() {
            return None;
        }
        let arg_lead = after.find(|c: char| !c.is_whitespace()).unwrap_or(after.len());
        let arg_start = start_idx + cmd_end + arg_lead;
        let prefix = &left[arg_start..];
        let candidates = crate::config::match_models(providers, prefix, 10);
        let hint = candidates
            .into_iter()
            .find_map(|m| crate::config::hint_remainder(&m, prefix));
        hint.filter(|h| !h.is_empty())
    }
}

impl Highlighter for PirHelper {
    fn highlight_hint<'h>(&self, hint: &'h str) -> std::borrow::Cow<'h, str> {
        if color() {
            std::borrow::Cow::Owned(format!("\x1b[2m{hint}\x1b[0m"))
        } else {
            std::borrow::Cow::Borrowed(hint)
        }
    }
}
impl Validator for PirHelper {}

thread_local! {
    // One editor per thread, reused across calls so history (arrow up/down)
    // and cursor bindings persist for the whole session.
    static EDITOR: RefCell<Option<Editor<PirHelper, rustyline::history::DefaultHistory>>> =
        const { RefCell::new(None) };
    // Persisted history store (a .history file next to the session log).
    static HISTORY_FILE: RefCell<Option<std::path::PathBuf>> = const { RefCell::new(None) };
}

/// Point the line editor at a history file that should be loaded on first
/// use and appended to on every line read. Call once after choosing a
/// session log path.
pub fn set_history_file(path: &Path) {
    HISTORY_FILE.with(|f| *f.borrow_mut() = Some(path.to_path_buf()));
    // Eagerly create the editor so the file is loaded before the first prompt.
    EDITOR.with(|e| {
        if e.borrow().is_none() {
            let _ = new_editor().map(|rl| *e.borrow_mut() = Some(rl));
        }
    });
    load_history();
}

/// Build a line editor with `/model` completion enabled.
fn new_editor() -> Option<Editor<PirHelper, rustyline::history::DefaultHistory>> {
    let config = Config::builder()
        .completion_type(CompletionType::List)
        .build();
    let mut rl = Editor::<PirHelper, rustyline::history::DefaultHistory>::with_config(config).ok()?;
    rl.set_helper(Some(PirHelper));
    Some(rl)
}

fn load_history() {
    EDITOR.with(|e| {
        let mut g = e.borrow_mut();
        let Some(rl) = g.as_mut() else { return };
        if let Some(path) = HISTORY_FILE.with(|f| f.borrow().clone()) {
            let _ = rl.load_history(&path);
        }
    });
}

fn save_history(rl: &mut Editor<PirHelper, rustyline::history::DefaultHistory>) {
    if let Some(path) = HISTORY_FILE.with(|f| f.borrow().clone()) {
        let _ = rl.save_history(&path);
    }
}

/// Read a line with full line editing: arrow-up/down history, left/right
/// cursor movement, home/end, word motion, etc. (provided by rustyline).
///
/// Returns `None` on EOF (e.g. ctrl-d) so the caller can quit cleanly.
pub fn read_line(prompt: &str) -> Option<String> {
    use rustyline::error::ReadlineError;

    // Initialization is best-effort; fall back to plain stdin if needed.
    EDITOR.with(|e| {
        if e.borrow().is_none() {
            *e.borrow_mut() = new_editor();
        }
        let mut guard = e.borrow_mut();
        let rl = match guard.as_mut() {
            Some(rl) => rl,
            None => return plain_read_line(prompt),
        };
        match rl.readline(prompt) {
            Ok(line) => {
                let _ = rl.add_history_entry(line.as_str());
                save_history(rl);
                Some(line)
            }
            // Ctrl-C: stay in the REPL instead of dying.
            Err(ReadlineError::Interrupted) => Some(String::new()),
            Err(ReadlineError::Eof) => None,
            Err(_) => None,
        }
    })
}

fn plain_read_line(prompt: &str) -> Option<String> {
    eprint!("{prompt} ");
    let _ = io::stderr().flush();
    let mut s = String::new();
    match io::stdin().read_line(&mut s) {
        Ok(0) => None,
        Ok(_) => Some(s),
        Err(_) => None,
    }
}

/// Append a line to the per-session history so prompts typed *while a turn was
/// running* (raw mode, recorded into `typeahead` and queued) show up in the
/// rustyline prompt's arrow-up history once we return to idle. Best-effort.
pub fn push_history(line: &str) {
    if line.trim().is_empty() {
        return;
    }
    EDITOR.with(|e| {
        let mut g = e.borrow_mut();
        if let Some(rl) = g.as_mut() {
            let _ = rl.add_history_entry(line);
            save_history(rl);
        }
    });
}


/// A small terminal spinner shown while we're waiting for the model to produce
/// its first token (the request is in flight but the stream hasn't started). It
/// animates on a background thread and overwrites its own block of lines; call
/// `stop()` the moment real output arrives. When stdout isn't a tty the spinner
/// is a silent no-op so logs / pipes stay clean.
///
/// The spinner renders a compact block under the agent's text while a turn
/// runs:
///
/// ```text
/// ⠋ thinking…
/// ────────────────────────────────────────────   (hrule, full terminal width)
/// ❯ <what the user is typing live>               (live REPL prompt)
/// ```
///
/// i.e. the hrule and a live REPL prompt sit *under* the "thinking" indicator,
/// and the user's keystrokes (recorded into `typeahead` by the REPL thread)
/// appear on the prompt line instead of being clobbered by the spinner. This
/// fixes the "REPL doesn't display during thinking" bug: the spinner thread is
/// the **only** thing that writes to stdout while it's alive, so it owns the
/// whole block and re-renders it in place each tick.
pub struct Spinner {
    handle: Option<JoinHandle<()>>,
    alive: Arc<AtomicBool>,
}

impl Spinner {
    /// Start spinning with the given leading label (e.g. "thinking"). Returns a
    /// `Spinner` whose `stop()` clears the block. Pass `false` for `enabled` to
    /// get a no-op spinner (used for quiet / non-tty contexts).
    ///
    /// `typeahead` is a buffer the REPL thread fills with any keystrokes the
    /// user types *while* the turn is running (the REPL runs in raw mode and is
    /// blocked waiting on the network). The spinner thread is the **only** thing
    /// that writes to stdout while it's alive, so it owns the "thinking" block
    /// (the spinner + hrule + live REPL prompt) and renders the user's typing
    /// on the prompt line below the hrule. This avoids two threads racing on
    /// stdout — the previous design had the main REPL thread echo keystrokes
    /// directly *and* the spinner rewrite the same line, which clobbered the
    /// user's input mid-thought (the "REPL doesn't display during thinking" bug).
    pub fn start(label: &str, typeahead: Arc<Mutex<String>>, enabled: bool) -> Spinner {
        if !enabled {
            return Spinner { handle: None, alive: Arc::new(AtomicBool::new(false)) };
        }
        let alive = Arc::new(AtomicBool::new(true));
        let a = alive.clone();
        let label = label.to_string();
        let handle = thread::spawn(move || {
            let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let mut i = 0usize;
            let mut out = io::stdout();
            let mut drawn = false;
            while a.load(Ordering::SeqCst) {
                let frame = if color() { format!("\x1b[36m{}\x1b[0m", frames[i % frames.len()]) } else { frames[i % frames.len()].to_string() };
                // Read the user's in-progress line (recorded by the REPL thread)
                // and show it on the live REPL line under the hrule so typing
                // "displays" while the model thinks. On the first tick we draw
                // the block from the current cursor position; afterwards we move
                // up to the block's top line before redrawing, so the block stays
                // anchored in place instead of scrolling. `\x1b[K` clears stale
                // tails after a backspace.
                let typed = typeahead.lock().map(|g| g.clone()).unwrap_or_default();
                let rule = hrule();
                let prompt = repl_prompt();
                let seq = if drawn {
                    "\r\x1b[K\x1b[2A\r\x1b[K"
                } else {
                    "\r\x1b[K"
                };
                let _ = out.write_all(
                    format!(
                        "{}{} {}…\n\x1b[K{}\n\x1b[K{}{}{}\x1b[K",
                        seq, frame, label, rule, prompt, typed, "\x1b[K"
                    )
                    .as_bytes(),
                );
                let _ = out.flush();
                drawn = true;
                thread::sleep(Duration::from_millis(80));
                i = i.wrapping_add(1);
            }
            // Erase the whole 3-line block (thinking line + hrule + draft-prompt
            // line) and leave the cursor on its TOP line (L0). We use `\x1b[2K`
            // (erase *entire* line) rather than `\x1b[K` (erase-to-EOL) — the
            // cursor is at end-of-line, so erase-to-EOL would only clip the tail
            // and leave "⠋ think" / "────" / "❯ hel" behind. The previous clear
            // also left the cursor on L1 (the hrule) and skipped the hrule line
            // entirely, so the next spinner's first draw anchored one line low
            // and the prior "⠋ thinking…" was never erased — every tool round
            // leaked another stray "thinking" onto the screen.
            let _ = out.write_all(
                b"\x1b[2A\x1b[2K\x1b[B\x1b[2K\x1b[B\x1b[2K\x1b[2A\x1b[2K",
            );
            let _ = out.flush();
        });
        Spinner { handle: Some(handle), alive }
    }

    /// Stop the spinner and erase its block. Safe to call multiple times.
    pub fn stop(&mut self) {
        if self.alive.swap(false, Ordering::SeqCst) {
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Non-blocking, raw-mode terminal input used while a foreground agent turn is
/// running on a worker thread. Lets the REPL stay responsive: the user can type
/// (the partial line is echoed) and press Enter to queue the next turn, or
/// ctrl-c to request cancellation — without the main thread ever blocking on a
/// read. This is intentionally minimal (no history/cursor nav): rich editing is
/// reserved for the idle rustyline prompt. Only printable ASCII plus
/// backspace/enter/ctrl-c/ctrl-d are handled; other bytes are ignored so escape
/// sequences (arrows) don't corrupt the buffer.
///
/// `enable_raw`/`disable_raw` manage the terminal attributes and the
/// non-blocking flag on stdin; the REPL toggles them around the running turn.
/// `wait_input` blocks event-driven (via the smol reactor) until stdin is
/// readable or the worker signals turn-completion, so the REPL thread sleeps
/// (0% CPU) instead of polling.
pub mod raw {
    use std::io::{self};
    use std::os::unix::io::AsRawFd;
    use std::sync::{Arc, Mutex};

    /// Terminal state captured around a raw-mode session. Guarded by a Mutex so
    /// access is never via a raw `&mut` to a static (sound under the 2024
    /// edition rules). There is only ever one REPL, so the lock is uncontended.
    struct RawState {
        orig_termios: Option<libc::termios>,
        orig_nonblock: Option<bool>,
        active: bool,
    }

    static STATE: Mutex<RawState> = Mutex::new(RawState {
        orig_termios: None,
        orig_nonblock: None,
        active: false,
    });

    /// Master switch for raw mode. When disabled (`--no-raw`), `enable_raw` /
    /// `disable_raw` become no-ops and the REPL falls back to line-buffered
    /// stdin. Default on. Safe to toggle before the first `enable_raw` call.
    static ENABLED: Mutex<bool> = Mutex::new(true);
    pub fn set_enabled(on: bool) {
        *ENABLED.lock().unwrap() = on;
    }

    /// Put stdin into raw, non-blocking mode (no canonical line editing, no
    /// echo, reads return immediately). Idempotent. No-op when raw mode is
    /// disabled (`--no-raw`).
    pub fn enable_raw() {
        if !*ENABLED.lock().unwrap() {
            return;
        }
        let mut st = STATE.lock().unwrap();
        if st.active {
            return;
        }
        unsafe {
            let fd = io::stdin().as_raw_fd();
            let mut tios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut tios) == 0 {
                st.orig_termios = Some(tios);
                let mut raw = tios;
                raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
                raw.c_cc[libc::VMIN] = 0;
                raw.c_cc[libc::VTIME] = 0;
                libc::tcsetattr(fd, libc::TCSANOW, &raw);
            }
            let flags = libc::fcntl(fd, libc::F_GETFL);
            st.orig_nonblock = Some(flags & libc::O_NONBLOCK != 0);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            st.active = true;
        }
    }

    /// Restore the previous terminal attributes and blocking mode. Idempotent.
    pub fn disable_raw() {
        if !*ENABLED.lock().unwrap() {
            return;
        }
        let mut st = STATE.lock().unwrap();
        if !st.active {
            return;
        }
        unsafe {
            let fd = io::stdin().as_raw_fd();
            if let Some(t) = st.orig_termios.take() {
                libc::tcsetattr(fd, libc::TCSANOW, &t);
            }
            if let Some(was) = st.orig_nonblock.take() {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                let newflags = if was {
                    flags | libc::O_NONBLOCK
                } else {
                    flags & !libc::O_NONBLOCK
                };
                libc::fcntl(fd, libc::F_SETFL, newflags);
            }
            st.active = false;
        }
    }

    /// Outcome of a single non-blocking poll/consume of pending stdin bytes.
    pub enum RawInput {
        /// Nothing was available (caller should pump notifications and retry).
        None,
        /// A full line was submitted (Enter). The buffer is consumed.
        Line(String),
        /// ctrl-c: caller should request cancellation of the running turn.
        Interrupt,
        /// ctrl-d: caller should stop the session.
        Eof,
        /// ctrl-z: caller should pause (suspend) the whole process and return to
        /// the parent shell. The REPL implements this by raising `SIGTSTP`
        /// (after dropping raw mode so the shell is usable) and re-enabling raw
        /// on resume. The partial input line is preserved across the suspend —
        /// unlike `Interrupt`/`Eof`, which clear it.
        Suspend,
    }

    /// Wait for input (or turn completion) and consume any available bytes,
    /// recording them into `typeahead` (the live "what the user is typing" line
    /// shown by the spinner thread — see [`Spinner::start`]). This REPL thread
    /// must **not** write to stdout itself while the spinner is alive: the
    /// spinner thread is the sole stdout writer during a turn, so echoing here
    /// would race with its carriage-return rewrites and clobber the display.
    /// Returns `RawInput::Line` only when Enter is pressed. `done` is a oneshot
    /// channel the worker thread closes when the foreground turn finishes; the
    /// call returns (with `None` if there's no pending input) as soon as *either*
    /// stdin becomes readable *or* the turn completes.
    ///
    /// This is fully event-driven: `smol::Async<stdin>` registers the terminal
    /// fd with the smol reactor so `readable()` wakes only when bytes actually
    /// arrive, and `block_on` puts the main thread to sleep until a wakeup
    /// fires — there is no `poll()` loop and no timer, so the REPL uses ~0% CPU
    /// while a worker turn is just waiting on the network (the agent
    /// "thinking"). Typing stays responsive because the reactor wakes
    /// immediately on a keypress.
    pub fn wait_input(
        buf: &mut String,
        typeahead: &Arc<Mutex<String>>,
        done: &smol::channel::Receiver<()>,
    ) -> RawInput {
        // Build an async wrapper around stdin for this wait. `enable_raw` already
        // put fd 0 into non-blocking mode; `Async::new` registers it with the
        // smol reactor (and deregisters on drop) so we can await readiness.
        let stdin = match smol::Async::new(io::stdin()) {
            Ok(s) => s,
            Err(_) => return read_chunk(buf, typeahead),
        };
        // Race "stdin became readable" against "the turn finished". Both arms
        // yield `()` so `or` can select between them.
        let readable = async { let _ = stdin.readable().await; };
        let finished = async { let _ = done.recv().await; };
        smol::block_on(smol::future::or(readable, finished));
        // Either side fired (or stdin closed): drain whatever is buffered.
        read_chunk(buf, typeahead)
    }

    /// Drain any currently-available stdin bytes, translating control chars and
    /// recording printable text into `typeahead` (for the spinner to render).
    /// `stdin` is non-blocking (see `enable_raw`), so this returns as soon as no
    /// more bytes are readable. Backspace pops the buffer; ctrl-c/ctrl-d/Enter
    /// are surfaced to the caller. This thread never writes to stdout.
    fn read_chunk(buf: &mut String, typeahead: &Arc<Mutex<String>>) -> RawInput {
        let fd = io::stdin().as_raw_fd();
        let mut tmp = [0u8; 256];
        let mut nread = 0usize;
        // stdin is non-blocking (see enable_raw), so read may return a partial
        // chunk; keep draining until it would block (r <= 0) or the buffer is
        // full. Without the loop a single read could leave bytes unconsumed.
        loop {
            let r = unsafe {
                libc::read(
                    fd,
                    tmp.as_mut_ptr().add(nread) as *mut libc::c_void,
                    tmp.len() - nread,
                )
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
            return RawInput::None;
        }
        for &b in &tmp[..nread] {
            match b {
                0x0a | 0x0d => {
                    let line = std::mem::take(buf);
                    return RawInput::Line(line);
                }
                0x7f | 0x08 => {
                    if !buf.is_empty() {
                        buf.pop();
                        // Update the shared typeahead so the spinner drops the
                        // removed character (it owns the only stdout writer).
                        if let Ok(mut g) = typeahead.lock() {
                            // Keep `g` only as long as needed; drop before any
                            // other stdout writer could race.
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
                    return RawInput::Interrupt;
                }
                0x04 => {
                    buf.clear();
                    if let Ok(mut g) = typeahead.lock() {
                        g.clear();
                    }
                    return RawInput::Eof;
                }
                0x1a => {
                    // ctrl-z: suspend. Do NOT clear `buf` — the partial line
                    // must survive the pause (the REPL raises SIGTSTP and the
                    // spinner/worker threads all stop with the process), so we
                    // return immediately and let the caller handle it.
                    return RawInput::Suspend;
                }
                0x1b => {
                    // ESC is ambiguous: a lone Esc should cancel the turn (like
                    // ctrl-c), but it's also the lead byte of arrow/function-key
                    // sequences (e.g. ← = 0x1b 0x5b 0x44). So peek for follow-up
                    // bytes with a short timeout: if none arrive, it's a lone Esc
                    // → cancel; otherwise it's an escape sequence → consume and
                    // ignore (we don't do cursor movement while raw).
                    if lone_escape(fd) {
                        buf.clear();
                        if let Ok(mut g) = typeahead.lock() { g.clear(); }
                        return RawInput::Cancel;
                    }
                    // Sequence: drain the remaining bytes so they don't leak into
                    // the next poll, then ignore.
                    drain_escape_sequence(fd);
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
        RawInput::None
    }
}
