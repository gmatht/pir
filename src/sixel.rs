//! Startup banner: the embedded `docs/pir-banner.png` logo (48x58) rendered
//! via `img2sixel`, with the status text printed to its right. Sixel is a
//! terminal graphics protocol (DECSIXEL); Windows Terminal supports it
//! (1.22+, opt-in via "Enable Sixel graphics").
//!
//! No image code lives here on purpose: `img2sixel` reads, resizes,
//! quantizes, and encodes, so there is one well-tested renderer instead of a
//! hand-rolled encoder plus an ffmpeg decode. Terminal sixel support is
//! detected via the DA1 query (`ESC [ c`), which Windows Terminal answers
//! with capability `4` only when sixel is enabled.
//!
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Eighth-size plus 25% (48x58; see scripts/rebuild-banner.py), pngcrushed
/// banner logo embedded in the binary, so the sixel banner needs no
/// external file and no runtime downscaling.
const BANNER_PNG: &[u8] = include_bytes!("../docs/pir-banner.png");

/// Whether sixel rendering is enabled. Auto-detects via DA1; `PIR_SIXEL=1`
/// forces it on, `PIR_SIXEL=0` forces it off.
pub fn enabled() -> bool {
    match std::env::var("PIR_SIXEL") {
        Ok(v) if v == "1" => return true,
        Ok(v) if v == "0" => return false,
        _ => {}
    }
    supported()
}

/// Detect sixel support via the DA1 (Device Attributes) query. Writes
/// `ESC [ c` and reads the response; capability `4` means sixel is supported.
/// Windows Terminal only advertises `4` when the "Enable Sixel graphics"
/// setting is on, so this is the reliable detection path there.
///
/// The response is parsed strictly: it must be a real DA1 reply
/// (`ESC [ ? … c`) and advertise sixel as an *exact* capability token `4` —
/// never a substring match, which would false-positive on a `4` inside a
/// multi-digit parameter (e.g. `14`/`24`) in a non-sixel reply.
fn supported() -> bool {
    // The DA1 query (`ESC [ c`) is only reliable on unix terminals. On Windows
    // the console is still in echo mode at startup (raw mode is only enabled
    // during a running turn), so the terminal's DA1 reply gets echoed back to
    // the screen as literal text — e.g. `^[[?61;4;…c` — instead of the REPL
    // prompt. Disable sixel detection entirely on non-unix to avoid that.
    #[cfg(not(unix))]
    {
        return false;
    }
    #[cfg(unix)]
    {
        if !crate::term::is_terminal() {
            return false;
        }
        // Conservative guards: never emit sixel into a dumb/unknown terminal or
        // over a remote (SSH) session where the local terminal may not render it.
        if let Ok(term) = std::env::var("TERM")
            && (term == "dumb" || term.is_empty()) {
                return false;
            }
        if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_CLIENT").is_some() {
            return false;
        }
        let resp = query_terminal("\x1b[c").unwrap_or_default();
        // A valid DA1 reply looks like `ESC [ ? 1 ; 2 ; 4 ; … c`. Require the
        // leading `ESC [ ?` and trailing `c`, then check for an exact `4` token.
        let body = resp
            .strip_prefix("\x1b[?")
            .and_then(|s| s.strip_suffix('c'))
            .unwrap_or_default();
        body.split(';').any(|tok| tok.trim() == "4")
    }
}

/// Query the terminal for its size in pixels (`ESC [ 1 4 t`) and cells
/// (`ESC [ 1 8 t`), returning the cell size in pixels. Falls back to 8x16.
fn cell_size() -> (usize, usize) {
    let px = query_terminal("\x1b[14t").unwrap_or_default();
    let cells = query_terminal("\x1b[18t").unwrap_or_default();
    // `ESC [ 4 ; <h> ; <w> t` and `ESC [ 8 ; <h> ; <w> t`
    let parse = |s: &str| -> Option<(usize, usize)> {
        let s = s.trim_start_matches("\x1b[");
        let s = s.trim_end_matches('t');
        let mut parts = s.split(';');
        let _kind = parts.next()?;
        let h: usize = parts.next()?.trim().parse().ok()?;
        let w: usize = parts.next()?.trim().parse().ok()?;
        Some((w, h))
    };
    match (parse(&px), parse(&cells)) {
        (Some((pw, ph)), Some((cw, ch))) if cw > 0 && ch > 0 => (pw / cw, ph / ch),
        _ => (8, 16),
    }
}

