//! Minimal terminal Markdown renderer.
//!
//! Agent replies are CommonMark (headings, `**bold**`, lists, fenced code
//! blocks, links, …). The REPL used to dump them verbatim, so the user saw
//! raw `**`/`#`/` ``` ` instead of formatted text. This module parses the
//! markdown and emits a plain-text approximation suitable for a fixed-width
//! terminal: headings get a marker + bold, emphasis becomes ANSI emphasis,
//! code blocks are fenced and indented, links show their text with the URL in
//! brackets, and list/quote markers are preserved.
//!
//! The output is *not* HTML — it is text with ANSI styling, so it can be
//! printed by the streaming REPL (`term::out`) and by the TUI (each paragraph
//! becomes a `Line`). Colour is applied only when the terminal supports it.
//!
//! # Backends
//!
//! There are two interchangeable parsers, selected at runtime via
//! `config::markdown_renderer_backend()` (the `PIR_MARKDOWN_RENDERER` env var
//! or `markdownRenderer` in settings.json):
//!
//!   * **pulldown** (default) — `pulldown-cmark`, an event-stream CommonMark
//!     + GFM parser with a minimal dependency tree; always compiled in.
//!   * **comrak** (optional) — the heavier AST parser, compiled only when the
//!     `comrak-backend` cargo feature is enabled.
//!
//! Both feed the same small set of terminal ANSI emitters, so flipping the
//! backend only changes the parser, never the output shape.

#[cfg(feature = "comrak-backend")]
use comrak::nodes::{AstNode, ListType, NodeValue};
#[cfg(feature = "comrak-backend")]
use comrak::{parse_document, Arena as ComrakArena, Options as ComrakOptions};
use pulldown_cmark::{Event as PdEvent, Options as PdOptions, Parser as PdParser, Tag as PdTag, TagEnd as PdTagEnd};
use std::time::{Duration, Instant};
use streamdown_parser::{InlineElement, InlineParser, ListBullet, ParseEvent, Parser as SdParser};
use streamdown_ansi::sanitize::{sanitize_for_terminal, sanitize_url};

/// Render `md` to a styled, terminal-width-friendly `String`. Block-level
/// structure (headings, lists, code fences, blank lines) is preserved; inline
/// emphasis is rendered with ANSI bold/italic when `color` is true.
///
/// The backend is chosen by [`crate::config::markdown_renderer_backend`]. When
/// the `comrak-backend` feature is *not* compiled in, only the pulldown backend
/// exists and the configuration is effectively pinned to it (a `comrak` setting
/// degrades to the default rather than failing — the terminal output is
/// compatible).
pub fn render(md: &str, color: bool) -> String {
    #[cfg(feature = "comrak-backend")]
    if crate::config::markdown_renderer_backend() == "comrak" {
        return render_comrak(md, color);
    }
    render_pulldown(md, color)
}

/// Default pulldown-cmark backend (always compiled; see [`render`]).
fn render_pulldown(md: &str, color: bool) -> String {
    let mut opts = PdOptions::empty();
    opts.insert(PdOptions::ENABLE_TABLES);
    opts.insert(PdOptions::ENABLE_STRIKETHROUGH);
    opts.insert(PdOptions::ENABLE_TASKLISTS);
    // Smart punctuation is on (matches the comrak backend's `parse.smart`). The
    // scanned agent output already carries frequent smart quotes / em-dashes;
    // enabling this keeps straight quotes and `--` typographically consistent.
    // `autolink` is deliberately NOT enabled: the scanned sessions contain no
    // bare URLs, so the extra pass buys nothing.
    opts.insert(PdOptions::ENABLE_SMART_PUNCTUATION);

    let parser = PdParser::new_ext(md, opts);
    let mut r = PdRenderer { color, ..Default::default() };
    r.run(parser);
    let collapsed = collapse_blank(&r.out);
    collapsed.trim_end().to_string() + "\n"
}

// ---------------------------------------------------------------------------
// pulldown-cmark backend (default)
// ---------------------------------------------------------------------------

/// Holds terminal-emitter state while walking pulldown's event stream.
#[derive(Default)]
struct PdRenderer {
    color: bool,
    /// Whether we're inside a list item (so paragraph breaks don't add blanks).
    in_list: bool,
    /// Whether we're inside a blockquote (SoftBreak becomes a `> ` line).
    in_blockquote: bool,
    /// Per-nesting-level ordered/bullet list state: `Some((start, count))` for
    /// an ordered list (starting at `start`), `None` for a bullet list.
    list_stack: Vec<Option<(u64, u64)>>,
    /// Pending link URL (set on Start(Link), appended after text on End).
    pending_link: Option<String>,
    /// Table capture state (active only inside a table).
    table_active: bool,
    table_in_cell: bool,
    table_cells: Vec<Vec<String>>,
    table_cur_cell: String,
    /// The accumulated output.
    out: String,
}

