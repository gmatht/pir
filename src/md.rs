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