/// the reply is never delivered and just sits in the tty buffer until the REPL
/// (rustyline) later reads it as the user's first prompt. Switching to
/// non-canonical + non-blocking mode lets `read()` return the bytes as they
/// arrive, so `query_terminal` can consume the whole reply. On non-unix this
/// is a no-op.
#[cfg(unix)]
pub(crate) struct RawStdinGuard {
    fd: std::os::unix::io::RawFd,
    orig_termios: Option<libc::termios>,
    orig_nonblock: bool,
}

#[cfg(unix)]
impl RawStdinGuard {
    pub(crate) fn enable() -> Self {
        use std::os::unix::io::AsRawFd;
        let fd = std::io::stdin().as_raw_fd();
        let mut orig_termios: Option<libc::termios> = None;
        let orig_nonblock;
        unsafe {
            let mut tios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut tios) == 0 {
                orig_termios = Some(tios);
                let mut raw = tios;
                // Non-canonical: no line editing / echo, and read returns
                // immediately (VMIN=0, VTIME=0) so the reply is delivered byte
                // by byte and our poll loop can time out cleanly.
                raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
                raw.c_cc[libc::VMIN] = 0;
                raw.c_cc[libc::VTIME] = 0;
                libc::tcsetattr(fd, libc::TCSANOW, &raw);
            }
            let flags = libc::fcntl(fd, libc::F_GETFL);
            orig_nonblock = flags & libc::O_NONBLOCK != 0;
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        RawStdinGuard { fd, orig_termios, orig_nonblock }
    }
}

#[cfg(unix)]
impl Drop for RawStdinGuard {
    fn drop(&mut self) {
        unsafe {
            if let Some(t) = self.orig_termios.take() {
                libc::tcsetattr(self.fd, libc::TCSANOW, &t);
            }
            let flags = libc::fcntl(self.fd, libc::F_GETFL);
            let newflags = if self.orig_nonblock {
                flags | libc::O_NONBLOCK
            } else {
                flags & !libc::O_NONBLOCK
            };
            libc::fcntl(self.fd, libc::F_SETFL, newflags);
        }
    }
}

