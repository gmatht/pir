use std::cell::RefCell;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

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
        loop {
            match rl.readline(prompt) {
                Ok(line) => {
                    let _ = rl.add_history_entry(line.as_str());
                    save_history(rl);
                    return Some(line);
                }
                // Ctrl-C: stay in the REPL instead of dying.
                Err(ReadlineError::Interrupted) => return Some(String::new()),
                Err(ReadlineError::Eof) => return None,
                Err(_) => return None,
            }
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
pub mod raw {
    use std::io::{self, Write};
    use std::os::unix::io::AsRawFd;
    use std::sync::Mutex;

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

    /// Put stdin into raw, non-blocking mode (no canonical line editing, no
    /// echo, reads return immediately). Idempotent.
    pub fn enable_raw() {
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
    }

    /// Consume any available input, echoing printable chars and managing a
    /// backspace. Returns `RawInput::Line` only when Enter is pressed; callers
    /// pass the *same* `buf` each iteration so partial typing persists.
    pub fn poll_input(buf: &mut String) -> RawInput {
        let fd = io::stdin().as_raw_fd();
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pfd, 1, 0) } <= 0 {
            return RawInput::None;
        }
        let mut tmp = [0u8; 256];
        let mut nread = 0;
        loop {
            let r = unsafe {
                libc::read(
                    fd,
                    tmp.as_mut_ptr() as *mut libc::c_void,
                    tmp.len(),
                )
            };
            if r <= 0 {
                break;
            }
            nread = r as usize;
            break;
        }
        if nread == 0 {
            return RawInput::None;
        }
        let mut stdout = io::stdout();
        for &b in &tmp[..nread] {
            match b {
                0x0a | 0x0d => {
                    let line = std::mem::take(buf);
                    return RawInput::Line(line);
                }
                0x7f | 0x08 => {
                    if !buf.is_empty() {
                        buf.pop();
                        let _ = stdout.write_all(b"\x08 \x08");
                        let _ = stdout.flush();
                    }
                }
                0x03 => {
                    buf.clear();
                    return RawInput::Interrupt;
                }
                0x04 => return RawInput::Eof,
                0x1b => { /* ignore ESC sequences (arrows, etc.) */ }
                c if c >= 0x20 && c < 0x7f => {
                    buf.push(c as char);
                    let _ = stdout.write_all(&[c]);
                    let _ = stdout.flush();
                }
                _ => { /* ignore other control bytes */ }
            }
        }
        RawInput::None
    }
}