impl PdRenderer {
    fn run(&mut self, parser: PdParser<'_>) {
        for ev in parser {
            match ev {
                PdEvent::Start(tag) => self.on_start(tag),
                PdEvent::End(end) => self.on_end(end),
                PdEvent::Text(t) => self.push_text(&t),
                PdEvent::Code(t) => {
                    if self.color {
                        self.push_text(&format!("\x1b[7m{}\x1b[0m", t));
                    } else {
                        self.push_text(&format!("`{}`", t));
                    }
                }
                PdEvent::SoftBreak | PdEvent::HardBreak => {
                    if self.in_blockquote {
                        self.out.push('\n');
                        self.out.push_str("> ");
                    } else {
                        self.out.push('\n');
                    }
                }
                PdEvent::Rule => self.push_text("---\n"),
                PdEvent::Html(t) => {
                    // Keep literal inline HTML as-is (matches terminal expectations).
                    self.push_text(&t);
                }
                // FootnoteReference / InlineMath / DisplayMath etc. are not used
                // by the terminal renderer; ignore them.
                _ => {}
            }
        }
        if self.table_active {
            self.flush_table();
        }
    }

    /// Route a text/plain segment into the current table cell (if any) or the
    /// main output.
    fn push_text(&mut self, s: &str) {
        if self.table_in_cell {
            self.table_cur_cell.push_str(s);
        } else {
            self.out.push_str(s);
        }
    }

    fn on_start(&mut self, tag: PdTag<'_>) {
        match tag {
            PdTag::Heading { level, .. } => {
                let n = level as usize;
                self.out.push_str(&format!("{} ", "#".repeat(n.min(6))));
                if self.color {
                    self.out.push_str("\x1b[1m");
                }
            }
            PdTag::Paragraph => {}
            PdTag::BlockQuote(_) => {
                self.in_blockquote = true;
                self.out.push_str("> ");
            }
            PdTag::CodeBlock(kind) => {
                let lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(info) => info.trim().to_string(),
                    pulldown_cmark::CodeBlockKind::Indented => String::new(),
                };
                self.out.push_str(&format!("```{lang}\n"));
            }
            PdTag::List(start) => {
                // A nested list that opens mid-line must start on a fresh line.
                if !self.out.is_empty() && !self.out.ends_with('\n') {
                    self.out.push('\n');
                }
                // Track the current nest depth for indentation. Ordered lists
                // carry their start number; bullet lists are `None`.
                self.list_stack.push(start.map(|s| (s, 0u64)));
            }
            PdTag::Item => {
                self.in_list = true;
                let marker = self.item_marker();
                self.out.push_str(&marker);
            }
            PdTag::Table(_) => {
                if self.table_active {
                    self.flush_table();
                }
                self.table_active = true;
                self.table_cells.clear();
            }
            PdTag::TableHead => self.table_cells.push(Vec::new()),
            PdTag::TableRow => self.table_cells.push(Vec::new()),
            PdTag::TableCell => {
                self.table_in_cell = true;
                self.table_cur_cell.clear();
            }
            PdTag::Emphasis => self.open_style("\x1b[3m"),
            PdTag::Strong => self.open_style("\x1b[1m"),
            PdTag::Strikethrough => self.open_style("\x1b[9m"),
            PdTag::Link { dest_url, .. } => {
                self.pending_link = Some(dest_url.to_string());
            }
            PdTag::Image { title, .. } => {
                let t = title.trim().to_string();
                if !t.is_empty() {
                    self.push_text(&format!("[image: {t}]"));
                }
            }
            _ => {}
        }
    }

    fn on_end(&mut self, end: PdTagEnd) {
        match end {
            PdTagEnd::Heading(_) | PdTagEnd::Emphasis | PdTagEnd::Strong | PdTagEnd::Strikethrough => {
                if self.color {
                    self.out.push_str("\x1b[0m");
                }
                if matches!(end, PdTagEnd::Heading(_)) {
                    self.out.push('\n');
                }
            }
            PdTagEnd::Paragraph => {
                // In a tight list the items flow without blank lines.
                if !self.in_list {
                    self.out.push('\n');
                }
            }
            PdTagEnd::BlockQuote(_) => {
                self.in_blockquote = false;
                self.out.push('\n');
            }
            PdTagEnd::CodeBlock => self.out.push_str("```\n"),
            PdTagEnd::List(_) => {
                self.list_stack.pop();
            }
            PdTagEnd::Item => {
                self.in_list = false;
                self.out.push('\n');
            }
            PdTagEnd::Table => {                self.flush_table();
                self.table_active = false;
            }
            PdTagEnd::TableHead => {}
            PdTagEnd::TableRow => {}
            PdTagEnd::TableCell => {
                if self.table_active && !self.cell_empty() {
                    if let Some(row) = self.table_cells.last_mut() {
                        let cell =
                            self.table_cur_cell.replace('\n', " ").trim().to_string();
                        row.push(cell);
                    }
                }
                self.table_in_cell = false;
            }
            PdTagEnd::Link => {
                if let Some(url) = self.pending_link.take() {
                    self.out.push_str(&format!(" [{url}]"));
                }
            }
            PdTagEnd::Image => {}
            _ => {}
        }
    }

    fn cell_empty(&self) -> bool {
        self.table_cur_cell.trim().is_empty()
    }

    fn item_marker(&mut self) -> String {
        // Top of stack is the innermost list currently being iterated. Nested
        // lists are indented by two spaces per level so the markers don't jam.
        let depth = self.list_stack.len();
        let pad = "  ".repeat(depth.saturating_sub(1));
        match self.list_stack.last_mut() {
            Some(Some((base, count))) => {
                let idx = *count;
                *count += 1;
                format!("{pad}{}. ", *base + idx)
            }
            // Bullet list (None) or not inside a list.
            _ => format!("{pad}• "),
        }
    }

    fn open_style(&mut self, code: &str) {
        if self.color {
            self.out.push_str(code);
        }
    }

    /// Emit a collected table as a monospace grid (mirrors the comrak backend).
    fn flush_table(&mut self) {
        if self.table_cells.is_empty() {
            return;
        }
        let rows = std::mem::take(&mut self.table_cells);
        let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let widths: Vec<usize> = (0..cols)
            .map(|ci| rows.iter().map(|r| r.get(ci).map_or(0, |s| s.chars().count())).max().unwrap_or(0))
            .collect();
        for (ri, row) in rows.iter().enumerate() {
            for (ci, w) in widths.iter().enumerate() {
                let cell = row.get(ci).cloned().unwrap_or_default();
                self.out.push('|');
                self.out.push(' ');
                self.out.push_str(&cell);
                let pad = w.saturating_sub(cell.chars().count());
                for _ in 0..pad {
                    self.out.push(' ');
                }
                self.out.push(' ');
            }
            self.out.push_str("|\n");
            if ri == 0 {
                self.out.push('|');
                for w in &widths {
                    for _ in 0..w + 2 {
                        self.out.push('-');
                    }
                    self.out.push('|');
                }
                self.out.push('\n');
            }
        }
        self.out.push('\n');
    }
}

