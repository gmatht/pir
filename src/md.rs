//! Minimal terminal Markdown renderer.
//!
//! Agent replies are CommonMark (headings, `**bold**`, lists, fenced code
//! blocks, lists, fenced code blocks, links, …). The REPL used to dump them verbatim, so the user
//! saw raw `**`/`#`/` ``` ` instead of formatted text. This module parses the
//! markdown with comrak and emits a plain-text approximation suitable for a
//! fixed-width terminal: headings get a marker + bold, emphasis becomes `^`
//! style emphasis via ANSI, code blocks are fenced and indented, links show
//! their text with the URL in brackets, and list/quote markers are preserved.
//!
//! The output is *not* HTML — it is text with ANSI styling, so it can be
//! printed by the streaming REPL (`term::out`) and by the TUI (each paragraph
//! becomes a `Line`). Colour is applied only when the terminal supports it.

use comrak::nodes::{AstNode, ListType, NodeValue};
use comrak::{parse_document, Arena, Options};
use std::time::{Duration, Instant};

/// Render `md` to a styled, terminal-width-friendly `String`. Block-level
/// structure (headings, lists, code fences, blank lines) is preserved; inline
/// emphasis is rendered with ANSI bold/italic when `color` is true.
pub fn render(md: &str, color: bool) -> String {
    let mut opts = Options::default();
    opts.extension.strikethrough = true;
    opts.extension.table = true;
    opts.extension.autolink = true;
    opts.extension.tasklist = true;
    opts.parse.smart = true;

    let arena = Arena::new();
    let root = parse_document(&arena, md, &opts);

    let mut out = String::new();
    render_node(root, &RenderCtx { color, indent: 0, in_list: false }, &mut out);
    // Collapse the worst of the excess blank lines while keeping paragraph
    // separation. Comrak emits a newline per leaf; we normalise ≥3 → 2.
    let collapsed = collapse_blank(&out);
    collapsed.trim_end().to_string() + "\n"
}

struct RenderCtx {
    color: bool,
    /// Current block indent (for nested lists).
    indent: usize,
    /// Whether we are inside a list (so children render as items).
    in_list: bool,
}