/// Write a terminal query and read its response with a timeout. Returns the
/// response text, or `None` on timeout. Reads with a deadline on the main
/// thread (no background thread), so a terminal that ignores the query can
/// never leave a thread blocked on stdin eating the user's keystrokes for the
/// rest of the session.
fn query_terminal(seq: &str) -> Option<String> {
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
    let mut s = String::new();
    let mut buf = [0u8; 1];
    let deadline = std::time::Instant::now() + Duration::from_millis(300);
    // Read in non-canonical/non-blocking mode so the (newline-less) reply is
    // actually delivered and consumed here rather than leaking into the REPL.
    #[cfg(unix)]
    let _guard = RawStdinGuard::enable();
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let wait_ms = remaining.as_millis().min(50) as u32;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = std::io::stdin().as_raw_fd();
            let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            let r = unsafe { libc::poll(&mut pfd, 1, wait_ms as i32) };
            if r > 0 && (pfd.revents & libc::POLLIN) != 0 {
                let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 1) };
                if n == 1 {
                    s.push(buf[0] as char);
                    if buf[0] == b'c' || buf[0] == b't' {
                        break;
                    }
                } else if n == 0 {
                    break;
                }
            }
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
            use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
            use windows_sys::Win32::System::Threading::WaitForSingleObject;
            let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
            if unsafe { WaitForSingleObject(handle, wait_ms) } == WAIT_OBJECT_0 {
                let mut stdin = std::io::stdin();
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => {
                        s.push(buf[0] as char);
                        if buf[0] == b'c' || buf[0] == b't' {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Locate the pir logo PNG: `docs/pir.png` / `./pir.png` relative to the cwd,
/// plus the binary's directory and the compile-time manifest dir, so the
/// banner still renders when `pir` is invoked from elsewhere on PATH.
fn find_png() -> Option<std::path::PathBuf> {
    let mut candidates: Vec<std::path::PathBuf> = vec![
        Path::new("docs/pir.png").to_path_buf(),
        Path::new("pir.png").to_path_buf(),
    ];
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent() {
            candidates.push(dir.join("docs/pir.png"));
            candidates.push(dir.join("pir.png"));
        }
    candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/pir.png"));
    candidates.into_iter().find(|p| p.exists())
}

/// Read PNG dimensions from the IHDR chunk (pure Rust — only the size is
/// needed for override scaling; `img2sixel` does the actual rendering).
fn png_dims(data: &[u8]) -> Option<(usize, usize)> {
    if data.len() < 24 || &data[0..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes([data[16], data[17], data[18], data[19]]) as usize;
    let h = u32::from_be_bytes([data[20], data[21], data[22], data[23]]) as usize;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}

/// Native size of the embedded banner in pixels.
const BANNER_W: usize = 48;
const BANNER_H: usize = 58;

/// Word-wrap `line` to rows of at most `width` visible columns. ANSI SGR
/// escapes ride along with the following text (zero-width) and are never
/// split, so styled lines (e.g. `term::dim`) wrap without leaking raw
/// escapes or breaking the styling. Long words hard-split on char
/// boundaries. Never returns an empty vec.
fn wrap_line(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    // Tokenize into escapes, words, and single-space gaps.
    enum Tok {
        Esc(String),
        Word(String),
        Space,
    }
    let mut toks: Vec<Tok> = Vec::new();
    let mut word = String::new();
    let flush_word = |word: &mut String, toks: &mut Vec<Tok>| {
        if !word.is_empty() {
            toks.push(Tok::Word(std::mem::take(word)));
        }
    };
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            flush_word(&mut word, &mut toks);
            let mut esc = String::from('\x1b');
            for e in chars.by_ref() {
                esc.push(e);
                if e == 'm' {
                    break;
                }
            }
            toks.push(Tok::Esc(esc));
        } else if c == ' ' {
            flush_word(&mut word, &mut toks);
            toks.push(Tok::Space);
        } else {
            word.push(c);
        }
    }
    flush_word(&mut word, &mut toks);

    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    let flush = |rows: &mut Vec<String>, cur: &mut String, cur_len: &mut usize| {
        if cur.is_empty() {
            return;
        }
        // Drop a trailing gap space: it fit, but the next word wrapped, so
        // keeping it would leave ragged whitespace at the row end.
        let trimmed = cur.trim_end().to_owned();
        *cur = trimmed;
        if !cur.is_empty() {
            rows.push(std::mem::take(cur));
        }
        *cur_len = 0;
    };
    // Push a (possibly long) word, hard-splitting on char boundaries when
    // it exceeds the width. Words contain no escapes by construction.
    // Spacing comes only from Space tokens (pushing here too doubled every
    // gap); a word never adds its own leading space.
    let push_word = |rows: &mut Vec<String>, cur: &mut String, cur_len: &mut usize, w: &str| {
        let wlen = w.chars().count();
        if wlen <= width && *cur_len + wlen <= width {
            cur.push_str(w);
            *cur_len += wlen;
            return;
        }
        if wlen <= width {
            flush(rows, cur, cur_len);
            cur.push_str(w);
            *cur_len = wlen;
            return;
        }
        flush(rows, cur, cur_len);
        let mut chunk = String::new();
        let mut chunk_len = 0usize;
        for ch in w.chars() {
            if chunk_len + 1 > width {
                rows.push(std::mem::take(&mut chunk));
                chunk_len = 0;
            }
            chunk.push(ch);
            chunk_len += 1;
        }
        *cur = chunk;
        *cur_len = chunk_len;
    };
    for tok in toks {
        match tok {
            Tok::Esc(e) => cur.push_str(&e),
            Tok::Word(w) => push_word(&mut rows, &mut cur, &mut cur_len, &w),
            Tok::Space => {
                if cur.is_empty() {
                    continue;
                }
                if cur_len + 1 > width {
                    flush(&mut rows, &mut cur, &mut cur_len);
                } else {
                    cur.push(' ');
                    cur_len += 1;
                }
            }
        }
    }
    if !cur.is_empty() {
        rows.push(cur);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}
/// Run `img2sixel` on PNG bytes from stdin, returning the validated DCS
/// string plus the rendered pixel size parsed from its `"1;1;W;H` device
/// setup (ground truth for cursor math — never assumed). Returns `None`
/// when the binary is missing, fails, or emits anything but a well-formed
/// DCS. `-d none` keeps flat fills undithered; callers retry with trimmed
/// flags for older libsixel builds.
fn img2sixel(png: &[u8], args: &[&str]) -> Option<(String, usize, usize)> {
    let mut child = Command::new("img2sixel")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(png).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let (dw, dh) = {
        let body = s.strip_prefix("\x1bPq")?.strip_suffix("\x1b\\")?;
        let rest = body.strip_prefix("\"1;1;")?;
        let (ws, rest) = rest.split_once(';')?;
        let hs: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        let (dw, dh): (usize, usize) = (ws.parse().ok()?, hs.parse().ok()?);
        if dw == 0 || dh == 0 {
            return None;
        }
        (dw, dh)
    };
    if s.len() < 64 {
        return None;
    }
    Some((s, dw, dh))
}

/// Render the logo via `img2sixel` with `side_lines` printed to its right.
/// Returns `None` when sixel isn't available, `img2sixel` is missing/fails,
/// or no logo is found. `img2sixel` ends its DCS with a carriage return on
/// the image's bottom row, so the cursor starts at the bottom-left.
pub fn render_banner(side_lines: &[String]) -> Option<String> {
    if !enabled() {
        return None;
    }
    // Embedded banner first (no external file needed); an external
    // docs/pir.png or ./pir.png is a full-size dev override resized to the
    // banner width. Plain defaults are retried once for older libsixel
    // builds that reject `-d none`.
    let (sixel, dw, dh) = img2sixel(BANNER_PNG, &["-d", "none"])
        .or_else(|| img2sixel(BANNER_PNG, &[]))
        .or_else(|| {
            let path = find_png()?;
            let data = std::fs::read(path).ok()?;
            png_dims(&data)?;
            img2sixel(&data, &["-d", "none", "-w", "48px"])
                .or_else(|| img2sixel(&data, &["-w", "48px"]))
        })?;
    let (cell_w, cell_h) = cell_size();
    let cols = (dw as f64 / cell_w as f64).ceil() as usize;
    let rows = ((dh as f64 / cell_h as f64).ceil() as usize).max(1);
    let term_w = crate::term::terminal_width();

    let mut out = String::new();
    out.push_str(&sixel);
    // Cursor sits at the bottom-left of the image. Rise to its top row and
    // print EVERY wrapped text row indented past the image. The image's
    // terminal height comes from cell-size queries that can be wrong (dead
    // fallback, scaled sixel aspect), and any row starting at column 0
    // risks overwriting the cat — as the help line did. A uniform indented
    // block can never overlap, whatever the true geometry.
    if rows > 1 {
        out.push_str(&format!("\x1b[{}A", rows - 1));
    }
    out.push_str("\r");
    let gap = 2usize;
    let indent = cols + gap;
    let avail = term_w.saturating_sub(indent).max(20);
    let mut wrapped: Vec<String> = Vec::new();
    for line in side_lines {
        wrapped.extend(wrap_line(line, avail));
    }
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    let n = wrapped.len();
    for (i, line) in wrapped.iter().enumerate() {
        if i > 0 {
            out.push_str("\x1b[1B\r");
        }
        out.push_str(&format!("\x1b[{indent}C"));
        out.push_str(line);
    }
    // Park the cursor just below whichever is taller, image or text.
    let down = (rows.max(n) + 1).saturating_sub(n).max(1);
    out.push_str(&format!("\r\x1b[{down}B"));
    Some(out)
}

/// Drain any leftover/// Drain any leftover bytes the startup terminal queries (DA1 / XTV) left in
/// the tty buffer, so they can't surface as the user's first REPL prompt (a
/// stray `\x1b[?61;4;...c` style reply with no trailing newline). Re-issues the
/// DA1 query and reads with a timeout in non-canonical/non-blocking mode,
/// discarding whatever is answered; then consumes any remaining buffered bytes.
/// No-op on non-unix.
pub fn drain_input() {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // query_terminal switches the fd to non-canonical/non-blocking mode (see
        // RawStdinGuard) and waits up to 300ms for the reply, so a late DA1
        // straggler from the banner is consumed here instead of leaking into
        // rustyline as the user's first keystroke.
        let _ = query_terminal("\x1b[c");
        // Whatever else arrived after the reply (paste fragments, spurious
        // bytes) is drained non-blocking and discarded.
        let _guard = RawStdinGuard::enable();
        let fd = std::io::stdin().as_raw_fd();
        let mut buf = [0u8; 64];
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 64) };
            if n <= 0 {
                break;
            }
            if n < 64 {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_dims_reads_ihdr() {
        // Minimal PNG header (signature + IHDR) for a 4x3 image.
        let mut data = Vec::new();
        data.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        data.extend_from_slice(b"\x00\x00\x00\x0dIHDR");
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&3u32.to_be_bytes());
        assert_eq!(png_dims(&data), Some((4, 3)));
        assert_eq!(png_dims(b"junk"), None);
        assert_eq!(png_dims(BANNER_PNG), Some((BANNER_W, BANNER_H)));
    }

    #[test]
    fn wrap_keeps_ansi_escapes_intact() {
        let line = crate::term::dim("model cerebras/gpt-oss-120b · confirm-actions · config /home/ai_pir/.pi/agent");
        let rows = wrap_line(&line, 40);
        assert!(rows.len() > 1, "long status line must wrap");
        for r in &rows {
            assert!(crate::term::visible_len(r) <= 40, "row too wide: {r:?}");
        }
        let vis: String = rows.iter().map(|r| {
            let mut s = String::new();
            let mut esc = false;
            for c in r.chars() {
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
                s.push(c);
            }
            s
        }).collect::<Vec<_>>().join(" ");
        assert!(vis.contains("cerebras/gpt-oss-120b"), "text must survive wrapping: {vis:?}");
        assert!(!vis.contains("  "), "single spaces must not double: {vis:?}");
        for r in &rows {
            // Every ESC must open a complete `ESC [ … m` sequence: no split
            // or truncated escape may appear in any wrapped row.
            let mut chars = r.chars().peekable();
            while let Some(c) = chars.next() {
                if c != '\x1b' {
                    continue;
                }
                assert_eq!(chars.next(), Some('['), "broken escape in {r:?}");
                let mut closed = false;
                for e in chars.by_ref() {
                    if e == 'm' {
                        closed = true;
                        break;
                    }
                    assert!(e.is_ascii_digit() || e == ';', "broken escape in {r:?}");
                }
                assert!(closed, "unterminated escape in {r:?}");
            }
        }
    }

    #[test]
    fn embedded_banner_dimensions() {
        // The embedded logo must stay 48x58: cursor math and the `-w 48px`
        // override sizing both assume it. Rebuild via scripts/rebuild-banner.py.
        assert_eq!(png_dims(BANNER_PNG), Some((BANNER_W, BANNER_H)));
        assert_eq!((BANNER_W, BANNER_H), (48, 58));
    }

    #[test]
    fn banner_renders_when_forced() {
        // The banner must render (Some) with valid DCS framing when sixel
        // is forced on and img2sixel is installed. Skipped where it isn't:
        // no external renderer, no banner — the fallback is plain text.
        if Command::new("img2sixel").arg("--help").output().is_err() {
            eprintln!("SKIP banner_renders_when_forced: img2sixel unavailable");
            return;
        }
        unsafe { std::env::set_var("PIR_SIXEL", "1") };
        let lines = vec![
            "model test-model · confirm-actions · config /home/test/.pi".to_string(),
            "/help for commands · ctrl-d quit".to_string(),
        ];
        let out = render_banner(&lines);
        unsafe { std::env::remove_var("PIR_SIXEL") };
        let out = out.expect("banner must render with PIR_SIXEL=1 and img2sixel present");
        assert!(out.starts_with("\x1bPq"), "must start with sixel DCS");
        assert!(out.contains("\x1b\\"), "must contain sixel ST");
        assert!(out.contains("test-model"), "status text must be beside the image");
    }

    #[test]
    fn side_text_never_starts_at_column_zero() {
        // Regression: wrapped rows past the estimated image height started
        // at column 0 and overwrote the cat (the help line). Every text
        // row must begin with an indent move, unconditionally.
        if Command::new("img2sixel").arg("--help").output().is_err() {
            eprintln!("SKIP side_text_never_starts_at_column_zero: img2sixel unavailable");
            return;
        }
        unsafe { std::env::set_var("PIR_SIXEL", "1") };
        // Short lines: one wrapped row each, so the row count is exact.
        // Five rows overflows the 3-row image: the old `i < rows` cutoff
        // left rows 4-5 at column 0, overwriting the cat.
        let lines = vec!["aaa".to_string(), "bbb".to_string(), "ccc".to_string(), "ddd".to_string(), "eee".to_string()];
        let out = render_banner(&lines);
        unsafe { std::env::remove_var("PIR_SIXEL") };
        let out = out.expect("banner must render");
        let tail = out.split("\x1b\\").nth(1).expect("text follows sixel ST");
        // First row: carriage return then indent move.
        assert!(tail.contains("\r\x1b["), "first row indented: {tail:?}");
        // Every subsequent row: down + return, then an indent move — never
        // bare text at column 0.
        assert_eq!(tail.matches("\x1b[1B\r\x1b[").count(), 4, "all later rows indented: {tail:?}");
        assert_eq!(tail.matches("\x1b[1B\r").count(), 4, "no stray row breaks: {tail:?}");
    }

    #[test]
    fn da1_parsing_requires_exact_4_token() {
        // A real sixel-capable reply advertises `4` as its own token.
        let ok = "\x1b[?1;2;4;22;24c";
        let body = ok.strip_prefix("\x1b[?").and_then(|s| s.strip_suffix('c')).unwrap_or_default();
        assert!(body.split(';').any(|t| t.trim() == "4"), "must detect sixel: {ok:?}");

        // A non-sixel reply with a `4` inside a multi-digit parameter (e.g. 14/24)
        // must NOT be treated as sixel support — this was the false positive that
        // dumped raw sixel bytes ("wrightwright…") into a non-sixel terminal.
        let no = "\x1b[?1;2;14;24c";
        let body = no.strip_prefix("\x1b[?").and_then(|s| s.strip_suffix('c')).unwrap_or_default();
        assert!(!body.split(';').any(|t| t.trim() == "4"), "must NOT detect sixel: {no:?}");

        // A reply that isn't a DA1 response at all must not match.
        let junk = "some text with a 4 in it";
        let body = junk.strip_prefix("\x1b[?").and_then(|s| s.strip_suffix('c')).unwrap_or_default();
        assert!(!body.split(';').any(|t| t.trim() == "4"));
    }
}

/// A terminal replies to DA1 (`\x1b[c`) with `\x1b[?61;4;22;24c`. That reply
/// must be fully consumed by the startup queries + drain (`query_terminal` /
/// `drain_input`) and never leak back into stdin, where it would surface as the
/// user's first REPL prompt. Regression for the `RawStdinGuard` machinery: once
/// the non-canonical read guard drops and restores the terminal, the reply
/// bytes must not be re-delivered to a later reader.
#[cfg(all(test, unix))]
mod leak_tests {
    use super::*;

    /// Restores stdin to its original fd after the test, even on panic.
    struct RestoreStdin(libc::c_int);
    impl Drop for RestoreStdin {
        fn drop(&mut self) {
            unsafe {
                libc::dup2(self.0, 0);
                libc::close(self.0);
            }
        }
    }

    #[test]
    fn da1_reply_consumed_not_leaked() {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let opened = unsafe {
            libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), std::ptr::null())
        };
        if opened != 0 {
            eprintln!("SKIP da1_reply_consumed_not_leaked: openpty unavailable");
            return;
        }
        unsafe {
            // Point stdin at the pty slave for the duration of the test.
            let saved_stdin = libc::dup(0);
            let _restore = RestoreStdin(saved_stdin);
            libc::dup2(slave, 0);
            // Non-blocking read-side so the "nothing left" assertion can't hang.
            let flags = libc::fcntl(slave, libc::F_GETFL);
            libc::fcntl(slave, libc::F_SETFL, flags | libc::O_NONBLOCK);

            // The emulated terminal answers the DA1 query; write the reply to
            // the master side up front (it is buffered and read via the slave).
            let reply = b"\x1b[?61;4;22;24c";
            assert_eq!(
                libc::write(master, reply.as_ptr() as *const libc::c_void, reply.len()),
                reply.len() as isize
            );

            // Issues `\x1b[c` and reads the reply through the RawStdinGuard.
            drain_input();

            // After the drain, nothing may remain for a later consumer: reading
            // the slave returns 0 (EOF) or -1/EAGAIN (non-blocking, no data) —
            // the DA1 reply was consumed, never leaked.
            let mut buf = [0u8; 8];
            let n = libc::read(slave, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
            assert!(n <= 0, "DA1 reply leaked into stdin after drain_input (read {n} bytes)");

            libc::close(master);
            libc::close(slave);
        }
    }
}



