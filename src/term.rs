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

static COLOR_OVERRIDE: Mutex<Option<bool>> = Mutex::new(None);

/// Providers, set once at startup, so the line-editor helper can offer
/// `/model` tab-completion.
static MODEL_PROVIDERS: OnceLock<Vec<crate::config::Provider>> = OnceLock::new();

/// Register the providers for `/model` tab-completion. Call once after
/// models.json has been loaded.
pub fn set_model_providers(providers: &[crate::config::Provider]) {
    let _ = MODEL_PROVIDERS.set(providers.to_vec());
}

/// Force colour on (`true`) or off (`false`). When unset (the default), colour
/// is decided automatically from the terminal and `NO_COLOR`. An explicit call
/// always overrides, so `--no-color` / forced colour win over the auto check.
pub fn set_color(on: bool) {
    if let Ok(mut g) = COLOR_OVERRIDE.lock() {
        *g = Some(on);
    }
}

fn color() -> bool {
    if let Ok(g) = COLOR_OVERRIDE.lock() {
        if let Some(v) = *g {
            return v;
        }
    }
    // Auto: colour only when stdout is a terminal and NO_COLOR is unset.
    io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// Whether stdout is attached to an interactive terminal (used by the
/// notification policy to decide whether the user is "watching").
pub fn is_terminal() -> bool {
    io::stdout().is_terminal()
}

/// True when ANSI colour is currently enabled (mirrors the private `color()`
/// predicate). Used by the TUI to colour conversation lines.
pub fn color_enabled() -> bool {
    color()
}

/// Whether highlighted text is rendered with a *transparent* background instead
/// of an opaque colour block. Runtime-toggle via [`set_transparent_highlight`];
/// default is opaque. A transparent highlight uses reverse video (`ESC[7m`),
/// which paints the current foreground colour as the background — so the
/// user's terminal theme shows *through* the highlight instead of an opaque
/// rectangle being drawn over whatever is already on screen (e.g. inside the
/// REPL's spinner block, or over a themed background). See [`highlight`].
static TRANSPARENT_HL: Mutex<bool> = Mutex::new(false);

/// Toggle transparent highlighting. Pass `true` for reverse-video (transparent)
/// highlights, `false` for an opaque colour block. Safe to call any time.
pub fn set_transparent_highlight(on: bool) {
    if let Ok(mut g) = TRANSPARENT_HL.lock() {
        *g = on;
    }
}

fn transparent_highlight() -> bool {
    *TRANSPARENT_HL.lock().unwrap_or_else(|e| e.into_inner())
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

/// Height of the terminal in rows (used by the `pir -r` session picker to size
/// its two panes). Falls back to 24 when the size can't be queried.
pub fn terminal_height() -> usize {
    #[cfg(unix)]
    {
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws as *mut libc::winsize) == 0
                && ws.ws_row > 0
            {
                return ws.ws_row as usize;
            }
        }
    }
    24
}

/// Clip `s` to at most `n` *visible* (ANSI-stripped) characters, appending a
/// `…` when truncated. Used by the session picker to fit preview text into a
/// column without splitting inside an escape sequence.
pub fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    if visible_len(s) <= n {
        return s.to_string();
    }
    // Truncate on visible-character boundaries.
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

/// Visible (ANSI-strip) length of a string.
pub fn visible_len(s: &str) -> usize {
    let mut len = 0;
    let mut esc = false;
    for c in s.chars() {
        if esc {
            if c == 'm' {
                esc = false;
            }
            continue;
        }
        if c == '\x1b' {
            esc = true;
            continue;
        }
        len += 1;
    }
    len
}

/// A one-line status bar rendered beneath the prompt, showing the active
/// Workspace (the current working directory, with `$HOME` collapsed to `~`) and
/// the model currently in use. Kept on a single dim line so the idle REPL is
/// self-describing without scrolling the conversation. Callers that change cwd
/// or switch model should re-render it (see the REPL loop / TUI).
pub fn status_line(workspace: &str, model: &str) -> String {
    let ws = dim(&format!("workspace: {workspace}"));
    let md = dim(&format!("model: {}", cyan(model)));
    format!("  {ws}   {md}")
}