fn render_node<'a>(node: &'a AstNode<'a>, ctx: &RenderCtx, out: &mut String) {
    let value = &node.data.borrow().value;
    match value {
        NodeValue::Document => {
            for c in node.children() {
                render_node(c, ctx, out);
            }
        }
        NodeValue::Heading(h) => {
            let level = h.level as usize;
            let hashes = "#".repeat(level.min(6));
            let prefix = format!("{hashes} ");
            let mut body = String::new();
            for c in node.children() {
                render_node(c, ctx, &mut body);
            }
            let body = body.trim();
            if ctx.color && !body.is_empty() {
                out.push_str(&format!("\x1b[1m{}{}\x1b[0m\n", prefix, body));
            } else {
                out.push_str(&format!("{prefix}{body}\n"));
            }
        }
        NodeValue::Paragraph => {
            let mut body = String::new();
            for c in node.children() {
                render_node(c, ctx, &mut body);
            }
            let body = body.trim_end();
            if !body.is_empty() {
                out.push_str(&indent_lines(body, ctx.indent));
                out.push('\n');
            }
        }
        NodeValue::BlockQuote => {
            let mut inner = RenderCtx { color: ctx.color, indent: ctx.indent, in_list: false };
            let mut body = String::new();
            for c in node.children() {
                render_node(c, &inner, &mut body);
            }
            for l in body.lines() {
                out.push_str(&" ".repeat(ctx.indent));
                out.push_str("> ");
                out.push_str(l);
                out.push('\n');
            }
        }
        NodeValue::List(l) => {
            let mut idx: usize = 1;
            let ordered = l.list_type == ListType::Ordered;
            let start = l.start as usize;
            for c in node.children() {
                let marker = if ordered {
                    format!("{}{}. ", " ".repeat(ctx.indent), start + idx - 1)
                } else {
                    format!("{}• ", " ".repeat(ctx.indent))
                };
                let mut item_ctx = RenderCtx {
                    color: ctx.color,
                    indent: 0,
                    in_list: true,
                };
                render_list_item(c, &marker, &mut item_ctx, out);
                idx += 1;
            }
        }
        NodeValue::Item(_) => {
            // When rendered directly (rare), just recurse as a paragraph.
            for c in node.children() {
                render_node(c, ctx, out);
            }
        }
        NodeValue::CodeBlock(cb) => {
            let indent = " ".repeat(ctx.indent);
            let lang = cb.info.as_str().trim();
            out.push_str(&format!("{indent}```{lang}\n"));
            let code = cb.literal.as_str();
            for l in code.lines() {
                out.push_str(&indent);
                out.push_str(l);
                out.push('\n');
            }
            out.push_str(&format!("{indent}```\n"));
        }
        NodeValue::ThematicBreak => {
            out.push_str(&format!("{}---\n", " ".repeat(ctx.indent)));
        }
        NodeValue::Table(_) => {
            render_table(node, ctx, out);
        }
        NodeValue::Text(t) => {
            out.push_str(t.as_ref());
        }
        NodeValue::SoftBreak => out.push('\n'),
        NodeValue::LineBreak => out.push('\n'),
        NodeValue::Code(c) => {
            // Inline code: reverse video when colour is on, else backticks.
            if ctx.color {
                out.push_str(&format!("\x1b[7m{}\x1b[0m", c.literal.as_str()));
            } else {
                out.push_str(&format!("`{}`", c.literal.as_str()));
            }
        }
        NodeValue::Emph => {
            let mut body = String::new();
            for c in node.children() {
                render_node(c, ctx, &mut body);
            }
            if ctx.color {
                out.push_str(&format!("\x1b[3m{body}\x1b[0m"));
            } else {
                out.push_str(&body);
            }
        }
        NodeValue::Strong => {
            let mut body = String::new();
            for c in node.children() {
                render_node(c, ctx, &mut body);
            }
            if ctx.color {
                out.push_str(&format!("\x1b[1m{body}\x1b[0m"));
            } else {
                out.push_str(&body);
            }
        }
        NodeValue::Strikethrough => {
            let mut body = String::new();
            for c in node.children() {
                render_node(c, ctx, &mut body);
            }
            if ctx.color {
                out.push_str(&format!("\x1b[9m{body}\x1b[0m"));
            } else {
                out.push_str(&body);
            }
        }
        NodeValue::Link(l) => {
            let mut body = String::new();
            for c in node.children() {
                render_node(c, ctx, &mut body);
            }
            let url = l.url.as_str();
            if body.is_empty() {
                out.push_str(url);
            } else if url == body {
                out.push_str(&body);
            } else {
                out.push_str(&format!("{body} [{url}]"));
            }
        }
        NodeValue::Image(i) => {
            out.push_str(&format!("[image: {}]", i.title.as_str()));
        }
        // Tables, HTML, footnotes, etc. fall back to recursing children.
        _ => {
            for c in node.children() {
                render_node(c, ctx, out);
            }
        }
    }
}

/// Render a list item. The marker is printed once on the first line; the
/// item's block children are rendered with the deeper indent so wrapped lines
/// align under the text (not the marker).
fn render_list_item<'a>(node: &'a AstNode<'a>, marker: &str, ctx: &mut RenderCtx, out: &mut String) {
    let mut first = true;
    for c in node.children() {
        let mut body = String::new();
        render_node(c, ctx, &mut body);
        let body = body.trim_end();
        if body.is_empty() {
            continue;
        }
        for l in body.lines() {
            if first {
                out.push_str(marker);
                out.push_str(l);
                first = false;
            } else {
                out.push_str(&" ".repeat(marker.chars().count()));
                out.push_str(l);
            }
            out.push('\n');
        }
    }
    if first {
        // Empty item (e.g. a checked task with no text): still show marker.
        out.push_str(marker);
        out.push('\n');
    }
}

/// Render a GFM table as a simple monospace grid.
fn render_table<'a>(node: &'a AstNode<'a>, ctx: &RenderCtx, out: &mut String) {
    // Collect rows (header + body) and column count.
    let mut rows: Vec<Vec<String>> = Vec::new();
    for c in node.children() {
        if let NodeValue::TableRow(_) = &c.data.borrow().value {
            let mut cells = Vec::new();
            for cell in c.children() {
                let mut body = String::new();
                for cc in cell.children() {
                    render_node(cc, ctx, &mut body);
                }
                cells.push(body.replace('\n', " ").trim().to_string());
            }
            rows.push(cells);
        }
    }
    if rows.is_empty() {
        return;
    }
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|i| rows.iter().map(|r| r.get(i).map_or(0, |s| s.chars().count())).max().unwrap_or(0))
        .collect();
    for (ri, row) in rows.iter().enumerate() {
        out.push_str(&" ".repeat(ctx.indent));
        for (ci, w) in widths.iter().enumerate() {
            let cell = row.get(ci).cloned().unwrap_or_default();
            out.push('|');
            out.push(' ');
            out.push_str(&cell);
            let pad = w.saturating_sub(cell.chars().count());
            out.push_str(&" ".repeat(pad));
            out.push(' ');
        }
        out.push_str("|\n");
        if ri == 0 {
            out.push_str(&" ".repeat(ctx.indent));
            out.push('|');
            for w in &widths {
                out.push_str(&"-".repeat(w + 2));
                out.push('|');
            }
            out.push('\n');
        }
    }
}

