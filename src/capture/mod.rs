use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

pub struct HtmlCapture {
    file: std::fs::File,
    frame_count: usize,
}

const STYLE: &str = "\
body{background:#111;color:#eee;font-family:monospace}\
.frame{border-bottom:2px solid #444;padding:8px;margin:4px}\
.frame-num{color:#888;font-size:10px}\
pre{margin:0;line-height:1.2}\
span.b{font-weight:bold}\
span.dim{opacity:.6}\
span.f0{color:#000}span.f1{color:#a00}span.f2{color:#0a0}span.f3{color:#a50}\
span.f4{color:#00a}span.f5{color:#a0a}span.f6{color:#0aa}span.f7{color:#aaa}\
span.f8{color:#555}span.f9{color:#f55}span.f10{color:#5f5}span.f11{color:#ff5}\
span.f12{color:#55f}span.f13{color:#f5f}span.f14{color:#5ff}span.f15{color:#fff}\
span.bg0{background:#000}span.bg1{background:#a00}span.bg2{background:#0a0}span.bg3{background:#a50}\
span.bg4{background:#00a}span.bg5{background:#a0a}span.bg6{background:#0aa}span.bg7{background:#aaa}\
span.bg8{background:#555}span.bg9{background:#f55}span.bg10{background:#5f5}span.bg11{background:#ff5}\
span.bg12{background:#55f}span.bg13{background:#f5f}span.bg14{background:#5ff}span.bg15{background:#fff}\
span.rev{background:#eee;color:#000}";

impl HtmlCapture {
    pub fn new(path: &Path) -> Result<Self, String> {
        let mut file = OpenOptions::new()
            .create(true).write(true).truncate(true)
            .open(path)
            .map_err(|e| format!("failed to create capture file: {e}"))?;
        writeln!(file, "<!DOCTYPE html><html><head>\
            <meta charset='utf-8'>\
            <title>corro terminal capture</title>\
            <style>{STYLE}</style></head><body>").map_err(|e| e.to_string())?;
        Ok(HtmlCapture { file, frame_count: 0 })
    }

    pub fn capture_frame(&mut self, ansi_text: &str) -> Result<(), String> {
        self.frame_count += 1;
        let n = self.frame_count;
        write!(self.file, "<div class='frame'><div class='frame-num'>Frame #{n}</div><pre>")
            .map_err(|e| e.to_string())?;
        let html = ansi_to_html(ansi_text);
        self.file.write_all(html.as_bytes()).map_err(|e| e.to_string())?;
        writeln!(self.file, "</pre></div>").map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn finish(&mut self) -> Result<(), String> {
        writeln!(self.file, "</body></html>").map_err(|e| e.to_string())
    }
}

impl Drop for HtmlCapture {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// Read a raw ANSI terminal recording file and produce an HTML visualization.
/// Frame boundaries are detected via ESC[2J (clear-screen).
/// Record with: `script -c "corro file.corro" output.ansi`
/// Convert with: `corro --convert-ansi output.ansi outfile`
pub fn convert_ansi_file(input: &Path, output: &Path) -> Result<(), String> {
    let data = std::fs::read_to_string(input).map_err(|e| format!("read input: {e}"))?;
    let mut out = OpenOptions::new()
        .create(true).write(true).truncate(true)
        .open(output).map_err(|e| format!("create output: {e}"))?;
    writeln!(out, "<!DOCTYPE html><html><head>\
        <meta charset='utf-8'><title>corro terminal playback</title>\
        <style>{STYLE}</style></head><body>").map_err(|e| e.to_string())?;

    let mut frame_count = 0usize;
    let mut frame_start = 0usize;
    for (i, _) in data.match_indices("\x1b[2J") {
        let frame = &data[frame_start..i];
        if !frame.trim().is_empty() {
            frame_count += 1;
            let html = ansi_to_html(frame);
            write!(out, "<div class='frame'><div class='frame-num'>Frame #{frame_count}</div><pre>{html}</pre></div>")
                .map_err(|e| e.to_string())?;
        }
        frame_start = i;
    }
    let remaining = &data[frame_start..];
    if !remaining.trim().is_empty() {
        frame_count += 1;
        let html = ansi_to_html(remaining);
        write!(out, "<div class='frame'><div class='frame-num'>Frame #{frame_count}</div><pre>{html}</pre></div>")
            .map_err(|e| e.to_string())?;
    }
    writeln!(out, "</body></html>").map_err(|e| e.to_string())
}

fn ansi_to_html(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    let mut bold = false;
    let mut fg: Option<u8> = None;
    let mut bg: Option<u8> = None;
    let mut reverse = false;
    let mut dim = false;

    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            let mut params = String::new();
            loop {
                match chars.next() {
                    Some(c @ '0'..='9') => params.push(c),
                    Some(';') => params.push(';'),
                    Some('m') => break,
                    _ => { params.clear(); break; }
                }
            }
            if params.is_empty() { continue; }

            if !out.is_empty() && !out.ends_with("</span>") && !out.ends_with('>') {
                out.push_str("</span>");
            }

            for part in params.split(';') {
                match part {
                    "0" => { bold = false; fg = None; bg = None; reverse = false; dim = false; }
                    "1" => { bold = true; }
                    "2" => { dim = true; }
                    "7" => { reverse = true; }
                    "22" => { bold = false; }
                    "27" => { reverse = false; }
                    p if p.starts_with("38;5;") => {
                        fg = p.strip_prefix("38;5;").and_then(|v| v.parse().ok());
                    }
                    p if p.starts_with("48;5;") => {
                        bg = p.strip_prefix("48;5;").and_then(|v| v.parse().ok());
                    }
                    p if p.len() <= 2 => {
                        if let Ok(n) = p.parse::<u8>() {
                            match n {
                                30..=37 => fg = Some(n - 30),
                                38 => {}
                                39 => fg = None,
                                40..=47 => bg = Some(n - 40),
                                48 => {}
                                49 => bg = None,
                                90..=97 => fg = Some(n - 90 + 8),
                                100..=107 => bg = Some(n - 100 + 8),
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }

            let mut classes: Vec<String> = Vec::new();
            if bold { classes.push("b".into()); }
            if dim { classes.push("dim".into()); }
            if let Some(c) = fg { classes.push(format!("f{c}")); }
            if let Some(c) = bg { classes.push(format!("bg{c}")); }
            if reverse { classes.push("rev".into()); }

            if !out.ends_with('>') {
                if !classes.is_empty() {
                    out.push_str(&format!("<span class='{}'>", classes.join(" ")));
                }
            } else if !classes.is_empty() {
                out.push_str(&format!("<span class='{}'>", classes.join(" ")));
            }
            continue;
        }

        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\n' => out.push_str("\n"),
            '\r' => {}
            c if c.is_ascii_control() => {}
            c => out.push(c),
        }
    }

    if out.ends_with('>') {
        out.push_str("</span>");
    }
    out
}