// ---------------------------------------------------------------------------
// comrak backend (optional — only compiled with `--features comrak-backend`)
// ---------------------------------------------------------------------------

/// Render `md` using the (optional) comrak parser. Equivalent output shape to
/// the default pulldown backend; only compiled when the `comrak-backend`
/// feature is enabled.
#[cfg(feature = "comrak-backend")]
fn render_comrak(md: &str, color: bool) -> String {
    let mut opts = ComrakOptions::default();
    opts.extension.strikethrough = true;
    opts.extension.table = true;
    opts.extension.autolink = true;
    opts.extension.tasklist = true;
    opts.parse.smart = true;

    let arena = ComrakArena::new();
    let root = parse_document(&arena, md, &opts);

    let mut out = String::new();
    render_node(root, &RenderCtx { color, indent: 0, in_list: false }, &mut out);
    let collapsed = collapse_blank(&out);
    collapsed.trim_end().to_string() + "\n"
}

#[cfg(feature = "comrak-backend")]
struct RenderCtx {
    color: bool,
    /// Current block indent (for nested lists).
    indent: usize,
    /// Whether we are inside a list (so children render as items).
    in_list: bool,
}

#[cfg(feature = "comrak-backend")]
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
        _ => {
            for c in node.children() {
                render_node(c, ctx, out);
            }
        }
    }
}

/// Render a list item (comrak backend). The marker is printed once; wrapped
/// lines align under the text.
#[cfg(feature = "comrak-backend")]
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
        out.push_str(marker);
        out.push('\n');
    }
}

