//! Render `docs/pir.png` as a sixel image on startup, with the help text
//! printed to its right. Sixel is a terminal graphics protocol (DECSIXEL);
//! Windows Terminal supports it (1.22+, opt-in via "Enable Sixel graphics").
//!
//! The PNG is decoded by shelling out to `ffmpeg` (no Rust image dependency),
//! scaled to fit the terminal, and encoded as sixel. Terminal support is
//! detected via the DA1 query (`ESC [ c`), which Windows Terminal answers with
//! capability `4` only when sixel is enabled.

use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

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
        if let Ok(term) = std::env::var("TERM") {
            if term == "dumb" || term.is_empty() {
                return false;
            }
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

/// Locate the pir logo PNG (docs/pir.png, falling back to ./pir.png).
fn find_png() -> Option<std::path::PathBuf> {
    for p in [Path::new("docs/pir.png"), Path::new("pir.png")] {
        if p.exists() {
            return Some(p.to_path_buf());
        }
    }
    None
}

/// Decode a PNG to raw RGB via `ffmpeg`. Returns (width, height, rgb). The
/// dimensions are read from the PNG IHDR chunk; the pixel data comes from
/// `ffmpeg -f rawvideo -pix_fmt rgb24`.
fn decode_png(path: &Path) -> Option<(usize, usize, Vec<u8>)> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 24 || &data[0..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes([data[16], data[17], data[18], data[19]]) as usize;
    let h = u32::from_be_bytes([data[20], data[21], data[22], data[23]]) as usize;
    if w == 0 || h == 0 {
        return None;
    }
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i", path.to_str()?, "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let rgb = out.stdout;
    if rgb.len() < w * h * 3 {
        return None;
    }
    Some((w, h, rgb))
}

/// Nearest-neighbour scale `rgb` from (w,h) to (nw,nh).
fn scale(rgb: &[u8], w: usize, h: usize, nw: usize, nh: usize) -> Vec<u8> {
    let mut out = vec![0u8; nw * nh * 3];
    for y in 0..nh {
        let sy = (y * h) / nh;
        for x in 0..nw {
            let sx = (x * w) / nw;
            let si = (sy * w + sx) * 3;
            let di = (y * nw + x) * 3;
            out[di..di + 3].copy_from_slice(&rgb[si..si + 3]);
        }
    }
    out
}

/// Compute a target size that fits within `max_cols` terminal columns.
fn scale_to_fit(w: usize, h: usize, max_cols: usize, cell_w: usize) -> (usize, usize) {
    let max_px = (max_cols * cell_w).max(1);
    let s = (max_px as f64 / w as f64).min(1.0);
    let nw = ((w as f64) * s).round().max(1.0) as usize;
    let nh = ((h as f64) * s).round().max(1.0) as usize;
    (nw, nh)
}

/// Build a colour palette from the image's unique colours (up to 256) and a
/// per-pixel palette index. Colours beyond 256 map to the nearest palette entry.
fn build_palette(rgb: &[u8]) -> (Vec<(u8, u8, u8)>, Vec<u8>) {
    let mut palette: Vec<(u8, u8, u8)> = Vec::new();
    let mut map: std::collections::HashMap<(u8, u8, u8), u8> = std::collections::HashMap::new();
    let mut color_of = vec![0u8; rgb.len() / 3];
    for (i, px) in rgb.chunks(3).enumerate() {
        let c = (px[0], px[1], px[2]);
        if let Some(&idx) = map.get(&c) {
            color_of[i] = idx;
        } else if palette.len() < 256 {
            let idx = palette.len() as u8;
            map.insert(c, idx);
            palette.push(c);
            color_of[i] = idx;
        } else {
            color_of[i] = nearest(&palette, c);
        }
    }
    (palette, color_of)
}

fn nearest(palette: &[(u8, u8, u8)], c: (u8, u8, u8)) -> u8 {
    let mut best = 0u8;
    let mut best_d = u32::MAX;
    for (i, p) in palette.iter().enumerate() {
        let dr = p.0 as i32 - c.0 as i32;
        let dg = p.1 as i32 - c.1 as i32;
        let db = p.2 as i32 - c.2 as i32;
        let d = (dr * dr + dg * dg + db * db) as u32;
        if d < best_d {
            best_d = d;
            best = i as u8;
        }
    }
    best
}

/// Encode raw RGB as a sixel image (DCS-wrapped).
fn encode_sixel(rgb: &[u8], w: usize, h: usize) -> String {
    let (palette, color_of) = build_palette(rgb);
    let mut out = String::new();
    out.push_str("\x1bPq");
    for (i, (r, g, b)) in palette.iter().enumerate() {
        out.push_str(&format!("#{};2;{};{};{}", i, r, g, b));
    }
    let bands = (h + 5) / 6;
    for band in 0..bands {
        let y0 = band * 6;
        // Which colours appear in this band?
        let mut present = vec![false; palette.len()];
        for y in y0..(y0 + 6).min(h) {
            for x in 0..w {
                present[color_of[y * w + x] as usize] = true;
            }
        }
        for ci in 0..palette.len() {
            if !present[ci] {
                continue;
            }
            let mut colvals = vec![0u8; w];
            for x in 0..w {
                let mut v = 0u8;
                for k in 0..6 {
                    let y = y0 + k;
                    if y < h && color_of[y * w + x] as usize == ci {
                        v |= 1 << k;
                    }
                }
                colvals[x] = v;
            }
            out.push_str(&format!("#{}", ci));
            let mut x = 0;
            while x < w {
                let v = colvals[x];
                let mut run = 1;
                while x + run < w && colvals[x + run] == v {
                    run += 1;
                }
                if run >= 3 {
                    out.push('!');
                    out.push_str(&run.to_string());
                    out.push((0x3f + v) as char);
                } else {
                    for _ in 0..run {
                        out.push((0x3f + v) as char);
                    }
                }
                x += run;
            }
        }
        out.push('-');
    }
    out.push_str("\x1b\\");
    out
}

/// Render the logo as sixel with `help_lines` printed to its right. Returns
/// `None` when sixel isn't available (no terminal, no ffmpeg, no PNG, or
/// unsupported).
pub fn render_banner(help_lines: &[String]) -> Option<String> {
    if !enabled() {
        return None;
    }
    let png = find_png()?;
    let (w, h, rgb) = decode_png(&png)?;
    let (cell_w, cell_h) = cell_size();
    let term_w = crate::term::terminal_width();
    let max_cols = (term_w / 2).clamp(20, 40);
    let (nw, nh) = scale_to_fit(w, h, max_cols, cell_w);
    let scaled = scale(&rgb, w, h, nw, nh);
    let sixel = encode_sixel(&scaled, nw, nh);
    let cols = (nw as f64 / cell_w as f64).ceil() as usize;
    let rows = (nh as f64 / cell_h as f64).ceil() as usize;

    let mut out = String::new();
    out.push_str(&sixel);
    // After the sixel the cursor is at the bottom-left of the image. Move to
    // the top-right and print the help lines.
    out.push_str(&format!("\x1b[{}A\x1b[{}C", rows, cols));
    let n = help_lines.len();
    for (i, line) in help_lines.iter().enumerate() {
        if i > 0 {
            let prev = crate::term::visible_len(&help_lines[i - 1]);
            out.push_str(&format!("\x1b[1B\x1b[{}D", prev));
        }
        out.push_str(line);
    }
    // Return the cursor to just below the image (bottom-left + 1 row).
    let last_len = if n > 0 { crate::term::visible_len(&help_lines[n - 1]) } else { 0 };
    let down = rows + 1 - n;
    out.push_str(&format!("\x1b[{}B\x1b[{}D", down, cols + last_len));
    Some(out)
}

/// Drain any leftover bytes the startup terminal queries (DA1 / XTV) left in
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
    fn png_header_parses_dimensions() {
        // Build a minimal PNG header (signature + IHDR) for a 4x3 image.
        let mut data = Vec::new();
        data.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        data.extend_from_slice(b"\x00\x00\x00\x0dIHDR");
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&3u32.to_be_bytes());
        let path = std::env::temp_dir().join("pir_test_ihdr.png");
        std::fs::write(&path, &data).unwrap();
        let d = std::fs::read(&path).unwrap();
        let w = u32::from_be_bytes([d[16], d[17], d[18], d[19]]) as usize;
        let h = u32::from_be_bytes([d[20], d[21], d[22], d[23]]) as usize;
        assert_eq!((w, h), (4, 3));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sixel_encoder_wraps_in_dcs() {
        // 2x2 solid red image.
        let rgb = vec![255u8, 0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0];
        let s = encode_sixel(&rgb, 2, 2);
        assert!(s.starts_with("\x1bPq"), "must start with DCS: {s:?}");
        assert!(s.ends_with("\x1b\\"), "must end with ST: {s:?}");
        assert!(s.contains("#0;2;255;0;0"), "palette must define red: {s:?}");
    }

    #[test]
    fn sixel_encoder_two_colours() {
        // 2x1: left red, right blue.
        let rgb = vec![255, 0, 0, 0, 0, 255];
        let s = encode_sixel(&rgb, 2, 1);
        assert!(s.contains("#0;2;255;0;0"));
        assert!(s.contains("#1;2;0;0;255"));
    }

    #[test]
    fn scale_preserves_size() {
        let rgb = vec![1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let out = scale(&rgb, 2, 2, 2, 2);
        assert_eq!(out, rgb);
    }

    #[test]
    fn scale_to_fit_respects_max() {
        let (nw, nh) = scale_to_fit(308, 371, 40, 8);
        assert!(nw <= 40 * 8);
        assert!(nh <= 371);
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