fn paint(code: &str, s: &str) -> String {
    if color() { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
}

pub fn dim(s: &str) -> String { paint("2", s) }
pub fn bold(s: &str) -> String { paint("1", s) }
pub fn red(s: &str) -> String { paint("31", s) }
pub fn green(s: &str) -> String { paint("32", s) }
pub fn yellow(s: &str) -> String { paint("33", s) }
pub fn cyan(s: &str) -> String { paint("36", s) }

/// Render `s` as highlighted text. By default the highlight is an opaque
/// colour block (bright background, bold text) so it stands out on a plain
/// terminal. When transparent highlighting is enabled (see
/// [`set_transparent_highlight`]) it instead uses SGR reverse video (`ESC[7m`),
/// whose background is the terminal's *current* foreground colour — so the
/// highlight appears as an inverted sliver that lets the underlying theme/REPL
/// show through rather than an opaque rectangle being painted over whatever is
/// already on screen (e.g. inside the spinner block while a turn runs, or over
/// a themed background). With colour disabled both modes fall back to plain
/// text. The returned string always closes the SGR sequence so the rest of the
/// line is unaffected.
pub fn highlight(s: &str) -> String {
    if !color() {
        return s.to_string();
    }
    if transparent_highlight() {
        format!("\x1b[7m{s}\x1b[27m")
    } else {
        format!("\x1b[1;97;44m{s}\x1b[0m")
    }
}

pub fn read_answer(prompt: &str) -> String {
    eprint!("{prompt} ");
    let _ = io::stderr().flush();
    // No human on the other end (piped / scripted / closed stdin): never block
    // waiting for an answer that will never arrive. Callers treat the empty
    // string as their safe default (ask → deny), so this also fixes the old
    // "waits endlessly for stdin when there is no stdin" hang. While a turn is
    // running, stdin is in non-blocking raw mode, so `read_line` returns Err
    // and we fall through to the same default instantly.
    if !io::stdin().is_terminal() {
        return String::new();
    }
    let mut s = String::new();
    let _ = io::stdin().read_line(&mut s);
    s.trim().to_lowercase()
}

/// Read a line from the terminal with echo disabled (for secrets such as API
/// keys). Used by the `/login` command. On unix this briefly turns off the
/// terminal's `ECHO` flag (restoring it afterwards) so the key isn't shown as
/// the user types; on non-unix, or when the terminal state can't be fetched, it
/// falls back to a normal line read (which may echo). The prompt is printed to
/// stderr so it never pollutes piped stdout.
pub fn read_secret(prompt: &str) -> String {
    eprint!("{prompt} ");
    let _ = io::stderr().flush();
    #[cfg(unix)]
    unsafe {
        let fd = libc::STDIN_FILENO;
        let mut tios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut tios) == 0 {
            let mut raw = tios;
            raw.c_lflag &= !libc::ECHO;
            libc::tcsetattr(fd, libc::TCSANOW, &raw);
            let mut s = String::new();
            let _ = io::stdin().read_line(&mut s);
            libc::tcsetattr(fd, libc::TCSANOW, &tios);
            eprintln!();
            return s.trim().to_string();
        }
    }
    let mut s = String::new();
    let _ = io::stdin().read_line(&mut s);
    s.trim().to_string()
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

/// rustyline helper: offers `/model` argument tab-completion plus
/// slash-command-name completion (so `/def` → `/default-model`) and a live
/// preview (up to ten matching `provider/model` labels) of a typed prefix.
struct PirHelper;

impl rustyline::Helper for PirHelper {}

/// The known slash commands, used for command-name completion (the user can
/// type a `/`-prefix and Tab to complete it). Keeping this as the single source
/// of truth means new commands are auto-discoverable via completion.
const SLASH_COMMANDS: &[&str] = &[
    "/bg",
    "/cancel",
    "/clear",
    "/continue",
    "/create",
    "/default-model",
    "/exit",
    "/fix",
    "/fg",
    "/goal",
    "/help",
    "/jobs",
    "/login",
    "/logout",
    "/model",
    "/model*",
    "/models",
    "/project",
    "/rebuild",
    "/resume",
    "/sessions",
    "/sh",
    "/thinking",
    "/undo",
    "/unfinished",
    "/usage",
];

/// Brief per-command help, shown inline (as a hint to the right of the cursor)
/// the moment the user types a `/command` — mirroring pi's autocomplete
/// dropdown, which displays each command's `description` + `argumentHint`. The
/// `(name, argumentHint, description)` shape keeps it in lockstep with the
/// `SLASH_COMMANDS` list above; commands without an argument use `""`.
const SLASH_HELP: &[(&str, &str, &str)] = &[
    ("/bg", "<prompt>", "run a prompt as a background job"),
    ("/cancel", "", "stop the running turn now"),
    ("/clear", "", "clear the conversation history"),
    ("/continue", "", "resume the session and drive its next goal step"),
    ("/create", "[name]", "scaffold a new project from a clipboard spec"),
    ("/default-model", "<sel>", "set the default model for new sessions"),
    ("/exit", "", "quit pir"),
    ("/fix", "", "make the git setup LLM-safe (commit guard hook)"),
    ("/fg", "<id>", "bring a background job to the foreground"),
    ("/goal", "[objective]", "start or show the current goal"),
    ("/help", "", "show all commands"),
    ("/jobs", "", "list background jobs"),
    ("/login", "<provider>", "store an API key for a provider"),
    ("/logout", "<provider>", "remove a stored provider credential"),
    ("/model", "<sel>", "switch the model for this session"),
    ("/model*", "<sel>", "switch model in all open pir terminals"),
    ("/models", "", "list available models"),
    ("/project", "init", "create the ai_<project> user (root)"),
    ("/rebuild", "", "cargo build and exec the fresh binary"),
    ("/resume", "<idx|fragment>", "resume an unfinished session"),
    ("/sessions", "", "list recent sessions"),
    ("/sh", "[cmd args]", "drop to a shell, or run a command via $SHELL"),
    ("/thinking", "<level> [show|hide]", "set model thinking level / display"),
    ("/undo", "[all]", "revert the last file edit (or all)"),
    ("/unfinished", "", "list interrupted / still-running sessions"),
    ("/usage", "", "show token usage for this session"),
];

/// Look up a slash command's brief help for an inline hint. `line` is the text
/// up to the cursor. Returns the `argumentHint` + description for the best match
/// (a command that equals or is a strict prefix of the typed name), so typing
/// `/login` shows its help immediately, while `Other text` shows nothing.
fn command_help_hint(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let (cmd, has_arg) = match trimmed.split_once(char::is_whitespace) {
        Some((c, rest)) if c.starts_with('/') => (c, !rest.trim().is_empty()),
        None if trimmed.starts_with('/') => (trimmed, false),
        _ => return None,
    };
    if cmd.len() <= 1 {
        return None;
    }
    // Prefer an exact match; otherwise the command being typed (a prefix).
    let entry = SLASH_HELP
        .iter()
        .find(|(name, _, _)| *name == cmd)
        .or_else(|| SLASH_HELP.iter().find(|(name, _, _)| name.starts_with(cmd)));
    let (name, arg, desc) = entry?;
    if has_arg {
        // Once an argument exists, pi stops showing command help; keep it quiet
        // so the argument preview (e.g. a typed model prefix) is free to show.
        if arg.is_empty() {
            return None;
        }
        // Still typing the first argument: show the description as guidance.
        return Some(desc.to_string());
    }
    let mut s = String::new();
    if !arg.is_empty() {
        s.push_str(arg);
        s.push(' ');
    }
    s.push_str("— ");
    s.push_str(desc);
    // Suppress the hint for `/help` itself (its own description is the list).
    if *name == "/help" {
        return None;
    }
    Some(s)
}

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
        // No space yet: the user is still typing the command name itself
        // (e.g. `/def`). Offer any slash command that starts with what they've
        // typed, so `/default-<Tab>` completes to `/default-model`.
        if !rest.contains(char::is_whitespace) {
            if let Some(prefix) = cmd.strip_prefix('/') {
                let matches: Vec<String> = SLASH_COMMANDS
                    .iter()
                    .filter(|c| c[1..].starts_with(prefix))
                    .map(|c| c.to_string())
                    .collect();
                if matches.is_empty() {
                    return Ok((0, Vec::new()));
                }
                return Ok((start_idx, matches));
            }
            return Ok((0, Vec::new()));
        }
        if cmd == "/thinking" {
            let after = &rest[cmd_end..];
            if after.is_empty() {
                return Ok((0, Vec::new()));
            }
            let arg_lead = after.find(|c: char| !c.is_whitespace()).unwrap_or(after.len());
            let arg_start = start_idx + cmd_end + arg_lead;
            let prefix = &left[arg_start..];
            let opts = ["off", "minimal", "low", "medium", "high", "xhigh", "max", "show", "hide"];
            let matches: Vec<String> = opts
                .iter()
                .filter(|o| o.starts_with(prefix))
                .map(|o| o.to_string())
                .collect();
            return Ok((arg_start, matches));
        }
        if cmd != "/model" && cmd != "/m" && cmd != "/default-model" && cmd != "/dm" {
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
        // Brief per-command help: as soon as the user types a `/command` (e.g.
        // `/login`), show its argument hint + description inline to the right of
        // the cursor, mirroring pi's autocomplete dropdown. This takes priority
        // over the model-preview hint, which only applies to /model arguments.
        if let Some(help) = command_help_hint(line) {
            return Some(help);
        }
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
        if cmd != "/model" && cmd != "/m" && cmd != "/default-model" && cmd != "/dm" {
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
            let r = rl.load_history(&path);
            eprintln!("pir-debug load_history: path={:?} err={:?}", path, r.err());
        }
    });
}