/// Render a GFM table as a simple monospace grid (comrak backend).
#[cfg(feature = "comrak-backend")]
fn render_table<'a>(node: &'a AstNode<'a>, ctx: &RenderCtx, out: &mut String) {
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
    /// Streaming renderer (O(n)) — when present, `redraw` renders only the new
    /// tail instead of re-rendering the whole buffer. Built lazily on first
    /// redraw so a disabled renderer never pays for it.
    stream: Option<StreamingRenderer>,
    /// Byte offset into `pending` already fed to the streaming renderer.
    last_rendered: usize,
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
            stream: None,
            last_rendered: 0,
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
        // O(n) streaming path: parse only the new tail since the last redraw,
        // but emit the *full* accumulated output (the streaming renderer holds
        // it) so the frame overwrites the previous block completely. The
        // streaming renderer carries open-block state, so a line that continues
        // a code fence / list / blockquote renders correctly without re-parsing
        // the whole buffer. Falls back to the O(n²) whole-buffer `render` on the
        // first redraw (to seed the streaming renderer's output).
        let rendered = if let Some(stream) = self.stream.as_mut() {
            stream.push(&self.pending[self.last_rendered..]);
            self.last_rendered = self.pending.len();
            stream.output().to_string()
        } else {
            let rendered = render(&self.pending, self.color);
            let mut stream = StreamingRenderer::new(self.color);
            stream.push(&self.pending);
            self.stream = Some(stream);
            self.last_rendered = self.pending.len();
            rendered
        };
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

// ---------------------------------------------------------------------------
// O(n) streaming renderer (streamdown-parser backend).
//
// The pulldown `IncrementalMarkdown` above re-renders the *whole* accumulated
// buffer on every throttled redraw — O(n²) over a long reply. This backend
// instead feeds each new line to a stateful `streamdown_parser::Parser` and
// renders only the *new* `ParseEvent`s, so the total work is O(n). The parser
// carries open-block state (code fence, list, blockquote, table) across calls,
// so a line that continues a block renders correctly without re-parsing the
// prefix.
//
// Output shape matches the pulldown backend (headings `# `+bold, `• ` lists,
// fenced code, `> ` quotes, `text [url]` links) so the two are interchangeable.
// ---------------------------------------------------------------------------

/// Streaming markdown renderer: feed lines, get ANSI output for just the new
/// content. Keeps pir's terminal-emitter conventions (colour gated on `color`).
pub struct StreamingRenderer {
    parser: SdParser,
    inline: InlineParser,
    color: bool,
    /// Pending link URL (set on Link, appended after text on the same event).
    pending_link: Option<String>,
    /// Current list nesting depth (for indentation).
    list_depth: usize,
    /// Whether we're inside a blockquote (for `> ` prefix on continuation lines).
    in_blockquote: bool,
    /// Table buffering: streamdown emits per-row events, but we must align
    /// columns (compute the widest cell per column) before emitting, like the
    /// pulldown backend's `flush_table`. Rows are buffered until `TableEnd`.
    table_rows: Vec<Vec<String>>,
    /// When inside a ```md / ```markdown fence, buffer the lines here and
    /// re-render them as markdown on the closing fence (instead of as literal
    /// code), so a markdown table shown as `md` source renders. Also buffers
    /// bare fences (empty language) to detect a `markdown`-on-next-line form.
    md_fence: Option<Vec<String>>,
    /// Whether the current fence's language (on the fence line) is md/markdown.
    md_fence_is_md: bool,
    out: String,
}

impl StreamingRenderer {
    /// Build a streaming renderer. `color` mirrors `term::color_enabled()`.
    pub fn new(color: bool) -> Self {
        StreamingRenderer {
            parser: SdParser::new(),
            inline: InlineParser::new(),
            color,
            pending_link: None,
            list_depth: 0,
            in_blockquote: false,
            table_rows: Vec::new(),
            md_fence: None,
            md_fence_is_md: false,
            out: String::new(),
        }
    }

    /// Feed one line of markdown and render the new events it produces.
    pub fn push_line(&mut self, line: &str) {
        let events = self.parser.parse_line(line);
        for ev in events {
            self.on_event(ev);
        }
    }

    /// Feed a chunk of markdown (may contain newlines); splits on lines and
    /// feeds each. Returns the newly rendered output.
    pub fn push(&mut self, chunk: &str) -> String {
        let start = self.out.len();
        for line in chunk.split('\n') {
            self.push_line(line);
        }
        self.out[start..].to_string()
    }

    /// Close any open blocks (call at end of stream) and return the tail.
    pub fn finalize(&mut self) -> String {
        let start = self.out.len();
        for ev in self.parser.finalize() {
            self.on_event(ev);
        }
        self.out[start..].to_string()
    }

    /// The full rendered output so far.
    pub fn output(&self) -> &str {
        &self.out
    }