/// Prefix every line of `s` with `indent` spaces.
fn indent_lines(s: &str, indent: usize) -> String {
    let pad = " ".repeat(indent);
    s.lines()
        .map(|l| format!("{pad}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collapse runs of ≥3 blank lines down to a single blank line.
fn collapse_blank(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blanks = 0;
    for line in s.lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks <= 1 {
                out.push('\n');
            }
        } else {
            blanks = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Incremental (in-place) markdown rendering.
//
// When an agent streams its old code accumulated the whole message
// and rendered it *once* at the end — so while the model was still "thinking",
// the terminal showed a blank spinner, and the moment it finished the entire
// (possibly multi-screen) markdown popped in at once. This struct instead
// re-renders the markdown **in place** as tokens arrive: it jumps the cursor
// back to the top of the block it drew previously and overwrites it with the
// fresh render, so the user watches the formatted markdown grow rather than a
// spinner, and (crucially) never sees the same lines duplicated or stacked.
//
// Redraws are throttled to [`Self::DEFAULT_THROTTLE`] (200 ms) so a fast token
// firehose can't saturate the terminal with a render per byte; the final state
// is always flushed via [`Self::flush`] at turn boundaries. When disabled (or
// not a tty) it is a no-op and the caller renders the finished text once.
// ---------------------------------------------------------------------------

/// Default minimum gap between incremental redraws.
pub const DEFAULT_THROTTLE_MS: u64 = 200;

/// Override the incremental-render throttle window, in milliseconds, via the
/// `PIR_INCREMENTAL_MD_THROTTLE_MS` environment variable. Anything below 1 is
/// clamped to the default so a bad value can't make the renderer spin. Falls
/// back to [`DEFAULT_THROTTLE_MS`].
fn throttle_from_env() -> u64 {
    std::env::var("PIR_INCREMENTAL_MD_THROTTLE_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT_THROTTLE_MS)
}

pub struct IncrementalMarkdown {
    /// Whether incremental rendering is active at all (off ⇒ no-op).
    enabled: bool,
    /// Whether to apply ANSI colour in the render.
    color: bool,
    /// The full accumulated markdown text so far (re-rendered whole each time).
    pending: String,
    /// Earliest instant the next throttled redraw may fire.
    next_redraw: Instant,
    /// Redraw throttle window in milliseconds. Defaults to
    /// [`DEFAULT_THROTTLE_MS`] (200ms) and is configurable at construction /
    /// per-renderer via `PIR_INCREMENTAL_MD_THROTTLE_MS` (and re-armable for
    /// tests via [`Self::set_throttle`]).
    throttle_ms: u64,
    /// Rows the last drawn block occupied (so we can jump back over it).
    last_height: usize,
    /// Whether we have drawn at least one block yet this call.
    written: bool,
    /// Each redraw, as the exact bytes handed to the terminal writer (escape
    /// prefix + rendered block). Kept for tests/inspection; in production these
    /// are also written to stdout.
    frames: Vec<String>,
}

impl IncrementalMarkdown {
    /// Build a renderer. `enabled` is normally `tty && !quiet`; it can be
    /// turned off with `--no-incremental` / `PIR_INCREMENTAL_MD=0`. `color`
    /// mirrors `term::color_enabled()`.
    pub fn new(enabled: bool, color: bool) -> Self {
        IncrementalMarkdown {
            enabled,
            color,
            pending: String::new(),
            next_redraw: Instant::now(),
            throttle_ms: throttle_from_env(),
            last_height: 0,
            written: false,
            frames: Vec::new(),
        }
    }

    /// Override whether incremental rendering is on (used by tests / late cfg).
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// Whether this renderer will actually draw incrementally.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Override the throttle window (tests set this to 0 to force a redraw per
    /// token, or to a large value to confirm batching). Re-arms so the change
    /// takes effect on the next `push`.
    pub fn set_throttle(&mut self, d: Duration) {
        self.throttle_ms = d.as_millis() as u64;
        self.next_redraw = Instant::now() + d;
    }

    /// Throttle window (exposed for tests / diagnostics).
    pub fn throttle(&self) -> Duration {
        Duration::from_millis(self.throttle_ms)
    }

    /// Append streamed markdown `t` and redraw if the throttle window has
    /// elapsed. No-op (no redraw) when disabled, but `pending()` is *always*
    /// accumulated so a disabled caller can still do one final render from the
    /// full markdown once the turn completes.
    pub fn push(&mut self, t: &str) {
        self.pending.push_str(t);
        if !self.enabled {
            return;
        }
        if self.pending.trim().is_empty() {
            return;
        }
        if Instant::now() >= self.next_redraw {
            self.redraw();
        }
    }

    /// Force an immediate redraw (ignoring the throttle). Call at turn
    /// boundaries so the final markdown is always shown exactly, even if the
    /// last token arrived mid-throttle. No-op when disabled.
    pub fn flush(&mut self) {
        if !self.enabled {
            return;
        }
        if self.pending.trim().is_empty() {
            // Nothing to draw: if we had drawn a block before, erase it cleanly.
            if self.written && self.last_height > 0 {
                self.frames.push(format!("\x1b[{}A\x1b[J", self.last_height));
                self.write_frames();
                self.written = false;
                self.last_height = 0;
            }
            return;
        }
        self.redraw();
    }

    /// Jump the cursor back over the previously drawn block and overwrite it
    /// with the freshly rendered markdown.
    fn redraw(&mut self) {
        let rendered = render(&self.pending, self.color);
        let height = rendered.lines().count().max(1);
        let mut s = String::with_capacity(rendered.len() + 16);
        if self.written && self.last_height > 0 {
            // Jump back to the top of the block we drew and erase from there to
            // the end of the screen — that removes the old render (and any
            // wrapped overflow) in one move, so the new render can't stack on
            // top of it.
            s.push_str(&format!("\x1b[{}A\x1b[J", self.last_height));
        }
        s.push_str(&rendered);
        self.frames.push(s);
        self.write_frames();
        self.last_height = height;
        self.written = true;
        self.next_redraw = Instant::now() + Duration::from_millis(self.throttle_ms);
    }

    /// Emit the most recent redraw frame to the terminal. Earlier frames are
    /// retained in `frames()` for inspection/tests; only the newest one changes
    /// the visible screen (it carries the jump-back escape that overwrites the
    /// prior block). Writing here goes straight via `term::out`.
    fn write_frames(&self) {
        if let Some(last) = self.frames.last() {
            crate::term::out(last);
        }
    }

    /// The redraw frames produced so far (escape prefix + rendered markdown),
    /// in order. The first frame has no jump-back escape (it's the initial
    /// draw); every later frame begins with `\x1b[<n>A` (jump up `n` rows) to
    /// overwrite the previous block in place. Test-only / introspection.
    pub fn frames(&self) -> &[String] {
        &self.frames
    }

    /// The full accumulated markdown text (for assertions / final fallbacks).
    pub fn pending(&self) -> &str {
        &self.pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_renders_with_hashes() {
        let r = render("# Title\n", false);
        assert!(r.contains("# Title"), "got: {r:?}");
    }

    #[test]
    fn bold_strips_asterisks() {
        let r = render("this is **bold** text", false);
        assert!(!r.contains("**"), "got: {r:?}");
        assert!(r.contains("bold"), "got: {r:?}");
    }

    #[test]
    fn fenced_code_is_fenced() {
        let md = "```rust\nlet x = 1;\n```\n";
        let r = render(md, false);
        assert!(r.contains("```rust"), "got: {r:?}");
        assert!(r.contains("let x = 1;"), "got: {r:?}");
    }

    #[test]
    fn list_markers_present() {
        let r = render("- a\n- b\n", false);
        assert!(r.contains("• a"), "got: {r:?}");
        assert!(r.contains("• b"), "got: {r:?}");
    }

    #[test]
    fn link_shows_url() {
        let r = render("[pir](https://example.com)", false);
        assert!(r.contains("https://example.com"), "got: {r:?}");
    }

    #[test]
    fn no_color_has_no_escape() {
        let r = render("# H\n\n**b** and `code`", false);
        assert!(!r.contains('\x1b'), "got: {r:?}"); }

    #[test]
    fn full_sample_renders_without_raw_markers() {
        let md = "# Plan\n\nWe will **ship** the thing and `fix` the _parser_.\n\n## Steps\n\n1. read the file\n2. edit it\n\n- support **markdown**\n- drop raw `**`\n\n```rust\nlet x = 1;\n```\n\n> a note\n\nsee [docs](https://example.com)\n";
        let out = render(md, false);
        // No raw emphasis/italic markdown left (inline `**` inside a code span
        // is intentionally preserved as literal text).
        assert!(!out.contains("**markdown**"), "got: {out:?}");
        assert!(!out.contains("_parser_"), "got: {out:?}");
        // The backtick in the inline code span keeps its literal `**`.
        assert!(out.contains("`**`"), "got: {out:?}");
        assert!(out.contains("# Plan"), "got: {out:?}");
        assert!(out.contains("## Steps"), "got: {out:?}");
        assert!(out.contains("```rust"), "got: {out:?}");
        assert!(out.contains("let x = 1;"), "got: {out:?}");
        assert!(out.contains("> a note"), "got: {out:?}");
        assert!(out.contains("https://example.com"), "got: {out:?}");
        // Ordered + unordered list markers both present.
        assert!(out.contains("1. read the file"), "got: {out:?}");
        assert!(out.contains("• support"), "got: {out:?}");
    }
}

#[cfg(test)]
mod incremental_tests {
    use super::*;
    use std::time::Duration;

    // Default throttle is 200ms (the ceiling requested by the PR: redraw at
    // most once every 200ms so a fast token stream can't flood the terminal).
    #[test]
    fn default_throttle_is_200ms() {
        assert_eq!(DEFAULT_THROTTLE_MS, 200);
        let r = IncrementalMarkdown::new(true, false);
        assert_eq!(r.throttle(), Duration::from_millis(200));
    }

    // The throttle window is configurable: `PIR_INCREMENTAL_MD_THROTTLE_MS` in
    // the environment overrides the 200ms default at renderer construction.
    #[test]
    fn throttle_is_configurable_via_env() {
        unsafe {
            std::env::set_var("PIR_INCREMENTAL_MD_THROTTLE_MS", "5");
        }
        let r = IncrementalMarkdown::new(true, false);
        assert_eq!(r.throttle(), Duration::from_millis(5));
        unsafe {
            std::env::remove_var("PIR_INCREMENTAL_MD_THROTTLE_MS");
        }
        let r2 = IncrementalMarkdown::new(true, false);
        assert_eq!(r2.throttle(), Duration::from_millis(200), "unsets to default");
        // A garbage/bad value (e.g. 0) falls back to the default rather than
        // letting the renderer redraw every byte (busy-spin) or panic.
        unsafe {
            std::env::set_var("PIR_INCREMENTAL_MD_THROTTLE_MS", "0");
        }
        let r3 = IncrementalMarkdown::new(true, false);
        assert_eq!(r3.throttle(), Duration::from_millis(200), "bad value falls back to default");
        unsafe {
            std::env::set_var("PIR_INCREMENTAL_MD_THROTTLE_MS", "not-a-number");
        }
        let r4 = IncrementalMarkdown::new(true, false);
        assert_eq!(r4.throttle(), Duration::from_millis(200), "non-numeric falls back to default");
        unsafe {
            std::env::remove_var("PIR_INCREMENTAL_MD_THROTTLE_MS");
        }
    }

    // On by default: a renderer built `enabled` starts drawing immediately, and
    // a flurry of tokens within 200ms collapses to (at most) one redraw.
    #[test]
    fn enabled_by_default_batches_within_window() {
        let mut r = IncrementalMarkdown::new(true, false);
        // Leave the default 200ms throttle in place.
        assert!(r.enabled());
        r.push("# A");
        let after_first = r.frames().len();
        // Burst of pushes inside the 200ms window must NOT trigger a redraw.
        for w in ["## B", "\n- c", "\n- d", "\nmore"] {
            r.push(w);
        }
        assert_eq!(
            r.frames().len(),
            after_first,
            "all bursts within 200ms should be throttled to the single initial draw"
        );
        assert_eq!(r.pending(), "# A## B\n- c\n- d\nmore", "accumulated markdown retained for the final flush");
        r.flush();
        // After a flush the final frame overwrites the previous block in place.
        assert!(r.frames().last().unwrap().starts_with('\x1b'));
    }

    // The renderer *jumps back and overwrites* the existing markdown: each new
    // frame begins with a cursor-up escape that clears the prior block, so the
    // same text pushed again never stacks — the body shows each line once.
    #[test]
    fn later_frames_jump_back_to_overwrite() {
        let mut r = IncrementalMarkdown::new(true, false);
        r.set_throttle(Duration::ZERO);
        r.push("# Title");
        assert!(!r.frames()[0].starts_with('\x1b'), "first draw needs no jump-back");
        r.push("\n\nbody text");
        let later = r.frames().last().unwrap();
        assert!(later.starts_with("\x1b["), "later frame must jump the cursor back: {later:?}");
        assert!(later.contains("[1A"), "jump-back must move up N rows: {later:?}");
        // The body is the full current markdown, rendered once — not doubled.
        assert_eq!(later.matches("# Title").count(), 1);
        assert_eq!(later.matches("body text").count(), 1);
    }

    // Throttling: a flurry of token `push`es within the window yields a single
    // redraw (the first one), not one render.
    #[test]
    fn redraws_are_throttled() {
        let mut r = IncrementalMarkdown::new(true, false);
        r.set_throttle(Duration::from_secs(60)); // effectively disable re-redraws
        r.push("# Title");
        let after_first = r.frames().len();
        r.push("\n\npara one");
        r.push("\n\npara two");
        r.push("\n\npara three");
        assert_eq!(r.frames().len(), after_first, "no new frame should be drawn within the throttle window");
    }

    // A forced flush after redraws (some throttled) draws the full, final text
    // in place over the last block. With a 0ms throttle every push redraws, so
    // the final flush frame is *not the first* and must jump back + overwrite.
    #[test]
    fn flush_draws_final_state() {
        let mut r = IncrementalMarkdown::new(true, false);
        r.set_throttle(Duration::ZERO);
        r.push("# Title");
        let first = r.frames().len();
        r.push("\n\nmore content");
        assert_eq!(r.frames().len(), first + 1, "each push redraws with a 0ms throttle");
        r.flush();
        assert_eq!(r.frames().len(), first + 2, "flush must force a final redraw");
        // The flushed frame carries the full accumulated markdown.
        let last = r.frames().last().unwrap();
        assert!(last.contains("# Title"), "flush must show the title: {last:?}");
        assert!(last.contains("more content"), "flush must show the appended text: {last:?}");
        // The last frame jumps back over the previous block (it is not the first).
        assert!(last.starts_with('\x1b'), "flush frame must overwrite in place: {last:?}");
    }

    // Disabled renderer never draws — the caller is responsible for the single
    // final render. `pending()` still accumulates so a final fallback works.
    #[test]
    fn disabled_is_a_noop_but_accumulates() {
        let mut r = IncrementalMarkdown::new(false, false);
        r.set_throttle(Duration::ZERO);
        r.push("# Nothing");
        r.push("\n\nrendered yet");
        assert_eq!(r.frames().len(), 0, "disabled renderer must not draw");
        assert_eq!(r.pending(), "# Nothing\n\nrendered yet", "but it must still accumulate");
        r.flush();
        assert_eq!(r.frames().len(), 0, "flush is a no-op when disabled");
    }

    // Re-rendering is idempotent: the same markdown pushed twice (after a flush
    // re-arm) produces a block whose rendered body represents the latest text,
    // not a concatenation of earlier bodies — i.e. there is no *visible* stacking
    // because each frame starts by jumping back and erasing the prior screen.
    #[test]
    fn no_duplicate_visible_lines() {
        let mut r = IncrementalMarkdown::new(true, false);
        r.set_throttle(Duration::ZERO);
        r.push("- a\n- b\n- c");
        r.push("\n- d");
        r.flush();
        // The final frame's *rendered body* (after the escape prefix) must only
        // contain the list items once each — the old block was erased, not
        // appended, so we never see "- a" twice in a single frame.
        let last = r.frames().last().unwrap();
        let body = last.trim_start_matches('\x1b').trim_start_matches('[').split('A').nth(1).unwrap_or(last.as_str());
        assert_eq!(body.matches("• a").count(), 1, "item a duplicated in final frame: {body:?}");
        assert_eq!(body.matches("• d").count(), 1, "item d missing/duplicated: {body:?}");
    }
}