fn save_history(rl: &mut Editor<PirHelper, rustyline::history::DefaultHistory>) {
    if let Some(path) = HISTORY_FILE.with(|f| f.borrow().clone()) {
        let _ = rl.save_history(&path);
    }
}

/// Return the currently-loaded line-editor history (most-recent-last order),
/// used by the TUI's idle prompt for arrow-up/down recall. Shares the same
/// `.history` file the streaming REPL loads into the rustyline editor, so the
/// TUI shows the exact same previous prompts (including those from before a
/// `pir -r` resume). Returns an empty vec when no history has been loaded yet.
pub fn load_history_lines() -> Vec<String> {
    let lines = EDITOR.with(|e| {
        let g = e.borrow();
        match g.as_ref() {
            Some(rl) => rl.history().iter().cloned().collect(),
            None => Vec::new(),
        }
    });
    eprintln!("pir-debug load_history_lines: {} entries", lines.len());
    lines
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
/// The spinner renders a single compact line while a turn runs:
///
/// ```text
/// ⠋ thinking… <what the user is typing live>
/// ```
///
/// The user's keystrokes (recorded into `typeahead` by the REPL thread) are
/// shown inline after the label so typing stays visible while the model thinks,
/// without a fragile multi-line block under the spinner. This is the **only**
/// thing that writes to stdout while it's alive; it redraws its one line in
/// place each tick and fully erases it on `stop()` — so a replaced spinner can
/// never leave a stray "thinking…" / "────" line behind (the old 3-line block
/// drifted on `\x1b[2A` line-arithmetic between tool rounds).
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
    /// that writes to stdout while it's alive, so it owns the single "thinking"
    /// line and renders the user's typing on it (inline after the label). This
    /// avoids two threads racing on stdout — the previous design had the main
    /// REPL thread echo keystrokes directly *and* the same line, which clobbered
    /// the user's input mid-thought (the "REPL doesn't display during thinking"
    /// bug).
    ///
    /// `quiet` is the shared "go silent" switch the REPL flips to background a
    /// running turn (bare `&`). When it is set, the spinner stops drawing
    /// (and erases whatever it last drew) so a detached turn doesn't keep
    /// writing its "thinking" block to the terminal behind the user's prompt.
    pub fn start(label: &str, typeahead: Arc<Mutex<String>>, enabled: bool) -> Spinner {
        Spinner::start_with(label, typeahead, enabled, Arc::new(AtomicBool::new(false)))
    }

    /// Like [`Spinner::start`], but also stops drawing when `quiet` is set (used
    /// by the agent, which passes its shared `quiet_req` so a turn detached
    /// mid-"thinking" silences the spinner immediately).
    pub fn start_with(
        label: &str,
        typeahead: Arc<Mutex<String>>,
        enabled: bool,
        quiet: Arc<AtomicBool>,
    ) -> Spinner {
        if !enabled {
            return Spinner { handle: None, alive: Arc::new(AtomicBool::new(false)) };
        }
        let alive = Arc::new(AtomicBool::new(true));
        let a = alive.clone();
        let q = quiet.clone();
        let label = label.to_string();
        let handle = thread::spawn(move || {
            let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let mut i = 0usize;
            let mut out = io::stdout();
            while a.load(Ordering::SeqCst) {
                // Detached turn: erase the spinner and go silent so a backgrounded
                // turn leaves a clean terminal behind it.
                if q.load(Ordering::SeqCst) {
                    let _ = out.write_all(b"\r\x1b[2K");
                    let _ = out.flush();
                    std::thread::sleep(Duration::from_millis(80));
                    continue;
                }
                let frame = if color() { format!("\x1b[36m{}\x1b[0m", frames[i % frames.len()]) } else { frames[i % frames.len()].to_string() };
                // Render the user's in-progress line (recorded by the REPL thread)
                // inline after the label, so typing stays visible while the model
                // thinks — without a fragile multi-line block under the spinner.
                // The old 3-line block (thinking + hrule + a fake `❯` prompt line)
                // drifted on \x1b[2A line-arithmetic between tool rounds, leaking
                // stray "thinking…" / "────" lines and clobbering the REPL prompt.
                let typed = typeahead.lock().map(|g| g.clone()).unwrap_or_default();
                // Single clean line: CR to column 0, erase the whole line, then
                // rewrite. One line means the erase/redraw can never drift, so no
                // lines accumulate. `\x1b[2K` clears the entire line.
                let mut line = format!("\r\x1b[2K{frame} {label}…");
                if !typed.is_empty() {
                    line.push_str("  ");
                    line.push_str(&typed);
                }
                let _ = out.write_all(line.as_bytes());
                let _ = out.flush();
                std::thread::sleep(Duration::from_millis(80));
                i = i.wrapping_add(1);
            }
            // On stop, erase the spinner line and leave the cursor at column 0 so
            // the next output (streamed model text) starts cleanly on that line.
            let _ = out.write_all(b"\r\x1b[2K");
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
    use std::io::{self, Write};
    use std::os::unix::io::AsRawFd;
    use std::sync::{Arc, Mutex};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    /// Millisecond timestamp (process-monotonic via `SystemTime`) of the most
    /// recent keystroke the raw reader saw — any printable char, backspace,
    /// Enter, or control key. Shared with the agent's thinking-stream callback
    /// so streamed reasoning can be *deferred* while the user is actively
    /// typing: the thinking text is held back until the keyboard has been idle
    /// for a moment (see `KEYBOARD_IDLE_BEFORE_THINKING_MS`), so the model's
    /// reasoning never wipes the in-progress line the user is typing.
    static LAST_KEY_MILLIS: AtomicU64 = AtomicU64::new(0);

    /// How long the keyboard must be idle (no keypress) before streamed
    /// "thinking" output is allowed to print. 1s: a typing burst isn't
    /// interrupted, and a short pause is all it takes for reasoning to appear.
    pub const KEYBOARD_IDLE_BEFORE_THINKING_MS: u64 = 1000;

    fn now_millis() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Record a keystroke (called by the raw reader on any input event).
    pub fn note_keypress() {
        LAST_KEY_MILLIS.store(now_millis(), Ordering::SeqCst);
    }

    /// Milliseconds since the last keystroke (u64::MAX if none yet this
    /// process, so "no keys ever" counts as fully idle).
    pub fn millis_since_keypress() -> u64 {
        let last = LAST_KEY_MILLIS.load(Ordering::SeqCst);
        if last == 0 {
            return u64::MAX;
        }
        now_millis().saturating_sub(last)
    }

    /// True when the keyboard has been idle for at least
    /// [`KEYBOARD_IDLE_BEFORE_THINKING_MS`] (or no key was ever pressed).
    pub fn keyboard_idle_long_enough() -> bool {
        millis_since_keypress() >= KEYBOARD_IDLE_BEFORE_THINKING_MS
    }

    /// Test hook: pretend the keyboard is idle (clears the last-key clock).
    #[cfg(test)]
    pub(crate) fn reset_keypress_clock() {
        LAST_KEY_MILLIS.store(0, Ordering::SeqCst);
    }

    /// Upper bound on one `wait_input` block, in milliseconds. `readable()` on
    /// a pipe at EOF fires immediately *forever*, so without a bounded wait the
    /// REPL would busy-spin at 100% CPU when stdin is closed while a turn never
    /// signals completion. The timer arm makes the wait return `None` at least
    /// this often; the caller re-loops, so typing stays responsive.
    pub(crate) const INPUT_POLL: u64 = 80;

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

    /// Whether raw (non-canonical, non-blocking) mode is currently active. Used
    /// by `/sh` to restore exactly the terminal state it found before dropping
    /// to a child shell.
    pub fn is_active() -> bool {
        STATE.lock().unwrap().active
    }

    /// Enable bracketed-paste mode (`ESC [ ? 2 0 0 4 h`). While this is on, the
    /// terminal wraps any pasted text in `ESC [ 2 0 0 ~ … ESC [ 2 0 1 ~`, so we
    /// can tell pasted newlines apart from the user pressing Enter. Without it,
    /// a *pasted* multiline block (which arrives in raw mode carrying literal
    /// `\n` bytes) would be split on every line into a separate queued prompt —
    /// the bug this guards against. Paired with `disable_bracketed_paste` in
    /// `disable_raw`. Written directly (not via crossterm) so it works in the
    /// streaming REPL's hand-rolled raw mode too. Best-effort; ignore errors.
    fn enable_bracketed_paste() {
        let _ = io::stdout().write_all(b"\x1b[?2004h");
        let _ = io::stdout().flush();
    }

    /// Disable bracketed-paste mode (`ESC [ ? 2 0 0 4 l`).
    fn disable_bracketed_paste() {
        let _ = io::stdout().write_all(b"\x1b[?2004l");
        let _ = io::stdout().flush();
    }

    /// Put stdin into raw, non-blocking mode (no canonical line editing, no
    /// echo, reads return immediately). Idempotent. No-op when raw mode is
    /// disabled (`--no-raw`). Enables bracketed-paste mode as well so pasted
    /// multiline text is delivered inside an `ESC[200~…ESC[201~` wrapper (see
    /// `read_chunk`/`translate`), instead of being split line-by-line into
    /// multiple queued prompts.
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
        enable_bracketed_paste();
    }

    /// Restore the previous terminal attributes and blocking mode. Idempotent.
    /// Also turns bracketed-paste mode back off so a pasted block in a different
    /// program (or the shell after /sh) behaves normally.
    pub fn disable_raw() {
        if !*ENABLED.lock().unwrap() {
            return;
        }
        let mut st = STATE.lock().unwrap();
        if !st.active {
            return;
        }
        disable_bracketed_paste();
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
    #[derive(Debug, PartialEq)]
    pub enum RawInput {
        /// Nothing was available (caller should pump notifications and retry).
        None,
        /// A full line was submitted (Enter). The buffer is consumed.
        Line(String),
        /// ctrl-c: caller should request cancellation of the running turn.
        Interrupt,
        /// A lone Escape key: caller should request cancellation of the running
        /// turn (treated like ctrl-c). An Escape that is the lead byte of an
        /// arrow/function-key sequence is *not* a lone Escape — disambiguated
        /// in `read_chunk` by peeking at the byte after `0x1b`.
        Cancel,
        /// ctrl-d: caller should stop the session.
        Eof,
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
    ///
    /// The wait is throttled so an EOF pipe can't busy-spin. A pipe at EOF is
    /// *permanently* "readable" (a closed fd wakes the reactor forever), so
    /// racing only `readable` against the turn-completion channel would spin at
    /// ~100% CPU whenever stdin is closed while the worker never signals
    /// completion (e.g. it is parked in a retry backoff) — the runaway-CPU bug
    /// we saw. After draining, if no real input arrived and barely any wall time
    /// elapsed, we sleep the rest of the [`INPUT_POLL`] window so the idle REPL
    /// stays near 0% CPU; genuine input returns instantly.
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
        let start = std::time::Instant::now();
        let readable = async { let _ = stdin.readable().await; };
        let finished = async { let _ = done.recv().await; };
        smol::block_on(smol::future::or(readable, finished));
        // Either side fired (or stdin closed): drain whatever is buffered.
        let result = read_chunk(buf, typeahead);
        // Throttle the EOF case. A pipe at EOF is *permanently* readable (a
        // closed fd wakes the reactor immediately, forever), and the turn never
        // signals completion while it's parked in a retry backoff — so racing
        // only `readable` against `finished` would busy-spin at ~100% CPU (that
        // was the runaway-CPU bug). When there's no actual input and barely any
        // wall time elapsed, sleep the rest of the poll window so the idle REPL
        // stays near 0% CPU. Real input (a `Line`/control key) returns instantly.
        if matches!(result, RawInput::None) {
            let elapsed = start.elapsed();
            let target = Duration::from_millis(INPUT_POLL);
            if elapsed < target {
                std::thread::sleep(target - elapsed);
            }
        }
        result
    }

    /// Drain any currently-available stdin bytes, translating control chars and
    /// recording printable text into `typeahead` (for the spinner to render).
    /// `stdin` is non-blocking (see `enable_raw`), so this returns as soon as no
    /// more bytes are readable. Backspace pops the buffer; ctrl-c/ctrl-d/Enter
    /// are surfaced to the caller. This thread never writes to stdout.
    ///
    /// Bracketed-paste aware: while a `ESC [ 2 0 0 ~ … ESC [ 2 0 1 ~` paste
    /// wrapper is open, embedded newlines are kept as part of the line (as
    /// `'\n'`) instead of being interpreted as Enter — so a *pasted* multiline
    /// block becomes a single queued prompt rather than one prompt per line.
    fn read_chunk(buf: &mut String, typeahead: &Arc<Mutex<String>>) -> RawInput {
        let fd = io::stdin().as_raw_fd();
        let mut tmp = [0u8; 256];
        let mut nread = 0usize;
        // stdin is non-blocking (see enable_raw), so read may return a partial
        // chunk; keep draining until it would block (r <= 0) or the buffer is
        // full. Without the loop a single read could leave bytes unconsumed.
        // Crucially, this loop drains the *entire* paste (which the terminal
        // delivers as a single write) in one `read_chunk` call, so the local
        // `pasting` flag below stays valid for the whole wrapper.
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
        // A real input event arrived: mark the keyboard active so deferred
        // thinking output keeps waiting (see `note_keypress`).
        note_keypress();
        // `pasting` tracks whether we're inside a bracketed-paste wrapper
        // (`ESC[200~` … `ESC[201~`). It is local because `read_chunk` always
        // drains every available byte (above), so a whole paste is consumed
        // within a single call.
        let mut pasting = false;
        // Process bytes sequentially with an index so we can look *ahead* at the
        // byte following an ESC (needed to tell a lone Esc from the lead of an
        // arrow/function-key sequence). `read_chunk` already drained every byte
        // that was available this tick into `tmp`, so a follow-up byte that
        // arrived in the SAME tick is right here in the buffer — no second fd
        // read is required for the common case.
        let mut i = 0usize;
        while i < nread {
            let b = tmp[i];
            match b {
                0x0a => {
                    // LF. Outside a paste this ends the line (Enter → a queued
                    // prompt). Inside a paste it's part of the pasted text, so we
                    // keep it as a real newline in the buffer.
                    if pasting {
                        buf.push('\n');
                        update_typeahead(buf, typeahead);
                    } else {
                        let line = std::mem::take(buf);
                        return RawInput::Line(line);
                    }
                }
                0x0d => {
                    // CR. Outside a paste this also ends the line. Inside a paste
                    // we normalise CRLF→LF: skip a CR that is immediately followed
                    // by an LF (the LF will add the newline); otherwise keep it.
                    if pasting {
                        if !(i + 1 < nread && tmp[i + 1] == 0x0a) {
                            buf.push('\n');
                            update_typeahead(buf, typeahead);
                        }
                    } else {
                        let line = std::mem::take(buf);
                        return RawInput::Line(line);
                    }
                }
                0x7f | 0x08 => {
                    if !buf.is_empty() {
                        buf.pop();
                        // Update the shared typeahead so the spinner drops the
                        // removed character (it owns the only stdout writer).
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
                    // ctrl-c), but it's also the lead byte of a CSI sequence
                    // (arrows, Home/End, F-keys: `0x1b 0x5b …`; and the bracketed
                    // paste wrappers `ESC[200~` / `ESC[201~`). Disambiguate on
                    // the byte that follows this one:
                    //   * if the next byte is `0x5b` (`[`) it's a CSI sequence;
                    //   * otherwise it's a lone Esc → cancel the turn.
                    // The next byte is almost always already in `tmp` (the whole
                    // sequence arrives in one terminal write); only when `0x1b`
                    // is the final buffered byte do we peek the fd briefly.
                    let next_is_csi = if i + 1 < nread {
                        tmp[i + 1] == 0x5b
                    } else {
                        matches!(read_byte_timeout(fd, std::time::Duration::from_millis(25)), Some(0x5b))
                    };
                    if !next_is_csi {
                        buf.clear();
                        if let Ok(mut g) = typeahead.lock() {
                            g.clear();
                        }
                        return RawInput::Cancel;
                    }
                    // We're in a CSI sequence. Check whether it's a bracketed-
                    // paste wrapper (`ESC[200~` starts, `ESC[201~` ends).
                    if let Some(start) = paste_marker_at(&tmp[..nread], i) {
                        // It's `ESC[200~` (start) or `ESC[201~` (end). Consume
                        // the whole wrapper (`ESC [ 2 0 0 ~` = 6 bytes including
                        // the 0x1b at index `i`) and flip the `pasting` flag. The
                        // body between them keeps being literal text (newlines
                        // included). `continue` skips the loop's trailing `i += 1`.
                        pasting = start; // `200~` = start
                        i += 6;
                        continue;
                    }
                    // Ordinary CSI sequence (arrows, Home/End, F-keys): swallow
                    // the parameter/terminator bytes. Consume from `tmp` first
                    // (the bulk of the sequence arrived in this same batch) up to
                    // its terminator (an alphabetic byte or `~`), then top up any
                    // tail still in flight on the fd. We MUST consume the buffered
                    // body here — otherwise those bytes would fall through to the
                    // printable-ASCII arm and corrupt `buf`.
                    i += 1; // skip `0x1b`
                    if i < nread && tmp[i] == 0x5b {
                        i += 1; // skip `0x5b` if it was buffered
                    }
                    while i < nread {
                        let b = tmp[i];
                        i += 1;
                        if b.is_ascii_alphabetic() || b == b'~' {
                            break; // terminator consumed
                        }
                    }
                    if i >= nread {
                        // The terminator wasn't in this batch; the rest is on the
                        // fd. Top up (bounded) so it doesn't leak into the next poll.
                        drain_csi_sequence(fd);
                    }
                    // `i` now sits just past the consumed sequence (or at `nread`);
                    // the outer `while` advances it once more, which is correct.
                }
                c if c >= 0x20 && c < 0x7f => {
                    buf.push(c as char);
                    update_typeahead(buf, typeahead);
                }
                _ => { /* ignore other control bytes */ }
            }
            i += 1;
        }
        RawInput::None
    }

    /// Mirror `buf` into the shared `typeahead` so the spinner/cursor reflects
    /// the current draft (including any pasted newlines). Single small helper
    /// so the three byte arms that mutate `buf` stay in sync.
    fn update_typeahead(buf: &str, typeahead: &Arc<Mutex<String>>) {
        if let Ok(mut g) = typeahead.lock() {
            g.clear();
            g.push_str(buf);
        }
    }

    /// True when the bytes at `idx` (in `buf`, where `buf[idx] == 0x1b`) begin a
    /// bracketed-paste wrapper. `pub(crate)` so the unit tests can exercise it
    /// directly.
    pub(crate) fn paste_marker_at(buf: &[u8], idx: usize) -> Option<bool> {
        // buf[idx] == 0x1b, buf[idx+1] == 0x5b (`[`), then "2 0 0 ~" / "2 0 1 ~".
        // Both wrappers terminate in `~`; they differ in the *third* parameter
        // byte (`0` = start, `1` = end). So: buf[a]=='2', buf[a+1]=='0',
        // buf[a+2] in {'0','1'}, buf[a+3]=='~'.
        let a = idx + 2; // first parameter byte ('2')
        if a + 3 < buf.len()
            && buf[a] == b'2'
            && buf[a + 1] == b'0'
            && (buf[a + 2] == b'0' || buf[a + 2] == b'1')
            && buf[a + 3] == b'~'
        {
            Some(buf[a + 2] == b'0') // `200~` = start
        } else {
            None
        }
    }

    /// Read a single byte from the (already non-blocking) fd, polling up to
    /// `timeout` so we can tell a lone Esc from the start of a CSI sequence
    /// without blocking forever. Returns `None` on timeout/EOF. EAGAIN/EINTR are
    /// retried until the deadline, since the fd is non-blocking.
    fn read_byte_timeout(fd: libc::c_int, timeout: std::time::Duration) -> Option<u8> {
        let start = std::time::Instant::now();
        let mut b: u8 = 0;
        loop {
            let r = unsafe { libc::read(fd, &mut b as *mut u8 as *mut libc::c_void, 1) };
            if r == 1 {
                return Some(b);
            }
            if r == 0 {
                return None; // EOF
            }
            // r < 0 (EAGAIN/EINTR): wait until the deadline, then give up.
            if start.elapsed() >= timeout {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Consume the remainder of a CSI escape sequence (after the `0x1b 0x5b`
    /// lead-in) so its bytes don't leak into the next input poll. CSI sequences
    /// terminate on an alphabetic byte (e.g. `A`–`D` for arrows, `H`/`F` for
    /// Home/End) or `~` (F-keys / modified keys); we read with the already
    /// non-blocking fd until we hit a terminator or a short deadline cap.
    fn drain_csi_sequence(fd: libc::c_int) {
        let mut buf = [0u8; 32];
        let mut n = 0usize;
        let start = std::time::Instant::now();
        loop {
            let r = unsafe { libc::read(fd, buf.as_mut_ptr().add(n) as *mut libc::c_void, 1) };
            if r == 1 {
                let b = buf[n];
                n += 1;
                if b.is_ascii_alphabetic() || b == b'~' {
                    break; // sequence terminated
                }
                if n >= buf.len() {
                    break; // absurdly long; stop to avoid spinning
                }
            } else if r <= 0 {
                // EAGAIN/EINTR/EOF: give a brief grace period in case the tail
                // of the sequence is still in flight, then stop.
                if start.elapsed() >= std::time::Duration::from_millis(25) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
    }

    /// Shared control-character translation used by both the streaming REPL
    /// (`read_chunk`) and the TUI's raw input reader, so the two front-ends
    /// never diverge in how Enter / ctrl-c / ctrl-d / esc / backspace are
    /// interpreted. `bytes` is the chunk just read from fd 0; `buf` accumulates
    /// the current line and `typeahead` mirrors it for the spinner/footer. ESC
    /// handling mirrors `read_chunk`'s CSI-aware disambiguation: when a `0x1b`
    /// is followed by `0x5b` (`[`) it is a CSI sequence (arrows, Home/End, F-keys,
    /// or a bracketed-paste wrapper `ESC[200~…ESC[201~`) and is swallowed; a
    /// lone Esc → cancel. While a bracketed-paste wrapper is open, embedded
    /// newlines are kept as part of the line (a real `'\n'`) instead of ending
    /// it, so a pasted multiline block becomes a single prompt. Because the TUI
    /// buffers a whole chunk before calling, the bytes following a `0x1b` are
    /// already present in `bytes`, so no second fd read is needed in the common
    /// case.
    pub fn translate(buf: &mut String, typeahead: &Arc<Mutex<String>>, bytes: &[u8]) -> RawInput {
        let mut pasting = false;
        let mut i = 0usize;
        while i < bytes.len() {
            let fd = io::stdin().as_raw_fd();
            let b = bytes[i];
            match b {
                0x0a => {
                    if pasting {
                        buf.push('\n');
                        update_typeahead(buf, typeahead);
                    } else {
                        let line = std::mem::take(buf);
                        return RawInput::Line(line);
                    }
                }
                0x0d => {
                    if pasting {
                        if !(i + 1 < bytes.len() && bytes[i + 1] == 0x0a) {
                            buf.push('\n');
                            update_typeahead(buf, typeahead);
                        }
                    } else {
                        let line = std::mem::take(buf);
                        return RawInput::Line(line);
                    }
                }
                0x7f | 0x08 => {
                    if !buf.is_empty() {
                        buf.pop();
                        update_typeahead(buf, typeahead);
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
                0x1a => return RawInput::Suspend,
                0x1b => {
                    let next_is_csi = if i + 1 < bytes.len() {
                        bytes[i + 1] == 0x5b
                    } else {
                        matches!(read_byte_timeout(fd, std::time::Duration::from_millis(25)), Some(0x5b))
                    };
                    if !next_is_csi {
                        buf.clear();
                        if let Ok(mut g) = typeahead.lock() {
                            g.clear();
                        }
                        return RawInput::Cancel;
                    }
                    // We're in a CSI sequence; check for the bracketed-paste
                    // wrapper (`ESC[200~` start / `ESC[201~` end) before the
                    // generic swallow below. NOTE: the loop has already done
                    // `i += 1` past the 0x1b, so the wrapper starts at `i - 1`.
                    if let Some(start) = paste_marker_at(bytes, i) {
                        pasting = start; // `200~` = start
                        i += 6; // skip `ESC [ 2 0 0 ~` / `ESC [ 2 0 1 ~`
                        continue;
                    }
                    i += 1; // skip 0x1b
                    if i < bytes.len() && bytes[i] == 0x5b {
                        i += 1; // skip 0x5b
                    }
                    while i < bytes.len() {
                        let c = bytes[i];
                        i += 1;
                        if c.is_ascii_alphabetic() || c == b'~' {
                            break;
                        }
                    }
                    if i >= bytes.len() {
                        drain_csi_sequence(fd);
                    }
                }
                c if c >= 0x20 && c < 0x7f => {
                    buf.push(c as char);
                    update_typeahead(buf, typeahead);
                }
                _ => { /* ignore other control bytes */ }
            }
            i += 1;
        }
        RawInput::None
    }

    /// Put stdin into raw, non-blocking mode for the `pir -r` session picker
    /// specifically. Unlike [`enable_raw`] (used by the running-turn input),
    /// this does NOT enable bracketed-paste and uses a *separate* termios save
    /// slot so the picker's raw session never interacts with the REPL's.
    /// Idempotent; pair with [`disable_raw_picker`].
    pub fn enable_raw_picker() {
        let mut st = PICKER_STATE.lock().unwrap();
        if st.active {
            return;
        }
        unsafe {
            let fd = io::stdin().as_raw_fd();
            let mut tios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut tios) == 0 {
                st.orig_termios = Some(tios);
                let mut raw = tios;
                // No ISIG so ctrl-c/ctrl-z arrive as raw bytes (we handle them
                // ourselves rather than letting them raise a signal).
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

    /// Restore the terminal attributes saved by [`enable_raw_picker`].
    pub fn disable_raw_picker() {
        let mut st = PICKER_STATE.lock().unwrap();
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

    /// Expose [`translate`] under a picker-friendly name. The session picker
    /// drives stdin in raw mode and only needs the shared CSI-aware translation
    /// (arrows, ctrl-c/ctrl-d, lone Esc). Kept as an alias so the picker call
    /// site reads clearly.
    pub fn translate_picker(buf: &mut String, typeahead: &Arc<Mutex<String>>, bytes: &[u8]) -> RawInput {
        translate(buf, typeahead, bytes)
    }

    /// Separate termios save slot for the picker, so enabling it can never
    /// disturb the REPL's running-turn raw state stored in [`STATE`].
    static PICKER_STATE: Mutex<RawState> = Mutex::new(RawState {
        orig_termios: None,
        orig_nonblock: None,
        active: false,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustyline::history::MemHistory;

    // `Context` borrows a `History`; the helper ignores it, so an empty
    // in-memory history (kept in a static so it outlives the `Context`) is fine.
    fn ctx() -> Context<'static> {
        static HISTORY: std::sync::OnceLock<MemHistory> = std::sync::OnceLock::new();
        let h = HISTORY.get_or_init(MemHistory::new);
        Context::new(h)
    }

    // `/default-` (no trailing space yet) must complete to `/default-model`,
    // not fall through as "no completion". This is the regression from the
    // "slash commands don't autocomplete" report.
    #[test]
    fn completes_slash_command_name_without_space() {
        let h = PirHelper;
        let (start, mut matches) = h.complete("/default-", "/default-".len(), &ctx()).unwrap();
        matches.sort();
        assert_eq!(matches, vec!["/default-model"]);
        assert_eq!(start, 0);
    }

    #[test]
    fn completes_unique_slash_prefix() {
        let h = PirHelper;
        let (_start, matches) = h.complete("/mod", "/mod".len(), &ctx()).unwrap();
        assert!(matches.contains(&"/model".to_string()));
        assert!(matches.contains(&"/models".to_string()));
    }

    #[test]
    fn no_command_completion_when_space_present() {
        // Once a space follows the command, command-name completion must not
        // kick in (argument completion takes over). With providers set (by
        // sibling tests) a non-matching argument yields an empty match list
        // rather than any slash-command-name suggestions.
        let h = PirHelper;
        let (_start, matches) = h
            .complete("/default-model zzznomatch ", "/default-model zzznomatch ".len(), &ctx())
            .unwrap();
        assert!(matches.is_empty());
    }

    // `default-model` must offer the same model argument completion as `/model`,
    // so picking a default is as easy as switching the live model. Regression
    // guard for "default-model should have the same completion as /model".
    #[test]
    fn default_model_completes_like_model() {
        use crate::config::{Model, Provider};
        let providers = vec![
            Provider {
                id: Some("openai".into()),
                name: None,
                base_url: Some("https://api.openai.com/v1".into()),
                api_key: None,
                api: Some("openai".into()),
                models: vec![Model {
                    id: "gpt-fake".into(),
                    name: Some("GPT Fake".into()),
                    context: None,
                    max_tokens: None,
                    api_override: None,
                    url_override: None,
                    no_reasoning_effort: false,
                    price_per_1k: None,
                }],
            },
            Provider {
                id: Some("anthropic".into()),
                name: None,
                base_url: Some("https://api.anthropic.com/v1".into()),
                api_key: None,
                api: Some("anthropic".into()),
                models: vec![Model {
                    id: "claude-fake".into(),
                    name: Some("Claude Fake".into()),
                    context: None,
                    max_tokens: None,
                    api_override: None,
                    url_override: None,
                    no_reasoning_effort: false,
                    price_per_1k: None,
                }],
            },
        ];
        crate::term::set_model_providers(&providers);
        let h = PirHelper;
        let (start, mut matches) = h
            .complete("/default-model claude", "/default-model claude".len(), &ctx())
            .unwrap();
        matches.sort();
        assert_eq!(matches, vec!["claude-fake".to_string()]);
        // The completion replaces only the argument (after the command + space).
        assert_eq!(start, "/default-model ".len());
    }
}

#[cfg(test)]
mod command_help_hint_tests {
    use super::*;

    // Typing `/login` (a known command, no argument yet) should surface its
    // brief help inline, mirroring pi's autocomplete dropdown hint.
    #[test]
    fn shows_help_for_known_command() {
        let hint = command_help_hint("/login");
        let h = hint.expect("expected a hint for /login");
        assert!(h.contains("<provider>"), "got: {h:?}");
        assert!(h.contains("store an API key"), "got: {h:?}");
    }

    // A command being typed as a prefix should still match the brief help.
    #[test]
    fn shows_help_for_prefix() {
        let hint = command_help_hint("/log");
        assert!(hint.is_some(), "expected help for the /log* prefix");
    }

    // Non-command text (e.g. a normal prompt) must yield no hint.
    #[test]
    fn no_help_for_plain_text() {
        assert!(command_help_hint("fix the parser").is_none());
        assert!(command_help_hint("").is_none());
    }

    // `/help` itself is suppressed (its description is the command list); an
    // argument-bearing command shows only the description once an arg is typed.
    #[test]
    fn help_self_suppressed_but_other_args_show_description() {
        assert!(command_help_hint("/help").is_none());
        let h = command_help_hint("/login openai").expect("expected description");
        assert!(h.contains("store an API key"), "got: {h:?}");
    }
}

#[cfg(test)]
mod paste_tests {
    use super::raw::{paste_marker_at, translate, RawInput};
    use std::sync::{Arc, Mutex};

    // A closed-pipe stdin must NOT busy-spin. `read_chunk` (and thus the
    // `wait_input` throttle path) must not spin; this guards the runaway-CPU
    // bug where a pipe at EOF made `smol::Async::readable()` fire immediately
    // forever while the turn never signalled completion.
    #[test]
    fn raw_input_poll_const_is_bounded() {
        // The throttle window is deliberately short (milliseconds), not 0.
        assert!(super::raw::INPUT_POLL > 0, "INPUT_POLL must be > 0");
        assert!(super::raw::INPUT_POLL <= 250, "INPUT_POLL too large would lag typing");
    }

    fn ta() -> Arc<Mutex<String>> {
        Arc::new(Mutex::new(String::new()))
    }

    // A pasted multiline block is delivered wrapped in `ESC[200~ … ESC[201~`.
    // We must keep the embedded newlines as part of ONE prompt, not split it
    // into one prompt per line. Regression guard for "pasting multiline text
    // while a turn runs queues multiple prompts".
    #[test]
    fn paste_wrapped_multiline_is_single_line() {
        let bytes: Vec<u8> = [
            b'\x1b', b'[', b'2', b'0', b'0', b'~',
            b'l', b'i', b'n', b'e', b' ', b'o', b'n', b'e',
            b'\n',
            b'l', b'i', b'n', b'e', b' ', b't', b'w', b'o',
            b'\n',
            b'l', b'i', b'n', b'e', b' ', b't', b'h', b'r', b'e', b'e',
            b'\x1b', b'[', b'2', b'0', b'1', b'~',
        ]
        .to_vec();
        let mut buf = String::new();
        let r = translate(&mut buf, &ta(), &bytes);
        assert_eq!(r, RawInput::None, "paste must NOT end the line early");
        assert_eq!(buf, "line one\nline two\nline three");
    }

    // A lone Enter (outside any paste) still ends the line as before.
    #[test]
    fn bare_enter_ends_line() {
        let bytes = b"hello\n".to_vec();
        let mut buf = String::new();
        let r = translate(&mut buf, &ta(), &bytes);
        assert_eq!(r, RawInput::Line("hello".to_string()));
    }

    // CRLF inside a paste should normalise to a single LF, not two.
    #[test]
    fn paste_crlf_normalised() {
        let bytes: Vec<u8> = [
            b'\x1b', b'[', b'2', b'0', b'0', b'~',
            b'a', b'\r', b'\n', b'b',
            b'\x1b', b'[', b'2', b'0', b'1', b'~',
        ]
        .to_vec();
        let mut buf = String::new();
        let r = translate(&mut buf, &ta(), &bytes);
        assert_eq!(r, RawInput::None);
        assert_eq!(buf, "a\nb");
    }

    // `paste_marker_at` must only match the bracketed-paste wrappers, not an
    // ordinary CSI sequence like an arrow key (`ESC [ A`).
    #[test]
    fn only_paste_markers_are_detected() {
        assert_eq!(paste_marker_at(&[b'\x1b', b'[', b'2', b'0', b'0', b'~'], 0), Some(true));
        assert_eq!(paste_marker_at(&[b'\x1b', b'[', b'2', b'0', b'1', b'~'], 0), Some(false));
        assert_eq!(paste_marker_at(&[b'\x1b', b'[', b'A'], 0), None);
    }
}

#[cfg(test)]
mod status_line_tests {
    use super::*;
    #[test]
    fn status_line_shows_workspace_and_model() {
        // Colour must be forced off so the escape sequences don't break the
        // plain substring match — otherwise this test's result depends on
        // whether stdout happens to be a TTY in the runner environment.
        set_color(false);
        let s = status_line("/home/me/project", "anthropic/claude");
        assert!(s.contains("workspace: /home/me/project"));
        assert!(s.contains("model: anthropic/claude"));
        // With colour on, the model name must still survive inside its ANSI
        // wrapper (this is the branch that used to make the test env-sensitive).
        set_color(true);
        let s2 = status_line("/home/me/project", "anthropic/claude");
        assert!(s2.contains("workspace: /home/me/project"));
        assert!(s2.contains("model: "), "label must be present even when coloured: {s2:?}");
        assert!(s2.contains("anthropic/claude"), "model must be visible even when coloured: {s2:?}");
    }
}

#[cfg(test)]
mod highlight_tests {
    use super::*;

    // Drive the shared colour switch so these tests are deterministic.
    fn set_color_for_test(on: bool) {
        set_color(on);
    }

    // With colour on, `highlight` must produce ANSI-wrapped text and the
    // SGR sequence must be *closed* so the rest of the line is unaffected.
    #[test]
    fn highlight_emits_sgr_and_closes_it() {
        set_color_for_test(true);
        let hl = highlight("TODO");
        assert!(hl.starts_with("\x1b["), "expected an SGR open sequence: {hl:?}");
        assert!(hl.ends_with("\x1b[0m") || hl.ends_with("\x1b[27m"),
            "highlight must reset SGR at the end: {hl:?}");
        // The logical text survives inside the escapes.
        let inner = hl.trim_matches(|c: char| c == '\x1b' || c == '[' || c == 'm' || c.is_ascii_digit())
            .to_string();
        assert!(inner.contains("TODO"), "highlight must preserve the text: {hl:?}");
    }

    // Default (opaque) highlight uses an opaque colour block escape, NOT the
    // reverse-video escape — so a solid rectangle is drawn over whatever is
    // behind it.
    #[test]
    fn default_highlight_is_opaque_block() {
        set_color_for_test(true);
        set_transparent_highlight(false);
        let hl = highlight("x");
        assert!(hl.contains("44m"), "opaque highlight should set a background colour (44m): {hl:?}");
        assert!(!hl.contains("\x1b[7m"), "opaque highlight must NOT use reverse video: {hl:?}");
    }

    // Transparent highlight uses SGR reverse video (ESC[7m…ESC[27m) whose
    // background is the terminal's *current* foreground colour — i.e. it lets
    // whatever is already on screen (the theme / REPL) show through, instead of
    // painting an opaque rectangle. This is exactly the "transparent highlight"
    // behaviour: no fixed background colour escape, only the reverse flag.
    #[test]
    fn transparent_highlight_uses_reverse_video_not_opaque_block() {
        set_color_for_test(true);
        set_transparent_highlight(true);
        let hl = highlight("x");
        assert!(hl.contains("\x1b[7m"), "transparent highlight must use reverse video (ESC[7m): {hl:?}");
        assert!(hl.contains("\x1b[27m"), "transparent highlight must reset reverse video (ESC[27m): {hl:?}");
        assert!(!hl.contains("44m"), "transparent highlight must NOT set an opaque background colour: {hl:?}");
        // And it still carries the original text.
        assert!(hl.contains('x'), "transparent highlight must preserve text: {hl:?}");
    }

    // With colour disabled (e.g. piped output), `highlight` must pass the text
    // through untouched sequences), regardless of the transparent
    // toggles.
    #[test]
    fn highlight_no_color_is_plain() {
        set_color_for_test(false);
        set_transparent_highlight(true);
        assert_eq!(highlight("TODO"), "TODO");
        set_transparent_highlight(false);
        assert_eq!(highlight("TODO"), "TODO");
    }
}

#[cfg(test)]
mod keyboard_idle_tests {
    use super::raw::*;
    use std::sync::Mutex;

    /// The last-keypress clock is process-global; serialise these tests so a
    /// parallel test's `note_keypress()` can't flake another's assertions.
    static CLOCK_LOCK: Mutex<()> = Mutex::new(());

    // No keypress ever recorded in this process => the keyboard counts as fully
    // idle, so streamed thinking prints immediately.
    #[test]
    fn no_keypress_counts_as_idle() {
        let _g = CLOCK_LOCK.lock().unwrap();
        reset_keypress_clock();
        assert!(keyboard_idle_long_enough());
        assert!(millis_since_keypress() >= KEYBOARD_IDLE_BEFORE_THINKING_MS);
    }

    // A fresh keypress must gate thinking output for the idle window.
    #[test]
    fn fresh_keypress_gates_thinking() {
        let _g = CLOCK_LOCK.lock().unwrap();
        reset_keypress_clock();
        note_keypress();
        assert!(
            millis_since_keypress() < KEYBOARD_IDLE_BEFORE_THINKING_MS,
            "a key just pressed must be inside the idle window"
        );
        assert!(!keyboard_idle_long_enough());
        // Restore global state for other tests (process-wide clock).
        reset_keypress_clock();
    }
}