    fn on_event(&mut self, ev: ParseEvent) {
        match ev {
            ParseEvent::Text(t) => self.out.push_str(&t),
            ParseEvent::InlineCode(c) => {
                if self.color {
                    self.out.push_str(&format!("\x1b[7m{}\x1b[0m", c));
                } else {
                    self.out.push_str(&format!("`{}`", c));
                }
            }
            ParseEvent::Bold(t) => self.styled("\x1b[1m", &t),
            ParseEvent::Italic(t) => self.styled("\x1b[3m", &t),
            ParseEvent::BoldItalic(t) => self.styled("\x1b[1;3m", &t),
            ParseEvent::Underline(t) => self.styled("\x1b[4m", &t),
            ParseEvent::Strikeout(t) => self.styled("\x1b[9m", &t),
            ParseEvent::Link { text, url } => {
                let text = sanitize_for_terminal(&text);
                self.out.push_str(&text);
                // Only emit the URL if it's safe for terminal hyperlinks (no
                // escape-sequence injection, safe scheme).
                if let Some(url) = sanitize_url(&url) {
                    if !url.is_empty() && url != text {
                        self.out.push_str(&format!(" [{url}]"));
                    }
                }
            }
            ParseEvent::Image { alt, .. } => {
                if !alt.is_empty() {
                    self.out.push_str(&format!("[image: {alt}]"));
                }
            }
            ParseEvent::Footnote(f) => {
                self.out.push_str(&format!("[^{f}]"));
            }
            ParseEvent::Heading { level, content } => {
                let n = (level as usize).min(6);
                let prefix = format!("{} ", "#".repeat(n));
                if self.color {
                    self.out.push_str(&format!("\x1b[1m{prefix}{content}\x1b[0m\n"));
                } else {
                    self.out.push_str(&format!("{prefix}{content}\n"));
                }
            }
            ParseEvent::CodeBlockStart { language, .. } => {
                let lang = language.clone().unwrap_or_default();
                // A ```md / ```markdown fence (very common from LLMs showing the
                // markdown *source* of a reply) is meant to be displayed *as*
                // markdown, not as literal code. Two forms:
                //   1. ```markdown  (language on the fence line)
                //   2. ```\nmarkdown (bare fence, language on the next line)
                // Both are buffered; the md marker is dropped at the end and the
                // rest is re-rendered as markdown. Non-md fences (rust, python,
                // ...) render as literal code with their markers.
                let is_md_lang = lang == "md" || lang == "markdown";
                // For a bare fence (empty lang) we buffer anyway and decide on
                // `md`-marker at CodeBlockEnd, so form #2 works too.
                self.md_fence = Some(Vec::new());
                self.md_fence_is_md = is_md_lang;
                if !is_md_lang && !lang.is_empty() {
                    self.out.push_str(&format!("```{lang}\n"));
                }
            }
            ParseEvent::CodeBlockLine(l) => {
                if let Some(buf) = self.md_fence.as_mut() {
                    buf.push(l);
                } else {
                    self.out.push_str(&l);
                    self.out.push('\n');
                }
            }
            ParseEvent::CodeBlockEnd => {
                let Some(mut buf) = self.md_fence.take() else {
                    self.out.push_str("```\n");
                    return;
                };
                let is_md = self.md_fence_is_md
                    || matches!(buf.first().map(|s| s.trim()), Some("md") | Some("markdown"));
                // Drop a leading `md` / `markdown` marker line (form #2).
                if !self.md_fence_is_md
                    && matches!(buf.first().map(|s| s.trim()), Some("md") | Some("markdown"))
                {
                    buf.remove(0);
                }
                if is_md {
                    // Re-render the fenced markdown verbatim (as markdown).
                    for line in buf {
                        self.push_line(&line);
                    }
                } else {
                    self.out.push_str("```\n");
                    for line in buf {
                        self.out.push_str(&line);
                        self.out.push('\n');
                    }
                    self.out.push_str("```\n");
                }
            }
            ParseEvent::ListItem { indent, bullet, content } => {
                let marker = match bullet {
                    ListBullet::Ordered(n) => format!("{n}. "),
                    _ => "• ".to_string(),
                };
                let pad = "  ".repeat(indent);
                self.out.push_str(&pad);
                self.out.push_str(&marker);
                // List item content is raw markdown — inline-parse it so `**bold**`
                // and `_em_` render like the pulldown backend.
                for el in self.inline.parse(&content) {
                    self.on_inline(el);
                }
                self.out.push('\n');
            }
            ParseEvent::ListEnd => {
                self.list_depth = self.list_depth.saturating_sub(1);
            }
            ParseEvent::TableHeader(cells) => {
                self.table_rows.push(cells);
            }
            ParseEvent::TableRow(cells) => {
                self.table_rows.push(cells);
            }
            ParseEvent::TableSeparator => {
                // The separator is implied by the aligned-grid layout; it's
                // rendered as the header underline when the table flushes.
            }
            ParseEvent::TableEnd => {
                self.flush_table();
            }
            ParseEvent::BlockquoteStart { .. } => {
                self.in_blockquote = true;
                self.out.push_str("> ");
            }
            ParseEvent::BlockquoteLine(l) => {
                self.out.push_str(&l);
                self.out.push('\n');
                self.out.push_str("> ");
            }
            ParseEvent::BlockquoteEnd => {
                self.in_blockquote = false;
                self.out.push('\n');
            }
            ParseEvent::ThinkBlockStart => {}
            ParseEvent::ThinkBlockLine(l) => {
                self.out.push_str(&l);
                self.out.push('\n');
            }
            ParseEvent::ThinkBlockEnd => {}
            ParseEvent::HorizontalRule => {
                self.out.push_str("---\n");
            }
            ParseEvent::EmptyLine => {
                self.out.push('\n');
            }
            ParseEvent::Newline => {
                self.out.push('\n');
            }
            ParseEvent::Prompt(p) => {
                self.out.push_str(&p);
            }
            ParseEvent::InlineElements(_) => {}
        }
    }

    fn styled(&mut self, code: &str, s: &str) {
        if self.color {
            self.out.push_str(code);
            self.out.push_str(s);
            self.out.push_str("\x1b[0m");
        } else {
            self.out.push_str(s);
        }
    }

    /// Render a single inline element (used for list-item content, which the
    /// streamdown parser leaves as raw markdown).
    fn on_inline(&mut self, el: InlineElement) {
        match el {
            InlineElement::Text(t) => self.out.push_str(&t),
            InlineElement::Bold(t) => self.styled("\x1b[1m", &t),
            InlineElement::Italic(t) => self.styled("\x1b[3m", &t),
            InlineElement::BoldItalic(t) => self.styled("\x1b[1;3m", &t),
            InlineElement::Underline(t) => self.styled("\x1b[4m", &t),
            InlineElement::Strikeout(t) => self.styled("\x1b[9m", &t),
            InlineElement::Code(c) => {
                if self.color {
                    self.out.push_str(&format!("\x1b[7m{}\x1b[0m", c));
                } else {
                    self.out.push_str(&format!("`{}`", c));
                }
            }
            InlineElement::Link { text, url } => {
                let text = sanitize_for_terminal(&text);
                self.out.push_str(&text);
                if let Some(url) = sanitize_url(&url) {
                    if !url.is_empty() && url != text {
                        self.out.push_str(&format!(" [{url}]"));
                    }
                }
            }
            InlineElement::Image { alt, .. } => {
                if !alt.is_empty() {
                    self.out.push_str(&format!("[image: {alt}]"));
                }
            }
            InlineElement::Footnote(f) => {
                self.out.push_str(&format!("[^{f}]"));
            }
        }
    }

    /// Emit a buffered table as a monospace grid, mirroring the pulldown
    /// backend's `flush_table`: compute the widest cell per column and pad
    /// each, with a header underline. Table rows are buffered (in `table_rows`)
    /// until `TableEnd` so column widths are known.
    fn flush_table(&mut self) {
        let rows = std::mem::take(&mut self.table_rows);
        if rows.is_empty() {
            return;
        }
        let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let widths: Vec<usize> = (0..cols)
            .map(|ci| {
                rows.iter()
                    .map(|r| r.get(ci).map_or(0, |s| crate::term::visible_len(s)))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        for (ri, row) in rows.iter().enumerate() {
            for (ci, w) in widths.iter().enumerate() {
                let cell = row.get(ci).cloned().unwrap_or_default();
                self.out.push('|');
                self.out.push(' ');
                self.out.push_str(&cell);
                let pad = w.saturating_sub(crate::term::visible_len(&cell));
                for _ in 0..pad {
                    self.out.push(' ');
                }
                self.out.push(' ');
            }
            self.out.push_str("|\n");
            if ri == 0 {
                self.out.push('|');
                for w in &widths {
                    for _ in 0..w + 2 {
                        self.out.push('-');
                    }
                    self.out.push('|');
                }
                self.out.push('\n');
            }
        }
        self.out.push('\n');
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use std::time::Instant;

    // Feed a full document line-by-line and check the streaming renderer
    // produces the same block structure as the pulldown backend.
    #[test]
    fn streaming_matches_pulldown_structure() {
        let md = "# Plan\n\nWe will **ship** the thing and `fix` the _parser_.\n\n## Steps\n\n1. read the file\n2. edit it\n\n- support **markdown**\n- drop raw `**`\n\n```rust\nlet x = 1;\n```\n\n> a note\n\nsee [docs](https://example.com)\n";
        let mut r = StreamingRenderer::new(false);
        for line in md.lines() {
            r.push_line(line);
        }
        r.finalize();
        let out = r.output();
        // No raw emphasis/italic markdown left.
        assert!(!out.contains("**markdown**"), "got: {out:?}");
        assert!(!out.contains("_parser_"), "got: {out:?}");
        assert!(out.contains("# Plan"), "got: {out:?}");
        assert!(out.contains("## Steps"), "got: {out:?}");
        assert!(out.contains("```rust"), "got: {out:?}");
        assert!(out.contains("let x = 1;"), "got: {out:?}");
        assert!(out.contains("> a note"), "got: {out:?}");
        assert!(out.contains("https://example.com"), "got: {out:?}");
        assert!(out.contains("1. read the file"), "got: {out:?}");
        assert!(out.contains("• support"), "got: {out:?}");
    }

    // Streaming across a code-fence boundary: the fence opens on one line and
    // closes later; the renderer must carry the open-block state.
    #[test]
    fn streaming_handles_code_fence_across_lines() {
        let mut r = StreamingRenderer::new(false);
        r.push_line("```rust");
        r.push_line("let x = 1;");
        r.push_line("```");
        r.finalize();
        let out = r.output();
        assert!(out.contains("```rust"), "got: {out:?}");
        assert!(out.contains("let x = 1;"), "got: {out:?}");
        assert!(out.contains("```\n"), "got: {out:?}");
    }

    // Streaming a list across lines keeps the bullet markers.
    #[test]
    fn streaming_handles_list_across_lines() {
        let mut r = StreamingRenderer::new(false);
        r.push_line("- a");
        r.push_line("- b");
        r.finalize();
        let out = r.output();
        assert!(out.contains("• a"), "got: {out:?}");
        assert!(out.contains("• b"), "got: {out:?}");
    }

    // Colour mode emits ANSI for bold/italic.
    #[test]
    fn streaming_color_emits_ansi() {
        let mut r = StreamingRenderer::new(true);
        r.push_line("**bold** and _italic_");
        r.finalize();
        let out = r.output();
        assert!(out.contains("\x1b[1m"), "got: {out:?}");
        assert!(out.contains("\x1b[3m"), "got: {out:?}");
    }

    // `push` (chunk with newlines) returns only the newly rendered tail.
    #[test]
    fn streaming_push_returns_only_new_tail() {
        let mut r = StreamingRenderer::new(false);
        let first = r.push("# Title\n");
        assert!(first.contains("# Title"), "got: {first:?}");
        let second = r.push("\nbody\n");
        assert!(!second.contains("# Title"), "tail must not repeat the title: {second:?}");
        assert!(second.contains("body"), "got: {second:?}");
    }

    // The streaming renderer is O(n): feeding a growing buffer line-by-line
    // costs ~linear total time, whereas the whole-buffer `render` is O(n²).
    // This test just sanity-checks the streaming path completes quickly on a
    // large input (it would be quadratic-slow if it re-parsed the prefix).
    #[test]
    fn streaming_is_linear_on_large_input() {
        let mut r = StreamingRenderer::new(false);
        let mut md = String::new();
        for i in 0..5000 {
            md.push_str(&format!("Token {i} **bold** and `code` with a [link](https://x.com) and _em_.\n"));
        }
        let t = Instant::now();
        for line in md.lines() {
            r.push_line(line);
        }
        r.finalize();
        let d = t.elapsed();
        assert!(d.as_secs() < 5, "streaming render took too long: {d:?}");
        assert!(r.output().len() > 100_000, "output too small: {}", r.output().len());
    }

    // Sanitize: unsafe URLs (javascript:, control chars) are dropped from the
    // output; safe URLs (https:) are kept.
    #[test]
    fn streaming_sanitizes_unsafe_links() {
        let mut r = StreamingRenderer::new(false);
        r.push_line("[safe](https://example.com)");
        r.push_line("[bad](javascript:alert(1))");
        r.push_line("[esc](https://evil.com\x1b]0;pwned\x07)");
        r.finalize();
        let out = r.output();
        assert!(out.contains("https://example.com"), "safe url dropped: {out:?}");
        assert!(!out.contains("javascript:"), "unsafe scheme leaked: {out:?}");
        assert!(!out.contains("pwned"), "escape injection leaked: {out:?}");
        assert!(!out.contains('\x1b'), "escape sequence leaked: {out:?}");
    }

    // A markdown table renders as an aligned monospace grid (padded to the
    // widest cell per column) with a header underline — not raw `| a | b |`
    // rows. This is the fix for "why does a table not render as md".
    #[test]
    fn streaming_renders_table_as_aligned_grid() {
        let md = "| Column A | Column B | Column C |\n| :--- | :---: | ---: |\n| Left aligned | Centered | Right aligned |\n| Data 1 | Data 2 | Data 3 |\n| Data 4 | Data 5 | Data 6 |\n";
        let mut r = StreamingRenderer::new(false);
        for line in md.lines() {
            r.push_line(line);
        }
        r.finalize();
        let out = r.output();
        // All cells present.
        assert!(out.contains("Column A"), "got: {out:?}");
        assert!(out.contains("Left aligned"), "got: {out:?}");
        assert!(out.contains("Data 6"), "got: {out:?}");
        // Columns are padded to `| cell |`-aligned (widest cell = "Left aligned",
        // 13 chars, so column A occupies 15 columns between bars).
        assert!(out.contains("| Left aligned |"), "row not aligned: {out:?}");
        assert!(out.contains("Data 1       | Data 2"), "cell not padded: {out:?}");
        assert!(out.contains("| Data 6        |"), "last cell not padded: {out:?}");
        // A header underline is present (the separator row becomes `|---|`).
        assert!(out.contains("|---"), "missing underline: {out:?}");
    }

    // A markdown table in a ```md / ```markdown fence renders as markdown
    // (aligned grid), not as literal code lines.
    #[test]
    fn streaming_renders_md_fenced_table() {
        let md = "```markdown\n| Name | Role | Location |\n| :--- | :--- | :--- |\n| Alice | Developer | New York |\n| Bob | Designer | London |\n```\n";
        let mut r = StreamingRenderer::new(false);
        for line in md.lines() {
            r.push_line(line);
        }
        r.finalize();
        let out = r.output();
        assert!(out.contains("| Name  | Role      | Location |"), "table not rendered as aligned grid: {out:?}");
        assert!(out.contains("Alice"), "got: {out:?}");
        assert!(out.contains("Designer"), "got: {out:?}");
        // The ` markdown` fence markers themselves are NOT shown (we re-render
        // the content as markdown, not as a literal code block).
        assert!(!out.contains("```markdown"), "fence markers leaked: {out:?}");
        // A non-md fence still renders as literal code with its markers.
        let mut r2 = StreamingRenderer::new(false);
        for line in "```rust\nlet x = 1;\n```\n".lines() {
            r2.push_line(line);
        }
        r2.finalize();
        assert!(r2.output().contains("```rust"), "non-md fence should keep markers: {:?}", r2.output());
    }

    // A bare fence with the `markdown` marker on its own line (``` \nmarkdown)
    // also renders as markdown, not literal code.
    #[test]
    fn streaming_renders_bare_fence_with_markdown_next_line() {
        let md = "```\nmarkdown\n| Name | Role | Location |\n| :--- | :--- | :--- |\n| Alice | Developer | New York |\n```\n";
        let mut r = StreamingRenderer::new(false);
        for line in md.lines() {
            r.push_line(line);
        }
        r.finalize();
        let out = r.output();
        assert!(out.contains("| Name  | Role      | Location |"), "table not rendered as aligned grid: {out:?}");
        assert!(out.contains("Alice"), "got: {out:?}");
        // The separate `markdown` marker line and fence backticks are dropped.
        assert!(!out.contains("```"), "fence markers leaked: {out:?}");
        assert!(!out.contains("\nmarkdown"), "marker line leaked: {out:?}");
    }

    // Perf: the streaming path (O(n)) must be dramatically faster than the
    // whole-buffer re-render (O(n²)) on a large growing buffer. We simulate the
    // incremental redraw pattern: every 20 tokens, re-render. The streaming
    // renderer only parses the new tail; the whole-buffer `render` re-parses
    // everything. Assert the streaming path is at least 5x faster.
    #[test]
    fn streaming_is_faster_than_whole_buffer_rerender() {        // Build a large reply incrementally.
        let mut md = String::new();
        for i in 0..3000 {
            md.push_str(&format!("Token {i} **bold** and `code` with a [link](https://x.com) and _em_.\n"));
        }

        // Whole-buffer re-render: re-render the entire accumulated buffer every
        // 20 tokens (the old O(n²) `IncrementalMarkdown` behavior).
        let mut acc = String::new();
        let t_whole = Instant::now();
        for (i, line) in md.lines().enumerate() {
            acc.push_str(line);
            acc.push('\n');
            if i % 20 == 0 {
                let _ = render(&acc, false);
            }
        }
        let d_whole = t_whole.elapsed();

        // Streaming: feed each line once, render only the new tail.
        let mut r = StreamingRenderer::new(false);
        let t_stream = Instant::now();
        for line in md.lines() {
            r.push_line(line);
        }
        r.finalize();
        let d_stream = t_stream.elapsed();

        // The streaming path must be substantially faster (≥5x) on this input.
        assert!(
            d_stream.as_secs_f64() * 5.0 < d_whole.as_secs_f64(),
            "streaming ({d_stream:?}) not ≥5x faster than whole-buffer ({d_whole:?})"
        );
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

