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

/// The concrete `Button` type returned by `App::create_button`, for the same
/// reason as [`GuiLabel`]: the sidebar rows hold real buttons so a click can
/// switch tabs, and the factory hands back the adapter type.
#[cfg(windows)]
type GuiButton = rustxwidgets::backends_nwg_adapter::Button;
#[cfg(all(target_os = "linux", not(windows)))]
type GuiButton = rustxwidgets::backends_gtk_adapter::Button;

/// The concrete `BoxWidget` type returned by `App::create_box`, for the same
/// reason: the sidebar owns its rows container across drain ticks.
#[cfg(windows)]
type GuiBox = rustxwidgets::backends_nwg_adapter::BoxWidget;
#[cfg(all(target_os = "linux", not(windows)))]
type GuiBox = rustxwidgets::backends_gtk_adapter::BoxWidget;

/// The concrete `CheckButton` type returned by `App::create_checkbutton`, for
/// the same reason: the security/settings dialogs own their toggles across
/// redraws, and the factory hands back the adapter type.
#[cfg(windows)]
type GuiCheckButton = rustxwidgets::backends_nwg_adapter::CheckButton;
#[cfg(all(target_os = "linux", not(windows)))]
type GuiCheckButton = rustxwidgets::backends_gtk_adapter::CheckButton;


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
/// GDK keyvals for keyboard scrolling (design row S6): Page_Up / Page_Down /
/// Home / End. The toolkit's prelude does not re-export these, and binding them
/// by name would silently compile as a catch-all `u32` pattern — hence raw
/// values, as with `KEY_ESCAPE` below. (0xff55 = Page_Up, 0xff56 = Page_Down,
/// 0xff50 = Home, 0xff57 = End.)
const GDK_KEY_PAGE_UP: u32 = 0xff55;
const GDK_KEY_PAGE_DOWN: u32 = 0xff56;
const GDK_KEY_HOME: u32 = 0xff50;
const GDK_KEY_END: u32 = 0xff57;

/// Viewport estimate, in lines, for keyboard / menu scrolling (design row S6).
/// `GuiScroll.page` is set to this until the pane's allocated height is known;
/// page-sized moves use `scroll.page.max(1)` so a zero page still moves.
const SCROLL_PAGE_LINES: usize = 20;

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
///
/// The list is also curated for unambiguous prefixes, not just coverage:
/// `/markup_demo` + `/md_demo` are implemented (arm, `/help`, menu) but stay
/// out, because four `m`-commands leave `/m` with no common prefix and break
/// `tab_completes_to_common_prefix`. Same reason the global table's short
/// aliases stay out.
///
/// `/menu` is deliberately *in*: it was the one command the GUI implemented
/// (a dialog, a bar of its own, every one of its actions in the menu bar) that
/// Tab completion did not offer and `/help` did not list — which is how an
/// operator concludes it does not exist.
const COMMANDS: &[&str] = &[
    "help", "menu", "new-project", "model", "models", "goal", "continue", "clear", "undo",
    "finished", "exit", "usage", "thinking", "cancel", "sessions", "reload",
    "reexec", "fix", "shell", "sh", "bg", "jobs", "checkpoints", "rewind",
    "quarantine", "limits", "resources", "skills", "scratchpad", "compact",
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

/// Every candidate Tab would choose between for `buf`, in list order.
///
/// This is what the GUI *shows* when Tab cannot decide on its own — see
/// [`completion_hint`]. `complete_idle` and this function share the same
/// filters, so the list can never offer something Tab would refuse; the pairing
/// is pinned by `the_completion_list_is_exactly_what_tab_matches`.
fn completion_candidates(buf: &str) -> Vec<String> {
    if buf.starts_with("/thinking ") {
        let arg = buf.trim_start_matches("/thinking ").trim_start();
        return THINKING_ARGS
            .iter()
            .filter(|o| o.starts_with(arg))
            .map(|o| format!("/thinking {o}"))
            .collect();
    }
    if buf.starts_with('/') {
        let frag = buf.trim_start_matches('/');
        if frag.contains(' ') {
            return Vec::new(); // past the command word
        }
        return COMMANDS
            .iter()
            .filter(|c| c.starts_with(frag))
            .map(|c| format!("/{c}"))
            .collect();
    }
    Vec::new()
}

/// The hint line for a draft while it is being typed: either what Tab just
/// did, or — when Tab could not decide or has not been pressed — which
/// completions exist.
///
/// The reported gap: `/men` completes to `/menu`, and nothing ever said so;
/// `/m` matched several commands and Tab silently did nothing. A completion
/// you have to already know about is not discoverable. This is the mirror of
/// the terminal's list-under-the-prompt: the GUI has no room to print a list
/// above the Entry, so the hint under the status bar names the candidates.
///
/// `tab_note` is set when Tab was pressed and could not complete. It is shown
/// *with* the list (`[Tab] no match` style) so a second Tab never looks like
/// the first one being lost.
fn completion_hint(buf: &str, tab_note: Option<&str>, narrow: bool) -> String {
    let cands = completion_candidates(buf);
    let is_command_word = buf.starts_with('/') && !buf.trim_start_matches('/').contains(' ');
    if cands.len() == 1 {
        // Only one candidate: Tab completes it, so name the result, not the
        // candidate list (they are the same command).
        let c = &cands[0];
        // Do not suggest completing something that is already complete.
        if c == buf.trim_end() || format!("{c} ") == buf {
            return tab_note.unwrap_or("").to_string();
        }
        return match tab_note {
            // Tab just did it: the note already says so. Repeating the advice
            // would read "completed to /menu — completes to /menu".
            Some(n) if n.starts_with("[Tab] completed") => n.to_string(),
            Some(n) => format!("{n} — [Tab] completes to {c}"),
            None => format!("[Tab] completes to {c}"),
        };
    }
    if cands.is_empty() {
        // No candidate is normal prose; only a note after Tab is worth a hint.
        return tab_note.unwrap_or("").to_string();
    }
    // Ambiguous (or a bare `/`): list them. Commands are short, so show them
    // all up to `COMPLETION_HINT_MAX`; beyond that the count and the narrow
    // instruction ("keep typing") are what matter. The `/model` argument list
    // is long (full provider/model labels), so it is capped much lower and
    // points at `/models` instead.
    // The `/thinking <arg>` list is a *sub*-argument list: show the bare
    // argument (`off`, `minimal`, …), not the command again on every entry.
    let label = |c: &str| -> String {
        if is_command_word {
            c.to_string()
        } else {
            c.rsplit(' ').next().unwrap_or(c).to_string()
        }
    };
    let cap = if is_command_word { COMPLETION_HINT_MAX } else { 7 };
    let shown: Vec<String> = cands.iter().take(cap).map(|c| label(c)).collect();
    let mut out = String::new();
    if let Some(n) = tab_note {
        out.push_str(n);
        out.push_str(" — ");
    } else {
        out.push_str("[Tab] ");
    }
    out.push_str(&shown.join("  "));
    if cands.len() > shown.len() {
        out.push_str(&format!("  … +{}", cands.len() - shown.len()));
    }
    if narrow && cands.len() > 1 {
        out.push_str("   (keep typing to narrow)");
    }
    out
}

/// How many completion candidates the hint line names before summarising the
/// rest as `… +N`.
const COMPLETION_HINT_MAX: usize = 6;

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

/// Whether `line` continues the assistant block above it, so both render as
/// one markdown document (and one Pango unit).
///
/// The conversation model is line-oriented, but markdown *is not*: a fenced
/// code block, a table, a blockquote, a paragraph and a loose list all span
/// several lines. Rendering each line on its own therefore splits every such
/// construct — the reported `/markup_demo` bug, where the table came out as
/// four literal `| … |` lines, each code body line got its own ```` ``` ````
/// fence, and a blockquote emitted a span that opens on one line and closes on
/// the next (invalid markup). A block is the run of consecutive assistant lines
/// between blank lines, which is exactly the granularity CommonMark parses at:
/// the renderer is handed the whole run and returns the whole run's Pango.
///
/// An empty line always breaks the block: it is the one thing the line model
/// and markdown agree on. Only assistant lines form blocks — a user prompt or
/// a system notice between them ends the block, so the renderer is never handed
/// another kind's text as markdown.
fn continues_block(prev: Option<&ConvLine>, line: &ConvLine) -> bool {
    match prev {
        Some(p) => p.kind == ConvKind::Assistant
            && line.kind == ConvKind::Assistant
            && !p.text.trim().is_empty()
            && !line.text.trim().is_empty(),
        None => false,
    }
}

/// The number of lines after the write cursor `from` that `sync_textview` may
/// paint now: every completed block, plus the trailing block from `from` on.
///
/// Completed blocks end at a blank line, but the *trailing* block has no such
/// end in a line-oriented model — its terminator may simply not have arrived
/// yet. The rule is therefore: paint up to the start of the trailing block, then
/// the trailing block from wherever the cursor stands. `sync_textview` parks
/// its cursor on the trailing block's first line, so on the next sync it
/// re-renders that block, whole, once more of it has arrived, and never paints
/// a partial copy twice. Without this a transcript whose last line is not blank
/// (the common case) would render nothing new at all.
///
/// This is the live-sync write rule; it is split out so it can be tested
/// without a display.
fn live_boundary(lines: &[ConvLine], from: usize) -> usize {
    let Some(last_nonblank) = lines.iter().rposition(|l| !l.text.trim().is_empty()) else {
        return 0;
    };
    // The trailing block (which starts after the last blank line above it) is
    // painted WHOLE, from its start, even though it is still incomplete —
    // otherwise a long streamed block (a table, a code fence) stays invisible
    // until a blank line finally terminates it, which for a big table means the
    // user sees nothing while it streams and then the entire thing appears at
    // once. `sync_textview` rewinds its write cursor to that start after
    // painting, so the block is re-rendered from the model each tick and never
    // appears twice: `rendered` is the cursor, not a high-water mark.
    let _ = from;
    last_nonblank + 1
}

/// Widen a line window `(start, end)` outward to block boundaries, so a block
/// that straddles the window edge is shown whole rather than sliced.
///
/// The window is only ever *widened*, and never past the model, so the
/// keyboard-scroll page still contains every line it asked for.
fn window_at_block_edges(conv: &[ConvLine], (start, end): (usize, usize)) -> (usize, usize) {
    let mut start = start.min(conv.len());
    let end = end.min(conv.len()).max(start);
    while start > 0 && continues_block(Some(&conv[start - 1]), &conv[start]) {
        start -= 1;
    }
    let mut end = end;
    while end < conv.len() && end > start && continues_block(Some(&conv[end - 1]), &conv[end]) {
        end += 1;
    }
    (start, end)
}

/// Render a block of assistant lines (all [`ConvKind::Assistant`]) as ONE Pango
/// unit — the whole markdown document, not one line at a time.
///
/// Colour lives in the kind prefix; the body is the Pango that
/// `md::render_pango` produced for the joined block. The text is escaped or
/// already markup by construction (it is renderer output, never raw model
/// text), so markup parsing cannot be tricked by the model.
#[cfg(feature = "gui-rich-text")]
fn assistant_block_markup(kind: ConvKind, block: &[ConvLine]) -> String {
    let (color, prefix) = kind.style();
    let raw = block
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let body = crate::md::render_pango(&raw);
    // The prefix must line up with EVERY line it introduces, not just the
    // first. Emitting it once put it on the block's opening line only, so a
    // multi-line construct (a table's `┌───` border, a fenced code block, a
    // blockquote) had its first row shifted right by the prefix width while
    // the rows below started at column 0 — the misaligned `/markup_demo`
    // table. Prefixing each body line keeps the whole construct flush.
    let body = body.trim_end_matches('\n');
    let pad = escape_markup(prefix);
    let body = body
        .split('\n')
        .map(|line| {
            if line.is_empty() {
                // Blank separator lines carry no prefix: trailing spaces in
                // Pango markup are invisible, but they would also make the
                // paragraph look non-empty to any future trim logic.
                String::new()
            } else {
                format!("<span foreground=\"{color}\">{pad}</span>{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    body
}

/// Strip the most common inline markdown markers for contexts that cannot
/// parse Pango (the frozen keyboard-scroll slice in rich mode, where
/// `set_text` takes plain text only). Not a renderer — just enough that a
/// scrolled-away reader does not see `**`.
#[cfg(feature = "gui-rich-text")]
fn strip_markdown_markers(s: &str) -> String {
    s.replace("**", "").replace("__", "").replace('`', "")
}

/// Record a pushed line's link inventory: `(text, url)` pairs from
/// `md::extract_links`, for every line kind — a pasted URL in a user prompt
/// is as clickable as one in an assistant reply, and the resolver is
/// fail-safe (a pair whose text is absent from the inserted buffer line tags
/// nothing).
fn record_link_pairs(para: &str) -> Vec<(String, String)> {
    crate::md::extract_links(para)
        .into_iter()
        .map(|l| (l.text, l.url))
        .collect()
}

/// Resolve `(text, url)` pairs against the exact visible text the buffer
/// received, yielding `(buffer_line, start_col, end_col, url)` in buffer
/// chars. Successive search (each pair from the end of the previous match):
/// links render in source order, so the first occurrence rule would tag the
/// wrong copy when the same text appears twice (`[docs](u1)` … `[docs](u2)`).
/// A pair whose text is absent (rendering transformed it, or it spans a block
/// merge) resolves to nothing — fail-safe, never a wrong link.
///
/// Pure so the mapping is testable without a display; `sync_textview` is the
/// only caller, and it always passes the exact string it just inserted.
fn resolve_link_spans(visible: &str, pairs: &[(String, String)]) -> Vec<(u32, u32, u32, String)> {
    // Byte offset of each visible line's start, so a byte match converts to
    // (line, char col) — the units `GtkTextIter` counts.
    let mut line_starts = vec![0usize];
    for (i, b) in visible.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let byte_to_line_col = |byte: usize| -> (u32, u32) {
        let line = line_starts.iter().rposition(|&s| s <= byte).unwrap_or(0);
        let col = visible[line_starts[line]..byte].chars().count();
        (line as u32, col as u32)
    };
    let mut out = Vec::new();
    let mut from = 0usize;
    for (text, url) in pairs {
        if text.is_empty() {
            continue;
        }
        let Some(rel) = visible[from..].find(text.as_str()) else {
            continue;
        };
        let start = from + rel;
        let end = start + text.len();
        let (line, scol) = byte_to_line_col(start);
        let (eline, ecol) = byte_to_line_col(end);
        // `find` never returns a split boundary (`text` is `&str`-matched, so
        // `end` is always a char boundary), but a link text containing `\n`
        // would span buffer lines — tag only the single-line case; anything
        // else stays unclickable rather than mis-tagged.
        if line == eline {
            out.push((line, scol, ecol, url.clone()));
        }
        from = end;
    }
    out
}

/// Look up the URL for a click at buffer `(line, col)`: the span containing
/// `col` on that line, if any. Pure so the hit rule is testable without GTK.
fn lookup_link(spans: &[(u32, u32, String)], col: u32) -> Option<&str> {
    spans
        .iter()
        .find(|(s, e, _)| col >= *s && col < *e)
        .map(|(_, _, u)| u.as_str())
}

/// Shared per-UI state, guarded by a Mutex so both the Entry-activate callback
/// (main thread) and the periodic drain callback (also main thread via glib)
/// can see it. Since glib callbacks run on the main loop, contention is trivial.
struct GuiState {
    /// Conversation lines (the transcript so far), each with a colour kind.
    conv: Vec<ConvLine>,
    /// Link inventory per conversation line, parallel to `conv`: entry `i`
    /// holds `(text, url)` pairs from `md::extract_links` on line `i`'s own
    /// text. Spans are NOT stored here — Pango markup shifts bytes between
    /// model text and buffer text, so spans are resolved at insert time
    /// against the exact string the buffer receives (see
    /// `resolve_link_spans`). The vec is per-line so `push`/`clear`/trim keep
    /// it in step with `conv` by construction rather than by audit.
    line_links: Vec<Vec<(String, String)>>,
    /// Clickable spans per BUFFER line: entry `i` holds
    /// `(start_col, end_col, url)` in buffer chars for buffer line `i`.
    /// Built by `sync_textview` as it inserts (the only place that knows what
    /// the buffer actually holds — assistant blocks render multi-line, so
    /// buffer lines never map 1:1 to model lines). Reset whenever the buffer
    /// is replaced (`set_text`); the live branch rebuilds it from scratch on
    /// re-arm because `view_slice.is_some()` forces a rebuild there.
    buf_links: Vec<Vec<(u32, u32, String)>>,
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
    /// Model index at which the trailing (still-growing) block starts once it
    /// has been painted; `None` when no trailing block is currently in the
    /// buffer. The next sync that sees that block grow clears the buffer and
    /// repaints from here, because the GTK text buffer cannot delete lines —
    /// without this the block would either stay invisible while it streams or
    /// be appended twice.
    trailing_from: Option<usize>,
    /// How many model lines the buffer held at the last paint. Used to tell
    /// "the trailing block grew" (repaint it) from "nothing arrived" (leave the
    /// pane alone).
    painted_len: usize,
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
    ///
    /// `follow` is read by `sync_textview` on every paint (see
    /// `sync_follow`): `false` holds back new lines (frozen full buffer) or
    /// shows the keyboard-scroll window, so disarming it has a visible effect
    /// rather than being advisory.
    follow: bool,
    /// Keyboard scroll position (design row S6). The toolkit has no programmatic
    /// scroll (`gtk_adjustment_set_value` is missing), so this is the GUI's own
    /// clamped model of where the user asked to be. Driven by Page_Up /
    /// Page_Down / Home / End (key handler in `spawn_session`) and by the View
    /// menu's scroll items (`MENU_VIEW_ACTIONS`); consulted by `sync_textview`.
    /// In live mode (`follow` armed) `offset` tracks the bottom
    /// (`max_offset(total)`); scrolled away, it selects the window
    /// `conv[offset .. offset + page]` the pane shows.
    scroll: GuiScroll,
    /// Slice painted while keyboard-scrolled: `(offset, total)` at paint time.
    /// `sync_textview` repaints the window only when the offset, the model
    /// length, or `needs_rebuild` changed — not on every 150ms drain tick.
    /// `None` while live (the append-only path owns the buffer then).
    view_slice: Option<(usize, usize)>,
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
    /// Set when Tab was pressed and could not complete the draft, holding the
    /// reason (an ambiguity, already listed, or no match at all). Cleared as
    /// soon as the draft changes, so it describes *this* line only — a stale
    /// "no match" over a line the operator has since edited would be a lie.
    tab_note: Option<String>,
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
    let started = std::time::Instant::now();
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
    // Wall time below the output, same `Elapsed …` readout as the bash tool.
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(&format!("Elapsed {}", crate::term::fmt_cmd_elapsed(started.elapsed())));
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
  /menu [action]        main menu in a dialog: resume/model/thinking/
                        security/settings/worktrees/coverage/help/about/quit
                        (the Menu bar above the tabs lists the same actions)
  /model [prefix]       pick a model in a dialog
  /models               list every model in the catalog
  /usage                tokens this session
  /goal [obj]           show or start the standing goal
  /continue             drive the goal to completion
  /thinking <lvl>       off|minimal|low|medium|high|xhigh|max  (+ show|hide)
  /clear                clear the pane
  /undo [all]           revert the last file edit (or all) to its pre-edit state
  /finished             mark this session finished
  /fix                  repair the workspace VCS setup
  /sh <cmd>             run a shell command (aliases: /shell; and ! <cmd>)
  /bg <prompt>          start a background turn
  /jobs                 list background jobs
  /cancel               stop the running turn
  /compact [status|on|off]
                        summarise the conversation so far and replace it with
                        the summary (frees the context window); `status` shows
                        usage without calling the model, `on`/`off` arm/disarm
                        the automatic compaction before a near-cap auto-retry
  /checkpoints          list rewindable turn boundaries
  /rewind <#>           restore the tree and the conversation to a turn
  /quarantine [status|apply all|discard]
                         review / approve staged writes to the real filesystem
  /limits               what the agent's commands are bounded by (alias: /resources)
  /skills [name]        list the skill catalog, or print one
  /scratchpad           this session's scratch directory
  /reload               reload catalog, policy and extension commands
  /reexec               restart in place (idle only)
  /markup_demo          render a canned markdown demo (headings/code/table)
  /sessions             new|close|next|prev|<n>  multi-session tabs
  /new-project [dir]    open a project directory in a new window (existing, or
                        type a new path to create it); no arg opens a picker
  /exit                 quit

keys:
  Enter                 send the line / run the command
  esc or ctrl-c         cancel the running turn (same as /cancel)
  ctrl-q or ctrl-d      quit
  tab                   complete the command or model name
  up / down             recall earlier / later prompts
  pgup / pgdn           scroll one page   home / end   top / live tail
  click a link          open it in the browser";

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
            line_links: Vec::new(),
            buf_links: Vec::new(),
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
            trailing_from: None,
            painted_len: 0,
            needs_rebuild: true,
            bg: Vec::new(),
            next_bg_id: 1,
            continuations: Vec::new(),
            spinner: 0,
            last_status: String::new(),
            last_model: None,
            follow: true,
            // Until the pane's allocated height is known, keyboard / menu
            // scrolling moves in `SCROLL_PAGE_LINES`-line steps (design row
            // S6); `page.max(1)` in the step/window helpers keeps a zero page
            // from becoming a stuck key.
            scroll: GuiScroll {
                page: SCROLL_PAGE_LINES,
                ..GuiScroll::default()
            },
            view_slice: None,
            awaiting_input: false,
            draft: String::new(),
            last_hint: String::new(),
            tab_note: None,
            last_title: String::new(),
        }
    }

    /// Append a conversation line, splitting on newlines and capping
    /// scrollback so memory stays bounded.
    ///
    /// The cap keeps the model and the buffer in step: trimming from the front
    /// means the next sync must rebuild rather than append, so we bump
    /// `rendered` down with the model and flag `needs_rebuild`.
    ///
    /// Each pushed paragraph records its own link inventory (see
    /// `md::extract_links`), so `line_links` stays parallel to `conv`: one
    /// entry per line, trimmed and cleared together.
    fn push(&mut self, kind: ConvKind, text: &str) {
        for para in text.split('\n') {
            self.conv.push(ConvLine { kind, text: para.to_string() });
            self.line_links.push(record_link_pairs(para));
        }
        const MAX: usize = 4000;
        if self.conv.len() > MAX {
            let drop = self.conv.len() - MAX;
            self.conv.drain(0..drop);
            self.line_links.drain(0..drop);
            self.rendered = self.rendered.saturating_sub(drop);
            self.needs_rebuild = true;
        }
    }

    /// Clear the conversation (e.g. `/clear`).
    fn clear(&mut self) {
        self.conv.clear();
        self.line_links.clear();
        self.buf_links.clear();
        self.rendered = 0;
        self.needs_rebuild = true;
        // A clear empties the pane, so there is nothing to be scrolled away
        // from: follow comes back armed or the next turn would render off-screen.
        self.follow = true;
        // And any keyboard-scroll window is meaningless against an empty model.
        self.scroll.home();
        self.view_slice = None;
    }

    /// Note where the pane is scrolled to, and re-arm or disarm `follow`.
    ///
    /// `vpos` is the vertical adjustment value read from the `ScrolledWindow`
    /// (pixels). `vmax` is that adjustment's upper bound, and `page` is the
    /// viewport height (the adjustment's page size). The bottom of what is
    /// shown is `vpos + page`; `follow` is armed only when that reaches `vmax`
    /// (within 2px, since GTK reports fractional values) and disarmed anywhere
    /// else. Comparing `vpos` against `vmax` directly is wrong: `vpos` is the
    /// top of the viewport, so it sits a full page above `vmax` even at the
    /// bottom, and `follow` would disarm on the very first drain and freeze
    /// every later append out of the pane.
    ///
    /// Called once per drain *before* appending, so the decision to pin reflects
    /// where the user was when the new output arrived rather than after GTK has
    /// already moved the position.
    fn observe_scroll(&mut self, vpos: f64, vmax: f64, page: f64) {
        const AT_BOTTOM_SLOP: f64 = 2.0;
        self.follow = vpos + page >= vmax - AT_BOTTOM_SLOP;
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

    /// The window `sync_textview` paints while keyboard-scrolled: the clamped
    /// `offset .. offset + page` slice of a `total`-line model. Shared by the
    /// renderer and by the menu/keys that move `offset`, so a scrolled pane and
    /// the model that drives it can never disagree about which lines are shown.
    fn window(&self, total: usize) -> (usize, usize) {
        let start = self.offset.min(self.max_offset(total));
        let end = (start + self.page.max(1)).min(total);
        (start, end)
    }
}

/// The number handlers pass to `GuiScroll::page_up/page_down`: a page-sized
/// step from the scrolled model's viewport estimate (design row S6). `0` would
/// be a no-op — and a stuck PageDown key — so clamp to at least 1.
fn scroll_step(s: &GuiState) -> usize {
    s.scroll.page.max(1)
}

/// Sentinel slice forcing `sync_textview` to repaint the keyboard-scroll
/// window: no real `(offset, total)` window can equal it, so the frozen branch
/// always repaints once after a scroll command.
const SCROLL_DIRTY: (usize, usize) = (usize::MAX, usize::MAX);

/// Move the keyboard-scroll window up one page (design row S6). `end` first
/// tracks the live tail, so the first PageUp from the bottom starts at the
/// last full page rather than one partial step past it. Shared by the Page_Up
/// key and the View menu's "Scroll up one page", so key and menu move the
/// same window.
fn scroll_page_up(s: &mut GuiState) {
    let total = s.conv.len();
    s.scroll.end(total);
    s.scroll.page_up(scroll_step(s));
    s.follow = false;
    s.view_slice = Some(SCROLL_DIRTY);
}

/// Move the keyboard-scroll window down one page. At the bottom edge this
/// re-arms follow (see `observe_scroll`: the bottom *is* live, so pinning and
/// follow agree there). Shared by the Page_Down key and the View menu.
fn scroll_page_down(s: &mut GuiState) {
    let total = s.conv.len();
    s.scroll.page_down(scroll_step(s), total);
    s.follow = s.scroll.offset >= s.scroll.max_offset(total);
    s.view_slice = Some(SCROLL_DIRTY);
}

/// Jump the keyboard-scroll window to the top (Home key). Shared helper so a
/// future menu item can reuse it without a second implementation.
fn scroll_home(s: &mut GuiState) {
    s.scroll.home();
    s.follow = false;
    s.view_slice = Some(SCROLL_DIRTY);
}

/// Jump to the live tail (End key, View menu "Scroll to bottom"). Re-arms
/// follow; the live branch of `sync_textview` then repaints the full tail
/// (any windowed buffer is discarded via the `view_slice` check there).
fn scroll_end(s: &mut GuiState) {
    s.scroll.end(s.conv.len());
    s.follow = true;
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

// ---------------------------------------------------------------------------
// New Project: pick a directory, then open pir there
// ---------------------------------------------------------------------------

/// Report a New Project outcome in the pane (and never silently do nothing).
fn report_new_project(state: &Arc<Mutex<GuiState>>, widgets: &GuiWidgets, kind: ConvKind, msg: &str) {
    let mut s = state.lock().unwrap();
    s.push(kind, msg);
    sync_textview(widgets, &mut s);
}

/// Open a project directory from the GUI: resolve it, launch, report.
///
/// The single implementation behind the sidebar button, the Session menu item
/// and `/new-project <path>` — all three call this, so they cannot disagree
/// about what a project launch does or reports.
fn open_project(
    app: &App,
    dir: &str,
    create: bool,
    state: &Arc<Mutex<GuiState>>,
    widgets: &GuiWidgets,
    full_auto: bool,
) {
    match crate::project_launch::target(dir, create) {
        Ok(path) => match crate::project_launch::launch(&path, full_auto) {
            Ok(pid) => report_new_project(
                state,
                widgets,
                ConvKind::System,
                &format!("· opening project {} (new window, pid {pid})", path.display()),
            ),
            Err(e) => report_new_project(state, widgets, ConvKind::Error, &format!("· {e}")),
        },
        Err(e) if !create => {
            // The path does not exist (or is a file): never mkdir silently —
            // open the picker, which offers the explicit "create" step.
            open_project_picker(app, state, widgets, full_auto, Some(&e));
        }
        Err(e) => report_new_project(state, widgets, ConvKind::Error, &format!("· {e}")),
    }
}

/// The New Project picker: a path in an `Entry` (pre-filled with the current
/// workspace), a *Browse…* button for the native folder chooser, and
/// Open / Cancel.
///
/// The typed path is what makes "new directory" work on every backend: a
/// missing path is created, but only after a second, explicit confirmation
/// (a picker that silently `mkdir`s a typo is worse than useless).
fn open_project_picker(
    app: &App,
    state: &Arc<Mutex<GuiState>>,
    widgets: &GuiWidgets,
    full_auto: bool,
    notice: Option<&str>,
) {
    let Ok(dialog) = app.create_dialog() else {
        report_new_project(
            state,
            widgets,
            ConvKind::Error,
            "· could not open the New Project dialog — use /new-project <path>",
        );
        return;
    };
    dialog.set_title("pir — new project");
    if let Some(n) = notice {
        if let Ok(lbl) = app.create_label(n) {
            #[cfg(all(target_os = "linux", not(windows)))]
            lbl.set_xalign(0.0);
            dialog.append_content_area(&lbl);
        }
    }
    let Ok(lbl) = app.create_label("Open a project directory (an existing one, or a new one to create):") else {
        return;
    };
    #[cfg(all(target_os = "linux", not(windows)))]
    lbl.set_xalign(0.0);
    dialog.append_content_area(&lbl);

    let Ok(entry) = app.create_entry() else { return };
    // Start at the workspace the operator is looking at, so "new project" from
    // an existing project is usually a sibling edit away.
    let start = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    entry.set_text(&start.to_string_lossy());
    dialog.append_content_area(&entry);

    // Browse…: the native folder chooser, which also lets the operator create
    // a folder in place (GTK's and Win32's folder dialogs both offer that).
    if let Ok(browse) = app.create_button("Browse…") {
        let entry_b = entry.clone();
        let browse_app = app.clone();
        let _ = browse.on_click(move || {
            if let Ok(Some(dir)) = browse_app.select_folder("pir — choose a project directory") {
                entry_b.set_text(&dir);
            }
        });
        dialog.append_content_area(&browse);
    }

    // Open: existing directory launches; a missing one asks first.
    if let Ok(open) = app.create_button("Open") {
        let st = state.clone();
        let w = widgets.clone();
        let e = entry.clone();
        let dlg = dialog.clone();
        let _ = open.on_click(move || {
            let raw = e.get_text().unwrap_or_default();
            let raw = raw.trim().to_string();
            if raw.is_empty() {
                return;
            }
            match crate::project_launch::target(&raw, false) {
                Ok(path) => {
                    dlg.close();
                    match crate::project_launch::launch(&path, full_auto) {
                        Ok(pid) => report_new_project(
                            &st,
                            &w,
                            ConvKind::System,
                            &format!("· opening project {} (new window, pid {pid})", path.display()),
                        ),
                        Err(e) => report_new_project(&st, &w, ConvKind::Error, &format!("· {e}")),
                    }
                }
                Err(_) => {
                    // Missing: confirm creation explicitly rather than mkdir a typo.
                    dlg.close();
                    match crate::project_launch::target(&raw, true) {
                        Ok(path) => match crate::project_launch::launch(&path, full_auto) {
                            Ok(pid) => report_new_project(
                                &st,
                                &w,
                                ConvKind::System,
                                &format!(
                                    "· created project {} (new window, pid {pid})",
                                    path.display()
                                ),
                            ),
                            Err(e) => report_new_project(&st, &w, ConvKind::Error, &format!("· {e}")),
                        },
                        Err(e) => report_new_project(&st, &w, ConvKind::Error, &format!("· {e}")),
                    }
                }
            }
        });
        dialog.append_content_area(&open);
    }
    if let Ok(cancel) = app.create_button("Cancel") {
        let dlg = dialog.clone();
        let _ = cancel.on_click(move || dlg.close());
        dialog.append_content_area(&cancel);
    }
    dialog.present();
}

// ---------------------------------------------------------------------------
// Left sessions tree (sidebar)
// ---------------------------------------------------------------------------

/// One row in the sidebar: a group header or a session button. Buttons are
/// created once and updated in place (`set_text`); stale ones are hidden, not
/// destroyed (the toolkit has no remove API), so the 150ms tick never churns
/// widgets — no flicker, no focus theft.
struct SidebarRow {
    /// Session id (log path string) or group key (`g:{project_key}`).
    id: String,
    /// True for a group header (toggles collapse), false for a session
    /// (switches to its tab). Retained for debugging; behavior branches on
    /// the `g:` id prefix instead.
    #[allow(dead_code)]
    is_group: bool,
    button: GuiButton,
    /// The status icon at the left of a session row (`▶` running, `●` stopped
    /// unseen, `○` stopped seen — see [`crate::session_tree::TreeStatus`]).
    /// `None` for group headers, which carry no status. A separate label
    /// rather than a glyph inside the button text so the icon can be coloured
    /// (Pango markup) while the title stays plain text.
    icon: Option<GuiLabel>,
    /// Horizontal container holding `[icon][button]` side by side. `None`
    /// for group headers (bare button). Without this the icon and button
    /// are siblings in the vertical `rows_box`, so the icon stacks *above*
    /// the button; the row box keeps them on one line with the icon left.
    row_box: Option<GuiBox>,
}

#[derive(Default)]
struct SidebarState {
    rows: Vec<SidebarRow>,
    /// Last rendered signature: when nothing changed the tick is a no-op.
    /// The signature covers row order, labels, glyphs and collapse, so any
    /// visible change repaints exactly once.
    signature: String,
    /// Group keys the operator collapsed (persisted per cwd on toggle).
    collapsed: std::collections::HashSet<String>,
    loaded: bool,
    /// Sticky acknowledgement of each session's status (session id →
    /// [`crate::session_tree::TreeStatus`]). Lives with the sidebar, not in a
    /// tab's `GuiState`: the tree is the sidebar's business, and a row must
    /// stay amber while it is `StoppedUnseen` no matter which tab is active.
    /// See [`sidebar_row_status`] for the settle edge and
    /// [`SidebarState::acknowledge`] for the seen edge.
    tree_status: std::collections::HashMap<String, crate::session_tree::TreeStatus>,
}

impl SidebarState {
    /// The operator has looked at `id`: a stopped row settles from
    /// `StoppedUnseen` to `StoppedSeen`. Clears the signature so the next tick
    /// repaints the icon even if nothing else changed.
    fn acknowledge(&mut self, id: &str) {
        if let Some(s) = self.tree_status.get_mut(id)
            && s.wants_attention()
        {
            *s = crate::session_tree::TreeStatus::StoppedSeen;
            self.signature.clear();
        }
    }
}

/// Rebuild the sidebar's rows from the open tabs, grouped by project.
///
/// Every session row carries a three-state status icon to the left of its
/// title, from [`crate::session_tree::TreeStatus`]:
///
///   * `▶` blue — **running**: the tab is working (a turn, a background job,
///     or a live pid), including a worker parked on a dialog it raised.
///   * `●` amber — **stopped, unseen**: it came to rest since the operator
///     last looked (turn finished in another tab, follow-up ended, the process
///     exited). The only state that asks for a look.
///   * `○` dim — **stopped, seen**: came to rest and the operator has seen
///     that result (clicked its row, switched to its tab, or it was already
///     resting).
///
/// The status is *sticky*: the "was running, now isn't" edge is the only thing
/// that turns a row amber ([`sidebar_row_status`]), and only opening the
/// session settles it ([`SidebarState::acknowledge`]) — a session that has
/// finished never re-lights just because the liveness sidecar says "not
/// running" on every later tick.
///
/// Clicking a session row switches to its tab, focuses its prompt and settles
/// its icon; clicking a group header collapses/expands it (persisted per cwd).
///
/// Cheap by construction: the whole tree flattens to one signature string and
/// the widget pass only runs when it changed — a spinner tick repaints text,
/// never structure.
fn sync_sidebar(
    app: &App,
    rows_box: &GuiBox,
    state: &Rc<RefCell<SidebarState>>,
    sessions: &Rc<RefCell<Vec<SessionView>>>,
    tabs: &TabView,
    bus: &SharedBus,
) {
    // Snapshot each tab. The agent slot is only briefly locked per tab; the
    // tree build below holds no locks. `active` is "something in this session
    // is still working and has not come to rest": the foreground turn, a
    // background turn/job, or a live pid in the status sidecar. A worker
    // parked on a dialog it raised still counts as active — "waiting on you"
    // is mid-work here, not stopped (the brief), and the settle edge is what
    // turns a row amber once the worker actually comes to rest.
    struct TabSnap {
        id: String,
        title: String,
        active: bool,
        log: Option<PathBuf>,
    }
    let snaps: Vec<TabSnap> = {
        let svs = sessions.borrow();
        svs.iter()
            .enumerate()
            .map(|(i, sv)| {
                let (title, log, running) = (
                    tabs.tab_title(i).unwrap_or_else(|| format!("session {i}")),
                    sv.agent_slot.lock().unwrap().as_ref().and_then(|a| a.log_path.clone()),
                    sv.state.lock().map(|s| s.running).unwrap_or(false),
                );
                let bg = sv.state.lock().map(|st| tasks_running(&st)).unwrap_or(0);
                let live_pid =
                    log.as_ref().is_some_and(|p| crate::session_tree::live_session(p));
                TabSnap {
                    id: log
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|| format!("tab:{i}")),
                    title,
                    active: running || bg > 0 || live_pid,
                    log,
                }
            })
            .collect()
    };
    if snaps.is_empty() {
        return;
    }
    // The notification feed still drives `/sessions` and the TUI; the GUI
    // sidebar's own three-state icon is derived from liveness below.
    let unread = bus.unread_counts();
    let _ = &unread;
    // Group by project root: the session's recorded cwd, else the log's parent
    // dir, else "(no directory)". Same roots `session_tree::build_tree` uses.
    let mut groups: std::collections::HashMap<String, Vec<usize>> = std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for (i, sn) in snaps.iter().enumerate() {
        let cwd = sn
            .log
            .as_ref()
            .and_then(|p| crate::session::read_status(p))
            .map(|m| m.cwd)
            .unwrap_or_default();
        let key = if cwd.is_empty() { "(no directory)".to_string() } else { cwd };
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(i);
    }
    order.sort();
    let mut st = state.borrow_mut();
    // Load persisted collapse once (per cwd, like the text tree).
    if !st.loaded {
        st.loaded = true;
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        st.collapsed = crate::session_tree::load_state(&cwd.to_string_lossy()).collapsed;
    }
    // Flatten to (id, is_group, text) rows + signature. Session rows carry the
    // three-state status icon *separately* (an `icon` label, so it can be
    // coloured); the button text is the title alone.
    let mut flat: Vec<(String, bool, String)> = Vec::new();
    // (session id, glyph, colour) for every visible session row.
    let mut icons: Vec<(String, String, &'static str)> = Vec::new();
    let mut sig = String::new();
    for key in &order {
        let label = key.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(key);
        let collapsed = st.collapsed.contains(key);
        let marker = if collapsed { "▸" } else { "▾" };
        let gid = format!("g:{key}");
        flat.push((gid.clone(), true, format!("{marker} {label}")));
        sig.push_str(&gid);
        sig.push(if collapsed { '-' } else { '+' });
        if collapsed {
            continue;
        }
        for &i in &groups[key] {
            let sn = &snaps[i];
            let status = sidebar_row_status(&mut st.tree_status, &sn.id, sn.active);
            icons.push((sn.id.clone(), status.glyph().to_string(), status.color()));
            flat.push((sn.id.clone(), false, sn.title.clone()));
            sig.push_str(&sn.id);
            sig.push_str(status.glyph());
            sig.push_str(&sn.title);
        }
    }
    if sig == st.signature {
        return; // nothing visible changed — skip the widget pass entirely
    }
    st.signature = sig;
    // Reconcile widgets: update text in place, append newcomers, hide the gone.
    let icon_of = |id: &str| icons.iter().find(|(i, _, _)| i == id);
    let mut seen = std::collections::HashSet::new();
    for (id, is_group, text) in &flat {
        seen.insert(id.clone());
        if let Some(row) = st.rows.iter().find(|r| &r.id == id) {
            row.button.set_text(text);
            row.button.set_visible(true);
            if let (Some(icon), Some(row_box), Some((_, glyph, colour))) =
                (&row.icon, &row.row_box, icon_of(id))
            {
                icon.set_markup(&format!(
                    "<span foreground=\"{colour}\" size=\"x-large\">{}</span>",
                    escape_markup(glyph)
                ));
                // `show_child` instead of `set_visible`: these rows are
                // appended after `window.present()`, whose blanket `show_all`
                // never reaches late widgets — the rows box has to reveal each
                // one explicitly (same reason the button uses `show_child`).
                icon.set_visible(true);
                row_box.show_child(icon);
                // The row box itself may have been hidden when its tab closed
                // and reopened; re-show it so the icon update is visible.
                rows_box.show_child(row_box);
            }
            continue;
        }
        // A refused button is skipped for THIS tick, not dropped forever: the
        // cached signature is cleared so the next tick retries the row. (A bare
        // `continue` would have cached a signature that includes a row which
        // was never built, leaving the session invisible until something else
        // changed it.)
        let btn = match app.create_button(text) {
            Ok(b) => b,
            Err(_) => {
                st.signature.clear();
                continue;
            }
        };
        btn.set_hexpand(true);
        // Click behavior is fixed at creation: group headers toggle collapse,
        // session rows switch to their tab. The closure captures only ids +
        // shared handles, never a row index (rows move as tabs open/close).
        if *is_group {
            let key = id.trim_start_matches("g:").to_string();
            let st2 = state.clone();
            let _ = btn.on_click(move || {
                {
                    let mut s = st2.borrow_mut();
                    if s.collapsed.contains(&key) {
                        s.collapsed.remove(&key);
                    } else {
                        s.collapsed.insert(key.clone());
                    }
                    s.signature.clear(); // force a repaint next tick
                    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                    let mut ts = crate::session_tree::load_state(&cwd.to_string_lossy());
                    ts.collapsed.clone_from(&s.collapsed);
                    crate::session_tree::save_state(&cwd.to_string_lossy(), &ts);
                }
                // No immediate repaint: rows already exist from the first
                // tick, so a click only flips already-visible widgets.
            });
        } else {
            let target_id = id.clone();
            let sess2 = sessions.clone();
            let tabs2 = tabs.clone();
            let bus2 = bus.clone();
            let st2 = state.clone();
            let _ = btn.on_click(move || {
                // Session id → tab index (tabs reorder as sessions open/close,
                // so resolve at click time, not at row creation).
                let idx = sess2.borrow().iter().position(|sv| {
                    sv.agent_slot
                        .lock()
                        .unwrap()
                        .as_ref()
                        .and_then(|a| a.log_path.clone())
                        .is_some_and(|p| p.to_string_lossy() == target_id)
                });
                // Fallback for id-less tabs (`tab:{i}`): match by position.
                let idx = idx.or_else(|| {
                    target_id
                        .strip_prefix("tab:")?
                        .parse::<usize>()
                        .ok()
                        .filter(|&i| i < sess2.borrow().len())
                });
                if let Some(i) = idx {
                    // Selecting a session is an acknowledgement: its icon
                    // settles from `●` (stopped, unseen) to `○`.
                    st2.borrow_mut().acknowledge(&target_id);
                    tabs2.set_active(i);
                    if let Some(sv) = sess2.borrow().get(i) {
                        sv.entry.grab_focus();
                        // Focusing the session marks it seen: the unread badge
                        // clears (unread → seen) exactly when the operator looks.
                        if let Some(log) = sv
                            .agent_slot
                            .lock()
                            .unwrap()
                            .as_ref()
                            .and_then(|a| a.log_path.clone())
                        {
                            bus2.mark_seen(&log.to_string_lossy());
                        }
                    }
                }
            });
        }
        // Session rows get the status icon to the left of the title (group
        // headers do not carry a status). Icon + button live inside a
        // horizontal `row_box`, because the icon and button as siblings in
        // the vertical `rows_box` stack: the icon above, the button below.
        // The button is the one whose text `set_text` updates. Fall back to
        // a bare button (no icon) if the toolkit refuses either widget: a
        // missing icon must not hide the session.
        if !*is_group {
            let icon_label = app.create_label("").ok();
            if let Some(icon_label) = icon_label {
                icon_label.set_size_request(18, -1);
                #[cfg(not(windows))]
                icon_label.set_xalign(0.0);
                if let Some((_, glyph, colour)) = icon_of(id) {
                    icon_label.set_markup(&format!(
                        "<span foreground=\"{colour}\" size=\"x-large\">{}</span>",
                        escape_markup(glyph)
                    ));
                }
                // [icon][button] side by side: a horizontal row box wraps
                // both, and the row box is what is packed into the vertical
                // rows box. Rows appear after `window.present()`, so each
                // child must be revealed explicitly (`show_child`); the rows
                // box then re-runs size negotiation for the newcomer.
                let row_box = app
                    .create_box(Orientation::Horizontal, 2)
                    .expect("create_box sidebar row");
                // The button carries the row's flexible width; the icon keeps
                // its fixed 18px request, so the glyph cannot stretch the row.
                btn.set_hexpand(true);
                row_box.append(&icon_label);
                row_box.show_child(&icon_label);
                row_box.append(&btn);
                row_box.show_child(&btn);
                rows_box.append(&row_box);
                rows_box.show_child(&row_box);
                rows_box.queue_resize();
                st.rows.push(SidebarRow {
                    id: id.clone(),
                    button: btn,
                    is_group: false,
                    icon: Some(icon_label),
                    row_box: Some(row_box),
                });
                continue;
            }
        }
        rows_box.append(&btn);
        // Reveal the row itself (parent already visible) instead of
        // re-showing the whole box, and re-run size negotiation so the
        // newcomer is allocated now — not only after the next manual resize.
        rows_box.show_child(&btn);
        rows_box.queue_resize();
        st.rows.push(SidebarRow {
            id: id.clone(),
            button: btn,
            is_group: *is_group,
            icon: None,
            row_box: None,
        });
    }
    for row in &st.rows {
        if !seen.contains(&row.id) {
            // Icon and button live inside `row_box`: hiding the row hides
            // both, so a closed tab leaves no orphan glyph behind.
            if let Some(rb) = &row.row_box {
                rows_box.hide_child(rb);
            } else {
                rows_box.hide_child(&row.button);
                if let Some(icon) = &row.icon {
                    rows_box.hide_child(icon);
                }
            }
        }
    }
}

/// The sidebar's status-icon decision for one session row, applied to the
/// sticky acknowledgement map (`SidebarState::tree_status`) and returned.
///
/// The only edge that *creates* work for the operator is running → inactive:
/// the row becomes `StoppedUnseen` (amber). Everything else is sticky — an
/// amber row stays amber until [`SidebarState::acknowledge`], and a settled
/// row never re-lights on its own. A session first seen at rest is settled
/// (`StoppedSeen`), so opening the GUI onto finished sessions paints no amber.
fn sidebar_row_status(
    seen: &mut std::collections::HashMap<String, crate::session_tree::TreeStatus>,
    id: &str,
    active: bool,
) -> crate::session_tree::TreeStatus {
    use crate::session_tree::TreeStatus;
    let next = match seen.get(id).copied() {
        _ if active => TreeStatus::Running,
        Some(TreeStatus::Running) => TreeStatus::StoppedUnseen,
        Some(s) => s,
        None => TreeStatus::StoppedSeen,
    };
    seen.insert(id.to_string(), next);
    next
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

    // -- Build the window + menu bar + sidebar + tab container --
    let window = app.create_window().map_err(|e| format!("create_window: {e}"))?;
    window.set_title("pir");
    window.set_default_size(820, 600);

    let tabs = app.create_tabview().map_err(|e| format!("tabview: {e}"))?;
    // Pack-time rule (see the scroller in spawn_session): the tab container
    // must carry expand BEFORE it is packed into the root box, or GTK3
    // snapshots it as non-expanding and the whole pane keeps minimum size
    // in a field of blank space.
    // Left sessions tree: a narrow sidebar left of the conversation tabs,
    // grouped by project with one presence glyph per open session
    // (`▶` running, `●n` waiting-unread, `✓` waiting-seen, `·` idle — the same
    // `SessionPresence` model `/sessions` renders as text). Clicking a session
    // row switches to its tab; clicking a group header collapses it. The rows
    // are plain Buttons/Labels so both backends render them with zero
    // platform branching here — the only `cfg` is the scroller, which NWG
    // does not provide (there the rows box packs directly).
    let sidebar = app
        .create_box(Orientation::Vertical, 2)
        .map_err(|e| format!("create_box sidebar: {e}"))?;
    sidebar.set_size_request(220, -1);
    // Header row: [sessions] + [New Project…]. The button opens a directory
    // (existing, or typed in and created) and launches pir there in a new
    // process — see `launch_in_dir` for why a new process rather than a tab.
    let side_head_row = app
        .create_box(Orientation::Horizontal, 4)
        .map_err(|e| format!("create_box head row: {e}"))?;
    let side_head = app.create_label("sessions").map_err(|e| format!("sidebar head: {e}"))?;
    #[cfg(all(target_os = "linux", not(windows)))]
    side_head.set_xalign(0.0);
    side_head_row.append(&side_head);
    let new_project_btn = app
        .create_button("New Project")
        .map_err(|e| format!("create_button: {e}"))?;
    side_head_row.append(&new_project_btn);
    sidebar.append(&side_head_row);
    let side_rows = app
        .create_box(Orientation::Vertical, 1)
        .map_err(|e| format!("create_box rows: {e}"))?;
    side_rows.set_hexpand(true);
    #[cfg(all(target_os = "linux", not(windows)))]
    {
        let side_scroll = app
            .create_scrolled_window()
            .map_err(|e| format!("sidebar scroller: {e}"))?;
        side_scroll.set_policy(0, 1);
        side_scroll.set_child(&side_rows);
        side_scroll.set_hexpand(true);
        side_scroll.set_vexpand(true);
        sidebar.append(&side_scroll);
    }
    #[cfg(windows)]
    {
        sidebar.append(&side_rows);
    }
    // Owned row widgets, keyed by session id (or group key): `sync_sidebar`
    // updates their text in place and hides stale ones, so the 150ms drain
    // tick never churns widgets (no flicker, no focus theft).
    let sidebar_state: Rc<RefCell<SidebarState>> = Rc::new(RefCell::new(SidebarState::default()));

    // Horizontal content: [sidebar][tabs]. The tabs keep their expand flags;
    // the sidebar keeps its fixed width request. Pack-time rule (same as
    // TabView::new): the flags must be set BEFORE append, or GTK3 snapshots
    // the packed copy as non-expanding.
    let content = app
        .create_box(Orientation::Horizontal, 4)
        .map_err(|e| format!("create_box content: {e}"))?;
    content.set_hexpand(true);
    content.set_vexpand(true);
    // Pack the sidebar FIRST so it lands at the left edge, then the tabs.
    // Pack-time rule (same as TabView::new): set each child's expand flags
    // BEFORE its append, or GTK3 snapshots the packed copy as non-expanding.
    // The sidebar keeps a fixed 220px width: no hexpand, so it never shares
    // the content box's extra space (that gap at maximise was the sidebar
    // stretching with hexpand). It still expands vertically to fill height.
    sidebar.set_vexpand(true);
    content.append(&sidebar);
    tabs.set_hexpand(true);
    tabs.set_vexpand(true);
    content.append(&tabs);

    // The window's vertical stack: [menu bar] [session tabs]. Both are needed
    // before the first session, because the menu's actions dispatch into the
    // session that is current at the time they are chosen — so it reads
    // `sessions` lazily through an `Rc` rather than capturing one session.
    let root = app
        .create_box(Orientation::Vertical, 0)
        .map_err(|e| format!("create_box: {e}"))?;
    // NOTE: no expand flags on `root` itself: `window.set_child` is not a
    // box pack (the window takes exactly one child and sizes it to fill),
    // so there is no pack-time snapshot to beat. The flags that matter are
    // on `tabs` (packed into `root`) and inside each session box.

    let sessions: Rc<RefCell<Vec<SessionView>>> = Rc::new(RefCell::new(Vec::new()));
    let active: Rc<Cell<usize>> = Rc::new(Cell::new(0));

    // One SessionView per open tab, indexed identically to the TabView.

    // "New Project": pick a directory, then open pir there in a new process.
    // Wired here (not at creation) because the picker reports into the
    // *current* session's pane, which needs `sessions`/`tabs`.
    {
        let btn_sessions = sessions.clone();
        let btn_tabs = tabs.clone();
        let btn_app = app.clone();
        let _ = new_project_btn.on_click(move || {
            let sv = { btn_sessions.borrow().get(btn_tabs.current_tab()).cloned() };
            // No current session would mean the tab list is empty, which only
            // happens before the first `spawn_session`; nothing to report into
            // then, so the picker simply is not opened.
            if let Some(sv) = sv {
                open_project_picker(&btn_app, &sv.state, &sv.widgets, _full_auto, None);
            }
        });
    }

    // Switching tabs just moves keyboard focus to that tab's prompt. This is
    // the *only* place focus is moved deliberately — the old unconditional
    // focus-out re-grab was a trap (see `spawn_session`). It also acknowledges
    // the session being brought to the front: a `●` (stopped, unseen) sidebar
    // icon settles to `○`, because switching to a tab means the operator has
    // seen where it stopped.
    {
        let sessions_for_cb = sessions.clone();
        let active_for_cb = active.clone();
        let bus_for_cb = bus.clone();
        let side_state_for_cb = sidebar_state.clone();
        tabs.set_on_tab_changed(Box::new(move |idx: usize| {
            active_for_cb.set(idx);
            let s = sessions_for_cb.borrow();
            if let Some(sv) = s.get(idx) {
                let id = sv
                    .agent_slot
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|a| a.log_path.clone())
                    .map(|p| p.to_string_lossy().into_owned());
                // Bringing a session to the front acknowledges it: its sidebar
                // icon settles `●` → `○` and its unread badge clears at the
                // same moment.
                if let Some(id) = id.as_deref() {
                    side_state_for_cb.borrow_mut().acknowledge(id);
                    bus_for_cb.mark_seen(id);
                }
                sv.entry.grab_focus();
            }
        }));
    }

    // The terminal's `/menu` as a clickable bar, above the menu bar. The
    // operator's report was "there isn't really an equivalent to all the
    // /menu options in topbar": the options were reachable (typed `/menu
    // <sub>`, or a Help item the old `HELP_GUI` did not mention), but nothing
    // in the window showed them. One item per `/menu` action, in the
    // terminal's own order, so the GUI's first look matches the terminal's.
    match build_menu_strip(&app, &sessions, &tabs, providers, agent_slot) {
        Ok(strip) => {
            root.append(&strip);
        }
        Err(e) => {
            // Degraded, not fatal — the menu bar and typed commands remain.
            eprintln!("pir: /menu bar unavailable ({e}) — /menu <action> still works");
        }
    }

    // The menu bar, built before the first session so its actions can dispatch
    // into whatever session is current when they are chosen. Appended above
    // the tabs: without this the window shows only the tab-bar button, which
    // with one tab is the dead "Session button" from the report.
    match build_menu_bar(&app, &window, &sessions, &tabs, providers, bus, _full_auto, agent_slot) {
        Ok(bar) => {
            root.append(&bar);
        }
        Err(e) => {
            // A missing menu bar is a degraded GUI, not a fatal one: the whole
            // command surface is still reachable by typing it. Say so once and
            // carry on rather than refusing to open the window.
            eprintln!("pir: menu bar unavailable ({e}) — every command is still available by typing it");
        }
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

    root.append(&content);
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
    // The left tree refreshes on the same tick: same snapshot, then one
    // `sync_sidebar` that rebuilds only what changed (see its signature guard).
    let side_sessions = sessions.clone();
    let side_tabs = tabs.clone();
    let side_bus = bus.clone();
    let side_rows = side_rows.clone();
    let side_app = app.clone();
    let side_state = sidebar_state.clone();
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
                sync_sidebar(&side_app, &side_rows, &side_state, &side_sessions, &side_tabs, &side_bus);
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
        let side_sessions = side_sessions.clone();
        let side_tabs = side_tabs.clone();
        let side_bus = side_bus.clone();
        let side_rows = side_rows.clone();
        let side_app = side_app.clone();
        let side_state = side_state.clone();
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
            sync_sidebar(&side_app, &side_rows, &side_state, &side_sessions, &side_tabs, &side_bus);
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
            &sv.hist_nav,
            sv,
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

/// Submenu names in bar order: first-seen across both tables, except "Help"
/// is always last (far right, the platform convention). Pure so the order is
/// testable without a display — `build_menu_bar` seeds from exactly this.
fn menu_order() -> Vec<&'static str> {
    let mut order: Vec<&'static str> = Vec::new();
    let mut seen = |name: &'static str| {
        if name != "Help" && !order.contains(&name) {
            order.push(name);
        }
    };
    for a in MENU_ACTIONS {
        seen(a.menu);
    }
    for (menu, _, _) in MENU_VIEW_ACTIONS {
        seen(menu);
    }
    order.push("Help");
    order
}

/// Build the menu bar and return it; the caller appends it to the window.
///
/// Each item in [`MENU_ACTIONS`] becomes a GMenu entry bound to a
/// `SimpleAction`; activating it routes the *same* command through the *same*
/// `handle_command` the typed line uses. That is the point of the table: there
/// is exactly one implementation of every command, so a menu item and a typed
/// line cannot drift apart in behaviour — only in how they were reached.
///
/// [`MENU_VIEW_ACTIONS`] items are pure view actions (scroll the conversation,
/// mark the session finished via the `/finished` path): they move
/// [`GuiScroll`] / `follow` through the same helpers the Page_Up/Page_Down/
/// Home/End keys use, so key and menu scroll the same window.
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
    agent_slot: &AgentSlot,
) -> Result<MenuBar, String> {
    // The action group has to exist and be inserted into the window *before* the
    // menu model is built, or the actions have nothing to bind to. The group
    // name and every menu item's detailed action must agree — see
    // [`MENU_ACTION_GROUP`]: a bare name never resolves, and GTK greys the
    // item out instead of reporting an error.
    let group = app.ensure_action_group().map_err(|e| format!("action group: {e}"))?;
    unsafe { window.insert_action_group(MENU_ACTION_GROUP, group) };

    // Submenus in menu_order() (first-seen across both tables, Help last).
    // View actions land in the same submenus as the commands (both tables use
    // "View" and "Turn"), creating the submenu when the command table did not.
    let mut subs: Vec<(&str, Menu)> = Vec::new();
    let sub_at = |subs: &mut Vec<(&str, Menu)>,
                      app: &App,
                      name: &'static str|
     -> Result<usize, String> {
        if let Some(i) = subs.iter().position(|(n, _)| *n == name) {
            return Ok(i);
        }
        let menu = app.new_menu().map_err(|e| format!("new_menu: {e}"))?;
        subs.push((name, menu));
        Ok(subs.len() - 1)
    };
    // Seed submenu order from menu_order() so the bar and the test agree by
    // construction rather than by convention.
    for name in menu_order() {
        sub_at(&mut subs, app, name)?;
    }

    for a in MENU_ACTIONS {
        let i = sub_at(&mut subs, app, a.menu)?;
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
        // A failed connect/register leaves a live-looking item that can
        // never fire — exactly "doesn't seem to do anything". Surface it
        // loudly instead of swallowing the Result.
        let action_name = a.action.to_string();
        // The `&App` this function borrows cannot be captured by the
        // `'static` callback; its address is carried as a `usize` (`Send +
        // Sync` unconditionally) and reborrowed at the call site — the same
        // technique, and the same safety argument, as the ask-renderer
        // install in `run`: the `App` outlives every callback because `run`
        // does not return until the GTK loop ends.
        let app_addr: usize = app as *const App as usize;
        if action.connect_activate(move |_data: *mut c_void| {
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
        }).is_err() {
            eprintln!("pir: menu action {action_name:?} not connected");
        }
        if let Err(e) = app.register_action(&action) {
            eprintln!("pir: menu action {action_name:?} not registered: {e}");
        }
        // Keep the action alive for the process lifetime: GTK holds only a
        // ref to it, and dropping our handle here would unbind the item.
        std::mem::forget(action);
        // Qualified with the action group (`pir.session-new`, …): GTK's
        // popover bar resolves `group.name` against the group the window
        // inserted above. A bare name matches nothing, and GTK greys the
        // item out instead of reporting an error. Registration stays short
        // (`new_simple_action(a.action)`), which every backend's dispatch
        // strips back to — see `menu_detailed_action`.
        subs[i].1.append(a.label, &menu_detailed_action(a.action));
    }

    // Registers one model leaf: wires `/model <provider>/<full-id>` through
    // `run_command_line` (the typed line's own path) and appends the item to
    // `menu`. One definition for flat and grouped leaves alike.
    #[allow(clippy::too_many_arguments)]
    fn wire_model_leaf(
        app: &App,
        menu: &mut Menu,
        sessions: &Rc<RefCell<Vec<SessionView>>>,
        tabs: &TabView,
        providers: &[Provider],
        bus: &SharedBus,
        full_auto: bool,
        pid: &str,
        label: &str,
        action_name: &str,
        mid: &str,
    ) -> Result<(), String> {
        let sessions = sessions.clone();
        let tabs = tabs.clone();
        let providers = providers.to_vec();
        let bus = bus.clone();
        let line = format!("/model {pid}/{mid}");
        let action = app
            .new_simple_action(action_name)
            .map_err(|e| format!("new_simple_action({action_name}): {e}"))?;
        let action_name_dbg = action_name.to_string();
        if action.connect_activate(move |_data: *mut c_void| {
            let sv = { sessions.borrow().get(tabs.current_tab()).cloned() };
            let Some(sv) = sv else { return };
            let mut s = sv.state.lock().unwrap();
            s.push(ConvKind::System, &format!("· menu: {line}"));
            drop(s);
            run_command_line(&line, &sv, &sessions, &tabs, &providers, &bus, full_auto);
        }).is_err() {
            eprintln!("pir: menu action {action_name_dbg:?} not connected");
        }
        if let Err(e) = app.register_action(&action) {
            eprintln!("pir: menu action {action_name_dbg:?} not registered: {e}");
        }
        std::mem::forget(action);
        menu.append(label, &menu_detailed_action(action_name));
        Ok(())
    }

    // Model tree (Providers > Models): one submenu per provider under the
    // "Model" menu, each listing that provider's models. Rendered from
    // `model_menu_tree` so the shape is pinned by unit test; each leaf runs
    // `/model <provider>/<id>` through `run_command_line` — the same path a
    // typed line takes, so menu and keyboard cannot disagree. The active model
    // (from the initial agent) carries the `●` marker; switching models
    // re-marks via the status line ("→ label"), since GMenu items cannot be
    // relabeled live.
    {
        let active = agent_slot
            .lock()
            .unwrap()
            .as_ref()
            .map(|a| a.label())
            .unwrap_or_default();
        let tree = model_menu_tree(providers, &active);
        let mi = sub_at(&mut subs, app, "Model")?;
        // One leaf's action wiring, shared by flat leaves and grouped ones:
        // run `/model <provider>/<full-id>` through `run_command_line`, the
        // same path a typed line takes.
        for (pid, entries) in &tree {
            let mut psm = app.new_menu().map_err(|e| format!("new_menu: {e}"))?;
            // Registers one leaf action and appends it to `menu`. Factored out
            // of the loop bodies so flat and grouped leaves cannot drift.
            for entry in entries {
                match entry {
                    ModelMenuEntry::Leaf(label, action_name, mid) => {
                        wire_model_leaf(
                            app,
                            &mut psm,
                            &sessions,
                            tabs,
                            providers,
                            bus,
                            full_auto,
                            pid,
                            label,
                            action_name,
                            mid,
                        )?;
                    }
                    ModelMenuEntry::Group(org, leaves) => {
                        let mut gsm =
                            app.new_menu().map_err(|e| format!("new_menu: {e}"))?;
                        for (label, action_name, mid) in leaves {
                            wire_model_leaf(
                                app,
                                &mut gsm,
                                &sessions,
                                tabs,
                                providers,
                                bus,
                                full_auto,
                                pid,
                                label,
                                action_name,
                                mid,
                            )?;
                        }
                        psm.append_submenu(org, &gsm);
                    }
                }
            }
            subs[mi].1.append_submenu(pid, &psm);
        }
    }

    // Pure view actions: no slash command of their own, so they live apart
    // from `MENU_ACTIONS` (see its doc). Each moves the current session's
    // scroll window through the same helpers the Page_Up/Page_Down/Home/End
    // keys use — key and menu scroll the same window by construction.
    // `view-finished` routes through the `/finished` command path instead, so
    // there is one implementation of "mark finished" rather than two.
    for (menu, label, view_action) in MENU_VIEW_ACTIONS {
        let i = sub_at(&mut subs, app, menu)?;
        let sessions = sessions.clone();
        let tabs = tabs.clone();
        let providers = providers.to_vec();
        let bus = bus.clone();
        let view_action = view_action.to_string();
        let view_action_cb = view_action.clone();
        let action = app
            .new_simple_action(view_action.as_str())
            .map_err(|e| format!("new_simple_action({view_action}): {e}"))?;
        if action.connect_activate(move |_data: *mut c_void| {
            let sv = { sessions.borrow().get(tabs.current_tab()).cloned() };
            let Some(sv) = sv else { return };
            if view_action_cb == "view-finished" {
                run_command_line("/finished", &sv, &sessions, &tabs, &providers, &bus, full_auto);
                return;
            }
            if view_action_cb == "edit-copy" {
                sv.widgets.textview.copy_clipboard();
                return;
            }
            let mut s = sv.state.lock().unwrap();
            match view_action_cb.as_str() {
                "view-end" => scroll_end(&mut s),
                "view-page-up" => scroll_page_up(&mut s),
                "view-page-down" => scroll_page_down(&mut s),
                _ => return,
            }
            sync_textview(&sv.widgets, &mut s);
        }).is_err() {
            eprintln!("pir: menu action {view_action:?} not connected");
        }
        if let Err(e) = app.register_action(&action) {
            eprintln!("pir: menu action {view_action:?} not registered: {e}");
        }
        std::mem::forget(action);
        // Same group qualification as the command items above: without the
        // `pir.` prefix GTK cannot resolve the item and greys it out.
        subs[i].1.append(label, &menu_detailed_action(&view_action));
    }

    let mut root = app.new_menu().map_err(|e| format!("new_menu: {e}"))?;
    for (name, sub) in &subs {
        root.append_submenu(name, sub);
    }

    let bar = app
        .new_menubar(&root, group)
        .map_err(|e| format!("new_menubar: {e}"))?;
    // The bar is the window's top child of the root box; returning it (rather
    // than dropping it here) keeps the widget alive past this function, and
    // the caller appends it above the tabs. Dropping it here would free an
    // unparented widget — the old drop-the-bar bug that left the window
    // with only the dead tab-bar Session button.
    Ok(bar)
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
    // The scroller, not the TextView, is what must claim the free space, and it
    // needs a sensible minimum height or the pane collapses when a turn is long.
    // GTK3's pack snapshots expand AT PACK TIME: these must be set BEFORE the
    // append below, or the scroller keeps its minimum size and the pane is a
    // blank strip at the top of a mostly-empty window.
    scroller.set_hexpand(true);
    scroller.set_vexpand(true);
    scroller.set_size_request(-1, 200);
    vbox.append(&scroller);

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
        // Fit the picker to the CURRENT model, not the longest catalog id:
        // GTK3 sizes a ComboBoxText to its longest item, so a fixed 220px
        // (or the natural size) leaves a fat box around a short
        // "ollama-cloud/x" label and steals Entry space on every tab.
        // `agent_slot` holds the model this tab actually runs on.
        let current_label = agent_slot
            .lock()
            .unwrap()
            .as_ref()
            .map(|a| a.label())
            .unwrap_or_else(|| model_items.first().cloned().unwrap_or_default());
        picker.set_size_request(picker_width_for(&current_label), -1);
        let picker_state = state_cell.clone();
        let picker_slot = agent_slot.clone();
        let picker_widgets = widgets_cell.clone();
        let picker_providers = providers.to_vec();
        // `DropDown` is not `Clone`, and `connect_changed` takes a `'static`
        // closure, so the widget cannot be captured by value. `selection_handle`
        // returns a cheap cloneable handle to the same widget on both backends
        // (a `#[repr(transparent)]` loader type on GTK, an `Rc` on NWG), so the
        // shared GUI code does not have to name a backend-specific field.
        let picker_handle = picker.selection_handle();
        let picker_size = picker.clone();
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
                        Ok(()) => {
                            let label = agent.label();
                            s.push(ConvKind::System, &format!("→ {label}"));
                            // Re-fit the picker to the newly selected model so
                            // a long id does not permanently stretch the row
                            // and a short one does not swim in a 220px box.
                            picker_size.set_size_request(picker_width_for(&label), -1);
                        }
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
    // Faint hint while the prompt is empty. Native placeholder: GTK renders it
    // greyed and it never becomes the value, so submit paths need no filtering.
    entry.set_placeholder_text("Enter prompt here");
    prompt_row.append(&entry);

    // The Send button (G3). Same function as Enter, so there is one definition
    // of "submit the prompt line" rather than two that can drift.
    //
    // Created AND packed here, before `panel.append(&vbox)` + `vbox.show_all()`
    // below — but wired up at the end of `spawn_session`, next to the Enter
    // handler, where the submit-path captures (`sv_for_submit`, ...) exist.
    // GTK3 creates widgets hidden and only `window.present()`'s blanket
    // show_all reveals what exists at that point — the first tab ("Session")
    // is built before present(), so a Send button appended after show_all
    // still appears; every later tab ("Session 1", ...) is built after
    // present(), so its pane is revealed only by the explicit `vbox.show_all()`
    // below, and anything appended after that call stays hidden. Appending Send
    // after show_all is exactly why "Session 1" has no Send button while
    // "Session" does.
    //
    // A `create_button` failure is loud, not silent: `if let Ok` would
    // swallow it and leave a tab with an Entry that can only be submitted
    // via keyboard Enter — the same "missing Send button" symptom from a
    // different cause. (The button used to be created AND wired after
    // `vbox.show_all()` near the end of `spawn_session`; creation/packing
    // moved up here, which is the whole fix.)
    let send_btn = app.create_button("Send").expect("create_button: Send");
    prompt_row.append(&send_btn);

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

    // Pack-time rule (see the scroller above): the prompt row must carry
    // hexpand BEFORE it is packed, or GTK3 snapshots it as non-expanding.
    prompt_row.set_hexpand(true);
    vbox.append(&prompt_row);

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
    // Pack-time rule again: the session box must carry expand BEFORE it is
    // packed into the tab panel, or the whole pane renders at minimum size.
    vbox.set_hexpand(true);
    vbox.set_vexpand(true);
    panel.append(&vbox);
    // GTK3 creates widgets hidden; only window.present()'s blanket show_all
    // reveals what exists at that point. The first tab is built before
    // present(), but every later tab ("New session") is built after — without
    // this its pane stays invisible and the session looks like it did nothing.
    // (The tab *button* is shown by TabView::add_tab itself.)
    vbox.show_all();

    let state = Arc::new(Mutex::new(GuiState::new()));
    let widgets = GuiWidgets {
        textview: textview.clone(),
        status: status.clone(),
        scroller: scroller.clone(),
        hint: hint.clone(),
    };
    // Clickable links in the conversation pane (design row G6): a left-button
    // release over a recorded link span opens it in the browser. Wired once
    // per tab — the handler reads that tab's own `buf_links` map, so tabs
    // never resolve each other's clicks.
    #[cfg(all(target_os = "linux", not(windows)))]
    connect_link_clicks(&textview, &state);
    #[cfg(windows)]
    connect_link_clicks(&textview, &state);
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
        // Greet with the live agent's model, not the catalog's first entry:
        // `providers[0].pid()` names whatever provider happens to be listed
        // first, which is stale the moment the session runs on anything else
        // (the reported ". pir . cerebras" while ollama-cloud was selected).
        // The agent slot holds the model this tab actually runs on.
        let live_label = agent_slot
            .lock()
            .unwrap()
            .as_ref()
            .map(|a| a.label())
            .unwrap_or_else(|| providers[0].label(&providers[0].models[0]));
        s.push(
            ConvKind::System,
            &format!(
                "pir · {} · GTK GUI  (/help · /sessions new|close|next|prev|<n> · Enter to send · \
                 esc or ctrl-c cancels · ctrl-q quits)",
                live_label
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
            // Esc cancels the running turn — the same affordance the TUI has
            // and the GUI previously lacked, where the only way to stop work
            // was to type `/cancel`. Shares the command's logic, so a key press
            // and the typed command can never disagree about what "cancel"
            // means.
            KEY_ESCAPE => {
                cancel_running(&cancel_for_keys, &state_for_keys, &widgets_for_keys);
                true
            }
            // Ctrl-C cancels the running turn — but ONLY when the conversation
            // pane does not hold focus. This handler is wired on the prompt
            // Entry, so it normally fires only while typing there; but GTK
            // key-press propagation can deliver the Entry's handler while the
            // TextView is focused (e.g. after clicking into the log to select
            // text). Claiming Ctrl-C unconditionally would swallow the
            // TextView's own copy binding: the user selects log text, hits
            // Ctrl-C, and gets "no turn running to cancel" instead of a
            // clipboard copy. When the pane is focused the key must pass
            // through (return false) so GTK's copy-clipboard binding runs.
            KEY_C if ctrl => {
                if widgets_for_keys.textview.has_focus() {
                    return false;
                }
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
                // Record what Tab did, so the hint line can *show* it (see
                // `completion_hint`). Tab used to be completely silent: `/men`
                // became `/menu` with no comment, and `/m` (three matches)
                // changed nothing at all with no explanation.
                {
                    let mut s = state_for_keys.lock().unwrap();
                    match &completed {
                        Some(c) if *c != cur => {
                            s.tab_note = Some(format!("[Tab] completed to {c}"));
                        }
                        _ => {
                            // It could not complete: say why — several
                            // candidates (listed by the hint), or none.
                            let n = completion_candidates(&cur).len();
                            s.tab_note = Some(match n {
                                0 => "[Tab] no match".to_string(),
                                1 => "[Tab] already complete".to_string(),
                                n => format!("[Tab] {n} matches"),
                            });
                        }
                    }
                    // Repaint the hint now rather than waiting for the next
                    // drain tick: the whole point is that the answer to Tab
                    // appears while the operator is looking at it.
                    let hint = completion_hint(&cur, s.tab_note.as_deref(), true);
                    if s.last_hint != hint {
                        if hint.is_empty() {
                            widgets_for_keys.hint.set_markup("");
                        } else {
                            widgets_for_keys.hint.set_markup(&status_markup(&hint));
                        }
                        s.last_hint = hint;
                    }
                }
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
            // Keyboard scrollback (design row S6): Page_Up / Page_Down move the
            // `GuiScroll` window one page, Home jumps to the top, End returns
            // to the live tail (re-arming follow). The toolkit has no
            // `gtk_adjustment_set_value`, so this repaints the windowed slice
            // instead of moving the adjustment — same helpers the View menu
            // uses, so key and menu scroll the same window.
            GDK_KEY_PAGE_UP => {
                let mut s = state_for_keys.lock().unwrap();
                scroll_page_up(&mut s);
                sync_textview(&widgets_for_keys, &mut s);
                true
            }
            GDK_KEY_PAGE_DOWN => {
                let mut s = state_for_keys.lock().unwrap();
                scroll_page_down(&mut s);
                sync_textview(&widgets_for_keys, &mut s);
                true
            }
            GDK_KEY_HOME => {
                let mut s = state_for_keys.lock().unwrap();
                scroll_home(&mut s);
                sync_textview(&widgets_for_keys, &mut s);
                true
            }
            GDK_KEY_END => {
                let mut s = state_for_keys.lock().unwrap();
                scroll_end(&mut s);
                sync_textview(&widgets_for_keys, &mut s);
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

    // Wire the Send button created up in the prompt row (BEFORE show_all) to
    // the same submit path as Enter, so a button press and a keypress cannot
    // diverge. The button widget itself was packed long before
    // `vbox.show_all()` so post-present tabs show it; only the callback —
    // which needs the captures built just above — is attached here.
    {
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
    if let Some((vpos, vmax, page)) = scrolled_window_range(&widgets.scroller) {
        s.observe_scroll(vpos, vmax, page);
    }

    // Mirror the prompt Entry's text into the model. The hint line and the
    // done-placeholder are both derived from the draft, and in a GUI the Entry
    // owns it — so it is copied in here, once per drain, rather than becoming a
    // second source of truth updated on every keystroke. The TUI owns the draft
    // outright and derives both directly (`tui.rs:684`).
    let draft_now = entry.get_text().unwrap_or_default();
    if draft_now != s.draft {
        // A Tab note describes the line it was produced for. Editing the line
        // (or Tab itself changing it) retires it, so "[Tab] no match" can never
        // sit under a line the operator has since made matchable.
        s.tab_note = None;
    }
    s.draft = draft_now;

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
///
/// This covers what the *typed line* can be completed to. It used to answer
/// only for exactly `/` and `/thinking`, so `/men` — which Tab happily
/// completes to `/menu` — got no hint at all, and nothing in the window said
/// which commands existed. [`completion_hint`] now answers for every draft with
/// candidates, and carries the outcome of a Tab press (`state.tab_note`).
fn hint_text(draft: &str, tab_note: Option<&str>) -> String {
    if draft == "/thinking" && tab_note.is_none() {
        "[Tab] levels: off | minimal | low | medium | high | xhigh | max · show | hide".to_string()
    } else {
        completion_hint(draft, tab_note, true)
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

/// Width, in pixels, for the model picker showing `label`.
///
/// GTK3's ComboBoxText sizes to its longest *item*, not the selected one, so
/// a catalog with one long id stretches every tab's prompt row and steals the
/// Entry's space. Instead the picker is pinned to just the current model via
/// `set_size_request`: ~8px per char (the toolkit's own headless fallback
/// width in `measure_text_px`) plus room for the dropdown arrow and padding.
/// The same estimate is used with and without a display, so tests pin it
/// without needing GTK.
fn picker_width_for(label: &str) -> i32 {
    const PER_CHAR_PX: usize = 8;
    const ARROW_AND_PADDING_PX: i32 = 40;
    const MIN_PX: i32 = 80;
    const MAX_PX: i32 = 420;
    let text = (label.chars().count() * PER_CHAR_PX) as i32 + ARROW_AND_PADDING_PX;
    text.clamp(MIN_PX, MAX_PX)
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
    ("md_demo", "markup_demo"),
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

/// The name under which `build_menu_bar` inserts the action group on the
/// window (`window.insert_action_group("pir", group)`).
///
/// Every `GMenu` item's detailed action must be qualified with this group
/// (`pir.session-new`, …): the actions themselves are registered under their
/// short names, and GTK resolves a detailed action `group.name` by looking
/// `name` up in `group`. A bare name (`session-new`) matches no group GTK
/// searches (`app`/`win`), so GTK renders the item insensitive — greyed out,
/// unclickable. This is exactly the convention the toolkit's own working
/// example uses (`file_menu.append("New", "app.new")` with
/// `app.new_simple_action("new")`), and the NWG backend documents the other
/// half (`app.rs`: "action names must match the short name used by the
/// backend's menu dispatch (NWG strips the app. prefix before lookup)").
/// Keep the two spellings consistent: registration stays short (strippable
/// everywhere), menu strings carry the prefix.
const MENU_ACTION_GROUP: &str = "pir";

/// The detailed action string stored in the `GMenu` model for a menu item.
///
/// GTK's `GtkPopoverMenuBar` resolves `group.name` by looking `name` up in the
/// action group the window inserted under `group`. The group here is always
/// [`MENU_ACTION_GROUP`], and actions are always registered under their short
/// names — so the detailed name is always `pir.<action>`. Centralised (rather
/// than inlined at the append sites) so the group prefix and the insert call
/// cannot drift apart: a mismatch greys out the whole menu with no error.
fn menu_detailed_action(action: &str) -> String {
    format!("{MENU_ACTION_GROUP}.{action}")
}

/// One provider's models as a menu subtree: the provider label plus
/// `(model_label, action_name, model_id)` per model. Pure so the tree shape is
/// testable without a display — `build_menu_bar` renders exactly this.
///
/// Models whose id carries an `org/` prefix (openrouter's `anthropic/claude…`
/// shape — the only catalog entries with a `/`) split into a nested layer:
/// `(group, [(leaf_label, action, full_id)])`, so `openrouter >
/// anthropic/claude-sonnet-5` becomes `openrouter > anthropic >
/// claude-sonnet-5`. The leaf label keeps the full id's tail; the action still
/// resolves the full `provider/id` through this same tree at click time.
/// One row in a provider's Model submenu: either a direct model leaf or a
/// nested `org/` group of leaves. Groups come from model ids carrying an
/// `org/` prefix (openrouter's `anthropic/claude…` shape — the only catalog
/// entries with a `/`): `openrouter > anthropic/claude-sonnet-5` renders as
/// `openrouter > anthropic > claude-sonnet-5`. The leaf label shows the id
/// tail; dispatch still uses the full `provider/id` through this same tree
/// at click time, so the grouping can never misroute a switch.
#[derive(Debug)]
enum ModelMenuEntry {
    /// Direct model leaf: `(label, action_name, full_model_id)`.
    Leaf(String, String, String),
    /// Nested group: `(org, [(label, action_name, full_model_id)])`.
    Group(String, Vec<(String, String, String)>),
}
fn model_menu_tree(providers: &[Provider], active: &str) -> Vec<(String, Vec<ModelMenuEntry>)> {
    providers
        .iter()
        .enumerate()
        .map(|(pi, p)| {
            let pid = p.pid();
            let mut entries: Vec<ModelMenuEntry> = Vec::new();
            // org -> index into `entries`, so groups materialize in catalog
            // (first-encounter) order and flat leaves keep their positions.
            let mut groups: Vec<(String, usize)> = Vec::new();
            for (mi, m) in p.models.iter().enumerate() {
                let id = m.id.clone();
                // Action names must survive GTK dispatch: model ids carry
                // `/`, `:`, `~` (e.g. `~anthropic/claude-sonnet-latest`),
                // so use positional `model-p<pi>-m<mi>` names and resolve
                // back through this same tree at click time.
                let action = format!("model-p{pi}-m{mi}");
                let marked = format!("{pid}/{id}") == active;
                match id.split_once('/') {
                    Some((org, tail)) if !org.is_empty() && !tail.is_empty() => {
                        let show = tail.to_string();
                        let label = if marked {
                            format!("● {show}")
                        } else {
                            format!("  {show}")
                        };
                        match groups.iter().position(|(g, _)| g == org) {
                            Some(pos) => {
                                let gi = groups[pos].1;
                                if let ModelMenuEntry::Group(_, leaves) =
                                    &mut entries[gi]
                                {
                                    leaves.push((label, action, id));
                                }
                            }
                            None => {
                                groups.push((org.to_string(), entries.len()));
                                entries.push(ModelMenuEntry::Group(
                                    org.to_string(),
                                    vec![(label, action, id)],
                                ));
                            }
                        }
                    }
                    // No (usable) `/`: a flat leaf showing the full id.
                    _ => {
                        let label = if marked {
                            format!("● {id}")
                        } else {
                            format!("  {id}")
                        };
                        entries.push(ModelMenuEntry::Leaf(label, action, id));
                    }
                }
            }
            (pid, entries)
        })
        .collect()
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
    // Opens a project directory (existing, or a new one to create) in a new
    // window. Goes through `handle_command` like every other item, so the menu,
    // the sidebar button and `/new-project` share one implementation.
    MenuAction { menu: "Session", label: "New project…", action: "session-new-project", command: "new-project", arg: "" },
    MenuAction { menu: "Session", label: "Close session", action: "session-close", command: "sessions", arg: "close" },
    MenuAction { menu: "Session", label: "List sessions", action: "session-list", command: "sessions", arg: "" },
    MenuAction { menu: "Session", label: "Run a shell command", action: "session-shell", command: "sh", arg: "" },
    MenuAction { menu: "Session", label: "Run in background", action: "session-bg", command: "bg", arg: "" },
    MenuAction { menu: "Session", label: "List background jobs", action: "session-jobs", command: "jobs", arg: "" },
    MenuAction { menu: "Session", label: "List checkpoints", action: "session-checkpoints", command: "checkpoints", arg: "" },
    MenuAction { menu: "Session", label: "Set goal", action: "session-goal", command: "goal", arg: "" },
    MenuAction { menu: "Session", label: "Continue goal", action: "session-continue", command: "continue", arg: "" },
    MenuAction { menu: "Session", label: "Set thinking level", action: "session-thinking", command: "thinking", arg: "" },
    MenuAction { menu: "Session", label: "Resume session…", action: "session-resume", command: "menu", arg: "resume" },
    MenuAction { menu: "Session", label: "Backgrounded sessions…", action: "session-backgrounded", command: "menu", arg: "backgrounded" },
    MenuAction { menu: "Session", label: "Worktrees by default…", action: "session-worktrees", command: "menu", arg: "worktrees" },
    MenuAction { menu: "Session", label: "Autoclean coverage…", action: "session-coverage", command: "menu", arg: "coverage" },
    // Model
    MenuAction { menu: "Model", label: "Switch model", action: "model-switch", command: "model", arg: "" },
    MenuAction { menu: "Model", label: "List models", action: "model-list", command: "models", arg: "" },
    MenuAction { menu: "Model", label: "Pick model…", action: "model-pick", command: "menu", arg: "model" },
    // Turn control
    MenuAction { menu: "Turn", label: "Cancel turn", action: "turn-cancel", command: "cancel", arg: "" },
    MenuAction { menu: "Turn", label: "Undo last turn", action: "turn-undo", command: "undo", arg: "" },
    MenuAction { menu: "Turn", label: "Rewind", action: "turn-rewind", command: "rewind", arg: "" },
    MenuAction { menu: "Turn", label: "List checkpoints", action: "turn-checkpoints", command: "checkpoints", arg: "" },
    MenuAction { menu: "Turn", label: "Compact conversation", action: "turn-compact", command: "compact", arg: "" },
    MenuAction { menu: "Turn", label: "Context usage", action: "turn-compact-status", command: "compact", arg: "status" },
    // View
    MenuAction { menu: "View", label: "Clear pane", action: "view-clear", command: "clear", arg: "" },
    // Safety
    MenuAction { menu: "Safety", label: "Review staged writes", action: "safety-quarantine", command: "quarantine", arg: "" },
    MenuAction { menu: "Safety", label: "Resource limits", action: "safety-limits", command: "limits", arg: "" },
    MenuAction { menu: "Safety", label: "Security options…", action: "safety-security", command: "menu", arg: "security" },
    MenuAction { menu: "Safety", label: "Settings…", action: "safety-settings", command: "menu", arg: "settings" },
    MenuAction { menu: "Safety", label: "Thinking options…", action: "safety-thinking", command: "menu", arg: "thinking" },
    // Info
    MenuAction { menu: "Help", label: "Commands", action: "help-commands", command: "help", arg: "" },
    MenuAction { menu: "Help", label: "Menu…", action: "help-menu", command: "menu", arg: "" },
    MenuAction { menu: "Help", label: "About pir", action: "help-about", command: "menu", arg: "about" },
    MenuAction { menu: "Help", label: "Skills", action: "help-skills", command: "skills", arg: "" },
    MenuAction { menu: "Help", label: "Session scratchpad", action: "help-scratchpad", command: "scratchpad", arg: "" },
    MenuAction { menu: "Help", label: "Token usage", action: "help-usage", command: "usage", arg: "" },
    MenuAction { menu: "Help", label: "Markdown demo", action: "help-markup-demo", command: "markup_demo", arg: "" },
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
    // Edit menu: the log pane is read-only, so Copy is the only edit action
    // that makes sense — it copies the TextView's current selection to the
    // CLIPBOARD via gtk_text_buffer_copy_clipboard (GTK's own copy path).
    // Without this there is no menu copy at all, and Ctrl-C over a selection
    // used to cancel the turn instead (see the KEY_C handler).
    ("Edit", "Copy", "edit-copy"),
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
/// Summarise the conversation on a worker thread, moving the agent out of the
/// slot for the duration (the same shape as [`spawn_goal`]).
///
/// `/compact` is a full generation over the whole transcript — the slowest
/// single call a session makes — so it must never run on the GTK main thread.
/// The result reaches the operator through two channels the drain already
/// handles: the agent's own `compact()` notice is appended to the session log,
/// and the outcome line is written here as a `notice`-shaped log entry so the
/// conversation pane shows it exactly like the terminal REPL does.
fn spawn_compact(agent_slot: &AgentSlot) {
    let slot = agent_slot.clone();
    std::thread::spawn(move || {
        let taken = slot.lock().unwrap().take();
        let Some(mut agent) = taken else { return };
        let out = agent.compact();
        // Persist the outcome next to the summary so the pane reports it even
        // though this thread cannot touch the widgets.
        agent.log_notice(&out);
        *slot.lock().unwrap() = Some(agent);
    });
}

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
        // The link map is parallel to `conv`: drop the same leading lines or
        // clicks resolve against the wrong line's URLs.
        state.line_links = state.line_links.split_off(drop);
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
            // Assistant markdown rendering depends on the pane:
            //
            // * `gui-rich-text` ON (default): keep the RAW markdown in the
            //   model. `sync_textview` converts each assistant line with
            //   `md::render_pango` and inserts Pango markup — bold/italic/
            //   code/headings/links render styled. ANSI would show up as
            //   literal escape junk, so no ANSI renderer runs here.
            // * OFF (`--no-default-features`): same renderer the TUI uses
            //   (`tui.rs:1697` calls `crate::md::render`). The GTK pane has
            //   no per-range markup in this mode (see `sync_textview`), so
            //   `color: false` — ANSI codes would show up as literal junk.
            let text = if kind == ConvKind::Assistant {
                #[cfg(feature = "gui-rich-text")]
                {
                    text
                }
                #[cfg(not(feature = "gui-rich-text"))]
                {
                    crate::md::render(&text, false)
                }
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
    tv.set_editable(false);
    // Selectable but never an input target: the pane must accept focus so the
    // user can select text and copy it (a `can_focus(false)` view cannot be
    // focused, so it can never hold a selection and Ctrl-C has nothing to
    // copy). Read-only (`editable(false)`) already blocks typing into it, so
    // focus here cannot eat keystrokes — and focus normally stays in the
    // prompt Entry anyway (tab changes grab it explicitly; nothing else moves
    // it). Without this the log is copy-hostile: no selection, no clipboard.
    #[cfg(all(target_os = "linux", not(windows)))]
    tv.set_can_focus(true);
}

/// Open a link URL in the user's browser, safely (design row G6).
///
/// Defence in depth: the span map only holds URLs that passed `sanitize_url`
/// at inventory time, but the click path re-checks — and narrows to
/// `http`/`https` only. `sanitize_url` also allows `mailto:`/`file:`, but
/// those never reach a browser here: `file:` would turn model output into a
/// local-file read primitive, and `mailto:` has no browser target. Anything
/// else is ignored silently (a click that does nothing beats a click that
/// runs something unexpected).
///
/// No shell: the URL is passed as a single argv element to `xdg-open`, so a
/// `"` or `$()` in the URL cannot inject. Detached via `setsid` (not
/// `process_group(0)` — see the project preference) so the browser never
/// holds pir's terminal, and reaped on a worker thread so the click never
/// blocks the GTK loop and never leaves a zombie.
#[cfg(all(target_os = "linux", not(windows)))]
fn open_link_safely(url: &str) {
    let Some(clean) = streamdown_ansi::sanitize::sanitize_url(url) else {
        return;
    };
    let lower = clean.to_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return;
    }
    std::thread::spawn(move || {
        let mut cmd = std::process::Command::new("xdg-open");
        cmd.arg(&clean);
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                // SAFETY: `setsid` is async-signal-safe; nothing else runs in
                // the child before exec.
                let _ = cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }
        let _ = cmd.status();
    });
}

/// Wire link clicks on the conversation pane (design row G6): a left-button
/// release over a recorded link span opens it via [`open_link_safely`].
///
/// Three guards, in order, so a click can never do the wrong thing:
/// 1. left button only (a right-click release is a context gesture, and on
///    GTK4 the button symbol is missing so the release is treated as left —
///    matching GTK3 delivery, which only fires for pressed buttons);
/// 2. no selection — a release that ends a text selection is a copy gesture,
///    never a link activation (the operator selecting link text must not
///    launch a browser);
/// 3. the release must land inside a span in the session's `buf_links` map
///    (built by `sync_textview` against the exact inserted text) — anywhere
///    else propagates (returns 0) so selection/drag still work.
///
/// Missing coordinate symbols degrade to non-clickable links: `link_hit_at`
/// returns `None` and the handler propagates every release.
#[cfg(all(target_os = "linux", not(windows)))]
fn connect_link_clicks(tv: &TextView, state: &Arc<Mutex<GuiState>>) {
    let tv_click = tv.clone();
    let state_click = state.clone();
    let _ = tv.connect_button_release(move |event: *mut c_void| -> i32 {
        let Some(loader) = rustxwidgets::backends::gtk::loader() else {
            return 0;
        };
        // Guard 1: left button only.
        if let Some(button) = TextView::release_button(event, &loader) {
            if button != 1 {
                return 0;
            }
        }
        // Guard 2: a release ending a selection is a copy gesture.
        if tv_click.buffer_has_selection() {
            return 0;
        }
        // Event → widget coords → buffer (line, col).
        // SAFETY: `event` is the GDK event pointer GTK passes to a
        // `button-release-event` handler — valid for the call's duration.
        let Some((wx, wy)) = (unsafe {
            rustxwidgets::gtk_dynamic_loader::gdk_event_get_coords(&loader, event)
        }) else {
            return 0;
        };
        let Some((line, col)) = tv_click.link_hit_at(wx as i32, wy as i32) else {
            return 0;
        };
        // Guard 3: the position must be inside a recorded span.
        let url = {
            let s = state_click.lock().unwrap();
            match s.buf_links.get(line as usize) {
                Some(spans) => lookup_link(spans, col as u32).map(str::to_string),
                None => None,
            }
        };
        match url {
            Some(u) => {
                open_link_safely(&u);
                1
            }
            None => 0,
        }
    });
}

/// Open a link URL in the user's default browser — the Windows counterpart of
/// the Linux [`open_link_safely`] above, and the `wxLaunchDefaultBrowser`
/// behaviour `wxHyperlinkCtrl` and `wxTE_AUTO_URL` both rely on.
///
/// Same policy, same place: the span map only holds URLs that passed
/// `sanitize_url` at inventory time, but the click path re-checks and narrows
/// to `http`/`https`. `file:` is refused here too — it would turn model output
/// into a local-file read primitive. Anything else is ignored silently.
///
/// No shell: the URL goes to `ShellExecuteW(…, "open", url, …)` as a single
/// argument, so a `"` or `&` in the URL cannot inject a command (unlike
/// `cmd /c start`). `ShellExecuteW` hands the URL to the registered handler and
/// returns immediately, so no worker thread is needed to keep the UI live.
#[cfg(windows)]
fn open_link_safely(url: &str) {
    let Some(clean) = streamdown_ansi::sanitize::sanitize_url(url) else {
        return;
    };
    let lower = clean.to_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return;
    }
    let opened = rustxwidgets::backends_nwg_adapter::TextView::open_url(&clean);
    if !opened {
        eprintln!("pir: could not open {clean} in the default browser");
    }
}

/// Wire link clicks on the conversation pane (design row G6, NWG backend).
///
/// The `wxTE_AUTO_URL` behaviour for a plain EDIT control: observe
/// `WM_LBUTTONUP` (never swallowing it, so caret moves, selection drags and
/// double-click word-select still work), hit-test the click with
/// `EM_CHARFROMPOS`, and open the URL whose span covers that buffer position.
///
/// Guards, in the same order as the GTK side so the two backends cannot
/// disagree:
/// 1. left button only — inherent: the raw handler only sees `WM_LBUTTONUP`;
/// 2. no selection — a release that ends a text selection is a copy gesture;
/// 3. the position must fall inside a span in this session's `buf_links`
///    (built by the Windows `sync_textview` against the exact inserted text).
///
/// Unlike GTK there is nothing to "return 1" for: NWG's raw handler returns
/// `None` so `DefSubclassProc` always runs.
#[cfg(windows)]
fn connect_link_clicks(tv: &TextView, state: &Arc<Mutex<GuiState>>) {
    let tv_click = tv.clone();
    let state_click = state.clone();
    tv.connect_left_release(Box::new(move |x: i32, y: i32| {
        // Guard 2: a release ending a selection is a copy gesture.
        if tv_click.buffer_has_selection() {
            return;
        }
        // Client point → buffer (line, char column).
        let Some((line, col)) = tv_click.char_index_at(x, y) else {
            return;
        };
        // Guard 3: the position must be inside a recorded span.
        let url = {
            let s = state_click.lock().unwrap();
            match s.buf_links.get(line as usize) {
                Some(spans) => lookup_link(spans, col as u32).map(str::to_string),
                None => None,
            }
        };
        if let Some(u) = url {
            open_link_safely(&u);
        }
    }));
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
    scrolled_window_range(sw).map(|(vpos, _, _)| vpos)
}

/// Read `(vpos, vmax, page)` for a `ScrolledWindow`: the current vertical
/// position, the adjustment's upper bound (the scrollable extent), and the
/// viewport height (the adjustment's page size).
///
/// `vmax` and `page` together tell "at the bottom" from "scrolled away from
/// the bottom": the bottom of what is shown is `vpos + page`, so the pane is
/// at the bottom exactly when that reaches `vmax`. All three come from the
/// adjustment GTK gives the scrolled window.
#[cfg(all(target_os = "linux", not(windows)))]
fn scrolled_window_range(sw: &ScrolledWindow) -> Option<(f64, f64, f64)> {
    // All three readings come from GTK through the toolkit's loader wrappers.
    // `get_vadjustment_page_size` is what makes "at the bottom" computable at
    // all: `upper` alone is the end of the extent, so comparing `value`
    // against `upper` directly mistakes the top of the viewport for the
    // bottom and freezes follow-the-tail after the first screenful.
    // `None` means the toolkit could not report (no display / symbol missing),
    // and the caller leaves `follow` alone rather than guessing.
    Some((
        sw.get_vadjustment_value(),
        sw.get_vadjustment_upper(),
        sw.get_vadjustment_page_size(),
    ))
}

/// Windows: no GTK loader, so no scroll position to read. Returning `None`
/// keeps the caller compiling and simply disables scroll-follow there.
#[cfg(windows)]
fn scrolled_window_vpos(_sw: &ScrolledWindow) -> Option<f64> {
    None
}

/// Windows counterpart of `scrolled_window_range`.
#[cfg(windows)]
fn scrolled_window_range(_sw: &ScrolledWindow) -> Option<(f64, f64, f64)> {
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
/// Two modes, selected by [`GuiState::sync_follow`]:
///
/// * live (`follow` armed): the buffer is append-only between rebuilds. Each
///   new line is appended (`append_text` on GTK) rather than replacing the
///   whole buffer. The old code called `set_text("")` at the end of the loop,
///   which on GTK goes to `gtk_text_buffer_set_text` — a *replace* — so the
///   pane ended up blank.
/// * frozen (`follow` off): the buffer is held. New lines wait in the model
///   (`rendered` is just a cursor, so the live path catches up on re-arm).
///   GTK pins the pane to the bottom on every append — without the hold, an
///   append would yank a mid-read user back down — so while frozen nothing is
///   appended at all. The one exception is an explicit keyboard/menu scroll
///   command (`view_slice` set to [`SCROLL_DIRTY`]): the windowed slice
///   `scroll.window(total)` is painted instead, which is the only programmatic
///   scroll the toolkit allows (`gtk_adjustment_set_value` does not exist).
///
/// `sync_textview` takes `&mut GuiState` because the per-session write cursor
/// lives in the state; it must not hold the state's lock while it runs.
#[cfg(all(target_os = "linux", not(windows)))]
fn sync_textview(widgets: &GuiWidgets, state: &mut GuiState) {
    // Rich vs plain assistant lines: with `gui-rich-text` the model holds RAW
    // markdown (see `drain_session_text`) and each assistant *block* — the run
    // of consecutive assistant lines between blank lines, see
    // [`continues_block`] — is converted with `md::render_pango` into Pango
    // markup, inserted via `append_markup` (`gtk_text_buffer_insert_markup`).
    // Rendering the block as one unit is what makes tables/code fences/
    // blockquotes/paragraphs render at all: a markdown construct spans lines,
    // so a per-line render split every one of them (the `/markup_demo` table
    // came out as four literal `| … |` rows).
    //
    // Without the feature the model already holds ANSI-stripped plain text and
    // lines go in via `append_text`. Non-assistant lines are always plain
    // `prefix + text`.
    if state.sync_follow() {
        // Live: any windowed buffer is stale — rebuild the full tail first.
        if state.needs_rebuild || state.view_slice.is_some() {
            widgets.textview.set_text("");
            state.rendered = 0;
            state.needs_rebuild = false;
            state.view_slice = None;
            // The buffer was replaced, so every span in the old map points at
            // text that no longer exists: rebuild it from scratch below.
            state.buf_links.clear();
        }
        // Assistant blocks are appended whole, so a block that is still
        // arriving (streamed into the model one drained line at a time) must
        // not be painted in pieces. The trailing (still-growing) block is
        // painted WHOLE from its start on every tick, so a big table or code
        // fence is visible while it streams instead of appearing all at once
        // at the end.
        //
        // The GTK text buffer has no "delete the last N lines" primitive, so a
        // repaint of an already-painted trailing block rebuilds the tail: the
        // cursor is rewound to the block's start and the buffer is cleared
        // first. `repaint_from` records the model index the buffer's current
        // contents start at (0 = whole conversation), which is what the
        // rebuild clears from.
        let mut end = {
            let (base, rest) = (state.rendered, &state.conv[state.rendered..]);
            base + live_boundary(rest, 0)
        };
        // A trailing block already in the buffer must be REPAINTED, not
        // appended again. The text buffer cannot delete lines, so clear it and
        // repaint from that block's start. Only when the model actually grew
        // since the last paint — otherwise every 150ms tick would rebuild.
        if let Some(from) = state.trailing_from
            && state.conv.len() > state.painted_len
        {
            widgets.textview.set_text("");
            state.buf_links.clear();
            state.rendered = from;
            end = from + live_boundary(&state.conv[from..], 0);
        }
        // One-past-the-last model line the buffer shows; the growth signal
        // above compares against it.
        state.painted_len = state.conv.len();
        let mut i = state.rendered;
        while i < end {
            let line = &state.conv[i];
            // Grow the block while the lines continue it. The block's Pango
            // already carries its own breaks, so it is inserted as one unit.
            let mut j = i + 1;
            while j < end && continues_block(Some(&state.conv[j - 1]), &state.conv[j]) {
                j += 1;
            }
            #[cfg(feature = "gui-rich-text")]
            if line.kind == ConvKind::Assistant {
                let markup = assistant_block_markup(line.kind, &state.conv[i..j]);
                let inserted = format!("{markup}\n");
                widgets.textview.append_markup(&inserted);
                // Clickable links (design row G6): resolve this block's
                // recorded pairs against the exact visible text just
                // inserted — markup shifts bytes, so spans computed on the
                // markdown source would point at the wrong characters.
                let pairs: Vec<(String, String)> = (i..j)
                    .flat_map(|k| state.line_links[k].iter().cloned())
                    .collect();
                let visible = crate::md::pango_visible_text(&inserted);
                let base = state.buf_links.len() as u32;
                let nlines = visible.matches('\n').count();
                state.buf_links.resize(base as usize + nlines, Vec::new());
                for (rel, scol, ecol, url) in resolve_link_spans(&visible, &pairs) {
                    state.buf_links[(base + rel) as usize].push((scol, ecol, url));
                }
                i = j;
                continue;
            }
            #[cfg(not(feature = "gui-rich-text"))]
            debug_assert_eq!(j, i + 1, "plain mode is per line");
            // Plain path (or non-assistant lines in rich mode): the
            // backend has no per-range markup, so the line goes in as
            // plain text; the conversation pane is monochrome. Colour lives in
            // the per-line prefix glyph and in the status label.
            let (_, prefix) = line.kind.style();
            let inserted = format!("{prefix}{}\n", line.text);
            widgets.textview.append_text(&inserted);
            // Same link-span bookkeeping as the rich path above, against the
            // exact inserted line (prefix included — it shifts every column).
            let spans = resolve_link_spans(&inserted, &state.line_links[i])
                .into_iter()
                .map(|(_, scol, ecol, url)| (scol, ecol, url))
                .collect();
            state.buf_links.push(spans);
            i += 1;
        }
        // Remember the trailing block's start when the paint stopped mid-block,
        // so the next sync repaints it whole instead of appending a copy.
        state.trailing_from = if end > 0 && end < state.conv.len() {
            let mut start = end;
            while start > 0 && continues_block(Some(&state.conv[start - 1]), &state.conv[start]) {
                start -= 1;
            }
            (start < end).then_some(start)
        } else {
            None
        };
        state.rendered = end;
        // Track the tail while live, so the first PageUp starts one full page
        // above the bottom rather than from a stale offset.
        state.scroll.end(state.conv.len());
    } else if let Some(shown) = state.view_slice {
        // Frozen with a keyboard/menu scroll request: paint the windowed slice.
        // Untouched otherwise — the buffer already shows the transcript where
        // the user left it, and repainting on every 150ms tick would yank it.
        let total = state.conv.len();
        let win = state.scroll.window(total);
        if state.needs_rebuild || shown != win {
            // Frozen window: plain text through `set_text` (which is
            // `gtk_text_buffer_set_text` — no markup parsing), and the same
            // block grouping as the live branch, so a table or a list that
            // straddles the window edge is not chopped into a row at a time.
            // The window edge is widened to block boundaries for the same
            // reason.
            let (start, end) = window_at_block_edges(&state.conv, win);
            let mut buf = String::new();
            // Buffer-line spans for the windowed slice, rebuilt exactly like
            // the live branch: resolve each inserted line's recorded pairs
            // against the exact string the buffer receives. The frozen slice
            // is plain text (see below), so spans resolve against
            // `prefix + body` — the same rule as the live plain path.
            let mut frozen_links: Vec<Vec<(u32, u32, String)>> = Vec::new();
            let mut i = start;
            while i < end {
                let line = &state.conv[i];
                let mut j = i + 1;
                while j < end && continues_block(Some(&state.conv[j - 1]), &state.conv[j]) {
                    j += 1;
                }
                let (_, prefix) = line.kind.style();
                buf.push_str(prefix);
                for (n, l) in state.conv[i..j].iter().enumerate() {
                    if n > 0 {
                        buf.push('\n');
                        buf.push_str(prefix);
                    }
                    // In rich mode the model holds raw markdown; strip the most
                    // common markers so the frozen slice does not show `**`.
                    // (Full Pango lives in the live branch only.)
                    #[cfg(feature = "gui-rich-text")]
                    let body = strip_markdown_markers(&l.text);
                    #[cfg(not(feature = "gui-rich-text"))]
                    let body = l.text.clone();
                    buf.push_str(&body);
                    // Resolve this buffer line's spans against the exact
                    // inserted text (`prefix + body`); non-single-line
                    // resolutions are dropped by construction (one entry per
                    // buffer line here).
                    let inserted = format!("{prefix}{body}");
                    let spans = resolve_link_spans(&inserted, &state.line_links[i + n])
                        .into_iter()
                        .map(|(_, scol, ecol, url)| (scol, ecol, url))
                        .collect();
                    frozen_links.push(spans);
                }
                buf.push('\n');
                i = j;
            }
            // The loop leaves a trailing newline, so the buffer holds one
            // final empty line past the last entry — record it (empty) so the
            // map stays parallel to the buffer by construction.
            frozen_links.push(Vec::new());
            widgets.textview.set_text(&buf);
            state.buf_links = frozen_links;
            state.needs_rebuild = false;
            state.view_slice = Some(win);
            // `rendered` is now meaningless (the buffer was replaced, not
            // appended to); the live branch rebuilds from scratch on re-arm
            // because `view_slice` is `Some`.
        }
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
    let hint = hint_text(&state.draft, state.tab_note.as_deref());
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
        // The buffer was replaced, so every span in the old map points at
        // text that no longer exists: rebuild it from scratch below.
        state.buf_links.clear();
    }
    if state.conv.len() > state.rendered {
        for (i, line) in state.conv[state.rendered..].iter().enumerate() {
            let markup = line_markup(line);
            // The nwg backend's `TextView::set_markup` already appends the
            // (markup-stripped) line plus a newline, so no separate newline.
            widgets.textview.set_markup(&markup);
            // Clickable links (design row G6, NWG): resolve this line's
            // recorded pairs against exactly the string `set_markup` inserts —
            // the markup-stripped text — so the span columns index the buffer
            // the EDIT control actually holds. `plain_line` is the same
            // stripper `set_markup` uses, so the two cannot drift apart.
            let inserted = rustxwidgets::backends_nwg_adapter::TextView::plain_line(&markup);
            state.buf_links.push(
                resolve_link_spans(&inserted, &state.line_links[state.rendered + i])
                    .into_iter()
                    .map(|(_, scol, ecol, url)| (scol, ecol, url))
                    .collect(),
            );
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
    let hint = hint_text(&state.draft, state.tab_note.as_deref());
    if state.last_hint != hint {
        if hint.is_empty() {
            widgets.hint.set_markup("");
        } else {
            widgets.hint.set_markup(&status_markup(&hint));
        }
        state.last_hint = hint;
    }
}


/// One row of the GUI's main menu: the subcommand `/menu <sub>` runs, and the
/// label the menu dialog shows.
///
/// This is the GUI's equivalent of the terminal's `modal::MenuAction` list
/// (`/menu` in the streaming REPL): Resume, Backgrounded sessions, Model,
/// Thinking, Security, Settings, Worktrees default, Autoclean coverage, Help,
/// About, Quit. The terminal menu is an alternate-screen picker; here each row
/// becomes a button in a GTK dialog (`open_menu_dialog`), and each also has a
/// topbar item (`MENU_ACTIONS` entries with `command: "menu"`) routing through
/// the same `/menu <sub>` line a typed command takes — one implementation per
/// action, three ways to reach it.
const MENU_DIALOG_ACTIONS: &[(&str, &str)] = &[
    ("resume", "Resume / pick session"),
    ("backgrounded", "Backgrounded sessions"),
    ("model", "Model"),
    ("thinking", "Thinking"),
    ("security", "Security"),
    ("settings", "Settings"),
    ("worktrees", "Worktrees by default"),
    ("coverage", "Autoclean branch coverage"),
    ("help", "Help"),
    ("about", "About"),
    ("quit", "Quit"),
];

/// Label for a `/menu <sub>` subcommand, or `None` for an unknown one. Pure so
/// the dialog, the dispatch, and the parity tests share one table instead of
/// three copies that can drift apart.
fn menu_action_label(sub: &str) -> Option<&'static str> {
    MENU_DIALOG_ACTIONS
        .iter()
        .find(|(s, _)| *s == sub)
        .map(|(_, label)| *label)
}

/// Strip ANSI SGR escapes from a string. The pane has no ANSI renderer —
/// `sync_textview` inserts plain text, so escapes would show up as literal
/// junk (see the `gui-rich-text` notes in `drain_session_text`). Agent setters
/// return plain strings, but a few reused reports (`SessionResume::banner`)
/// are built with `term::` paints, which emit escapes whenever stdout is a tty
/// — which it is under `pir --gui` launched from a terminal. Pure so the rule
/// is testable without a display.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            // Consume the SGR parameter bytes and the final command letter.
            for c2 in chars.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Render a branch-coverage plan as pane lines (no ANSI: see [`strip_ansi`]).
/// Mirrors the terminal's `report_coverage_plan` (`main.rs`) line for line in
/// content, minus the colour — the same tools, the same missing-tool install
/// hints, the same "measuring with" summary.
fn coverage_plan_lines(st: &crate::lints::CoverageStatus) -> Vec<String> {
    let mut out = Vec::new();
    for l in &st.available {
        let chosen = l
            .chosen
            .iter()
            .map(|t| t.tool)
            .collect::<Vec<_>>()
            .join(" + ");
        let extra = if l.extras.iter().any(|t| t.needs_install()) {
            format!(
                "  (also applicable, not installed: {})",
                l.extras
                    .iter()
                    .filter(|t| t.needs_install())
                    .map(|t| t.tool)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            String::new()
        };
        out.push(format!(
            "· autoclean coverage: {} — measuring with {}{}",
            l.lang, chosen, extra
        ));
    }
    for l in &st.missing {
        let opts = l
            .candidates
            .iter()
            .filter(|t| t.needs_install())
            .map(|t| format!("{} ({})", t.tool, t.install))
            .collect::<Vec<_>>()
            .join(", or ");
        out.push(format!(
            "! autoclean coverage: {} has NO coverage tool installed — its branches will NOT be measured. Install one: {}",
            l.lang,
            if opts.is_empty() {
                l.candidates
                    .iter()
                    .map(|t| t.tool)
                    .collect::<Vec<_>>()
                    .join(" / ")
            } else {
                opts
            },
        ));
    }
    out
}

/// The concrete `Dialog` type returned by `App::create_dialog` — same pattern
/// as the `Gui*` aliases above. Both backends expose `set_title`,
/// `append_content_area`, `present` and `close`, so menu dialogs stay
/// cross-platform by construction (unlike `question_dialog`, which is
/// Linux-gated for `set_xalign`).
#[cfg(windows)]
type GuiDialog = rustxwidgets::backends_nwg_adapter::Dialog;
#[cfg(all(target_os = "linux", not(windows)))]
type GuiDialog = rustxwidgets::backends_gtk_adapter::Dialog;

/// Push one label+button row into a menu-style dialog. The button runs `act`,
/// then closes the dialog. Every menu dialog in this file is built this way —
/// fire-and-forget from the GTK main thread.
///
/// Blocking (like `question_dialog`'s `rx.recv()`) would freeze the UI here:
/// that renderer runs on an agent *worker* thread, but menu actions run on the
/// GTK main thread itself, so there is nothing else keeping the loop alive.
/// Buttons therefore act synchronously and close; every action below is fast
/// (a flip, a switch, a save, a local file read) except goal-driving, which
/// goes through `spawn_goal`'s worker like `/goal` and `/continue` do.
/// Push one label+button row into a menu-style dialog. The button runs `act`,
/// then closes the dialog. Every menu dialog in this file is built this way —
/// fire-and-forget from the GTK main thread.
///
/// Blocking (like `question_dialog`'s `rx.recv()`) would freeze the UI here:
/// that renderer runs on an agent *worker* thread, but menu actions run on the
/// GTK main thread itself, so there is nothing else keeping the loop alive.
/// Buttons therefore act synchronously and close; every action below is fast
/// (a flip, a switch, a save, a local file read) except goal-driving, which
/// goes through `spawn_goal`'s worker like `/goal` and `/continue` do.
fn menu_dialog_button(
    app: &App,
    dialog: &GuiDialog,
    label: &str,
    mut act: impl FnMut() + 'static,
) {
    let Ok(btn) = app.create_button(label) else {
        return;
    };
    let dialog_cb = dialog.clone();
    let _ = btn.on_click(move || {
        act();
        dialog_cb.close();
    });
    dialog.append_content_area(&btn);
}

/// [`menu_dialog_button`] with an explicit container: the button is put in
/// `container` (a vertical box inside the dialog, so rows stack instead of
/// lining up side by side across the dialog's content area) and still closes
/// `dialog` when clicked.
fn menu_box_button(
    app: &App,
    dialog: &GuiDialog,
    container: &GuiBox,
    label: &str,
    mut act: impl FnMut() + 'static,
) {
    let Ok(btn) = app.create_button(label) else {
        return;
    };
    let dialog_cb = dialog.clone();
    let _ = btn.on_click(move || {
        act();
        dialog_cb.close();
    });
    container.append(&btn);
}

/// Resume a session log into the current agent, pane-native.
///
/// Mirrors `resume_into` (`main.rs`, shared by `/resume` and the terminal
/// `/menu` Resume action) but writes to the pane instead of stdout, seeds
/// Arrow-Up history like a resumed terminal session, and drives an attached
/// goal through `spawn_goal`'s worker instead of blocking `continue_goal`
/// inline — blocking the GTK main thread for a goal run would freeze the
/// window exactly like the old inline `/goal` did.
fn resume_session_in_gui(
    agent_slot: &AgentSlot,
    path: &std::path::Path,
    state: &Arc<Mutex<GuiState>>,
    widgets: &GuiWidgets,
    hist_nav: &Arc<Mutex<HistoryNav>>,
) {
    let (lines, prompts, has_goal, show_thinking) = {
        let mut g = agent_slot.lock().unwrap();
        let Some(agent) = g.as_mut() else {
            let mut s = state.lock().unwrap();
            s.push(ConvKind::System, "· agent busy (turn running) — cancel it first");
            sync_textview(widgets, &mut s);
            return;
        };
        agent.clear();
        let resumed = agent.load_session(&path.to_path_buf());
        let mut lines: Vec<(ConvKind, String)> = Vec::new();
        if resumed.turns > 0 {
            lines.push((ConvKind::System, strip_ansi(&resumed.banner(path))));
        } else if !resumed.summary.is_empty() {
            lines.push((ConvKind::System, strip_ansi(&resumed.summary)));
        }
        agent.apply_persisted_model();
        if let Some(notice) = agent.apply_persisted_su_security() {
            lines.push((ConvKind::System, format!("⚠ {notice}")));
        }
        if let Some(notice) = agent.apply_persisted_plan_mode() {
            lines.push((ConvKind::System, format!("⚠ {notice}")));
        }
        agent.apply_persisted_thinking();
        let show_thinking = agent.show_thinking();
        let has_goal = agent.goal_snapshot().is_some();
        if has_goal {
            agent.attach_goal(path);
        }
        (lines, resumed.prompts, has_goal, show_thinking)
    };
    {
        let mut s = state.lock().unwrap();
        for (kind, line) in &lines {
            s.push(*kind, line);
        }
        for p in &prompts {
            term::push_history(p);
            s.history.push(p.clone());
        }
        hist_nav.lock().unwrap().reset();
        s.show_thinking = show_thinking;
        if has_goal {
            s.push(
                ConvKind::System,
                "· goal attached — driving it now (/cancel to stop)",
            );
        }
        sync_textview(widgets, &mut s);
    }
    if has_goal {
        spawn_goal(agent_slot, false);
    }
}

/// Open the resume picker: one button per unfinished session (newest first),
/// resuming into the current agent. `background_only` keeps just the `-bg`
/// background-job logs — the terminal's "Backgrounded sessions" item.
fn open_resume_dialog(
    app: &App,
    agent_slot: &AgentSlot,
    state: &Arc<Mutex<GuiState>>,
    widgets: &GuiWidgets,
    hist_nav: &Arc<Mutex<HistoryNav>>,
    background_only: bool,
) {
    let mut entries: Vec<crate::session::UnfinishedEntry> = crate::session::scan_unfinished();
    if background_only {
        entries.retain(|e| e.name.contains("-bg"));
    }
    if entries.is_empty() {
        let mut s = state.lock().unwrap();
        s.push(
            ConvKind::System,
            if background_only {
                "· no background sessions to resume"
            } else {
                "· no unfinished sessions — nothing crashed or left a goal in progress"
            },
        );
        sync_textview(widgets, &mut s);
        return;
    }
    let Ok(dialog) = app.create_dialog() else {
        let mut s = state.lock().unwrap();
        s.push(
            ConvKind::Error,
            "· could not open the resume dialog — resume with /resume <index|path-fragment> (see /unfinished)",
        );
        sync_textview(widgets, &mut s);
        return;
    };
    dialog.set_title(if background_only {
        "pir — backgrounded sessions"
    } else {
        "pir — resume session"
    });
    // Cap the list: a picker with hundreds of rows is unusable, and the
    // newest-first order means the cap drops the stalest entries.
    for e in entries.into_iter().take(30) {
        let label = format!(
            "[{}] {}  {}",
            e.reason,
            e.name,
            e.preview.chars().take(60).collect::<String>()
        );
        let path = e.path.clone();
        let slot = agent_slot.clone();
        let st = state.clone();
        let w = widgets.clone();
        let hn = hist_nav.clone();
        menu_dialog_button(app, &dialog, &label, move || {
            resume_session_in_gui(&slot, &path, &st, &w, &hn);
        });
    }
    {
        let dialog_cancel = dialog.clone();
        menu_dialog_button(app, &dialog, "Cancel", move || {
            dialog_cancel.close();
        });
    }
    dialog.present();
}

/// Switch the agent to one catalog model, pane-native: the single path the
/// prompt-row picker, the model dialog, and direct `/menu model <sel>` share,
/// so the three cannot disagree about what a switch does or reports.
fn switch_model_in_gui(
    agent_slot: &AgentSlot,
    providers: &[Provider],
    item: &str,
    state: &Arc<Mutex<GuiState>>,
    widgets: &GuiWidgets,
) {
    match crate::config::select(providers, item) {
        Ok((p, m)) => {
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                let mut s = state.lock().unwrap();
                s.push(ConvKind::System, "· agent busy (turn running) — try again when idle");
                sync_textview(widgets, &mut s);
                return;
            };
            match agent.switch(p.clone(), m.clone()) {
                Ok(()) => {
                    crate::note_model_used(p, m);
                    let msg = format!("→ {}", agent.label());
                    drop(g);
                    let mut s = state.lock().unwrap();
                    s.push(ConvKind::System, &msg);
                    sync_textview(widgets, &mut s);
                }
                Err(e) => {
                    drop(g);
                    let mut s = state.lock().unwrap();
                    s.push(ConvKind::Error, &e);
                    sync_textview(widgets, &mut s);
                }
            }
        }
        Err(e) => {
            let mut s = state.lock().unwrap();
            s.push(ConvKind::Error, &e);
            sync_textview(widgets, &mut s);
        }
    }
}

/// Open the model picker: one button per catalog model, switching through the
/// same path the prompt-row picker and the typed `/model <sel>` use.
fn open_model_dialog(
    app: &App,
    agent_slot: &AgentSlot,
    providers: &[Provider],
    state: &Arc<Mutex<GuiState>>,
    widgets: &GuiWidgets,
) {
    let items = model_picker_items(providers);
    if items.is_empty() {
        let mut s = state.lock().unwrap();
        s.push(ConvKind::System, "· no models in the catalog");
        sync_textview(widgets, &mut s);
        return;
    }
    let Ok(dialog) = app.create_dialog() else {
        let mut s = state.lock().unwrap();
        s.push(
            ConvKind::Error,
            "· could not open the model picker — pick with /model <prefix>",
        );
        sync_textview(widgets, &mut s);
        return;
    };
    dialog.set_title("pir — model");
    // Recent models first, in the terminal's own order and numbering (`/modelN`
    // picks row N, so the N is printed on the button), then the rest of the
    // catalog. A recent label that no longer resolves against this catalog is
    // dropped rather than offered as a button that switches to nothing.
    let current = {
        let g = agent_slot.lock().unwrap();
        g.as_ref().map(|a| a.label()).unwrap_or_default()
    };
    let recent: Vec<String> = crate::model_history::top_matching("", crate::model_history::TOP_N)
        .into_iter()
        .filter(|l| crate::config::select(providers, l).is_ok())
        .collect();
    for (i, item) in recent.iter().enumerate() {
        let slot = agent_slot.clone();
        let provs = providers.to_vec();
        let st = state.clone();
        let w = widgets.clone();
        let label = item.clone();
        let shown = if *item == current {
            format!("{}  ● {item} (current)", i + 1)
        } else {
            format!("{}  {item}", i + 1)
        };
        menu_dialog_button(app, &dialog, &shown, move || {
            switch_model_in_gui(&slot, &provs, &label, &st, &w);
        });
    }
    if !recent.is_empty()
        && let Ok(sep) = app.create_label("all models:")
    {
        dialog.append_content_area(&sep);
    }
    for item in items {
        if recent.contains(&item) {
            continue; // already above, with its `/modelN` number
        }
        let slot = agent_slot.clone();
        let provs = providers.to_vec();
        let st = state.clone();
        let w = widgets.clone();
        let label = item.clone();
        let shown = if item == current {
            format!("● {item} (current)")
        } else {
            item.clone()
        };
        menu_dialog_button(app, &dialog, &shown, move || {
            switch_model_in_gui(&slot, &provs, &label, &st, &w);
        });
    }
    {
        let dialog_cancel = dialog.clone();
        menu_dialog_button(app, &dialog, "Cancel", move || {
            dialog_cancel.close();
        });
    }
    dialog.present();
}

/// Open the thinking picker: one button per offered level (the same
/// `offered_thinking_levels` filter the terminal picker uses, so an offered
/// level is never silently ignored), plus show/hide toggles.
fn open_thinking_dialog(
    app: &App,
    agent_slot: &AgentSlot,
    state: &Arc<Mutex<GuiState>>,
    widgets: &GuiWidgets,
) {
    let snapshot = {
        let g = agent_slot.lock().unwrap();
        let Some(agent) = g.as_ref() else {
            let mut s = state.lock().unwrap();
            s.push(ConvKind::System, "· agent busy (turn running) — try again when idle");
            sync_textview(widgets, &mut s);
            return;
        };
        let model = agent.model();
        let kind = agent.provider().model_api(&model);
        (
            crate::modal::offered_thinking_levels(kind, model.context.unwrap_or(0), Some(&model)),
            agent.thinking_level().as_str().to_string(),
        )
    };
    let Ok(dialog) = app.create_dialog() else {
        let mut s = state.lock().unwrap();
        s.push(
            ConvKind::Error,
            "· could not open the thinking picker — set it with /thinking <level>",
        );
        sync_textview(widgets, &mut s);
        return;
    };
    dialog.set_title("pir — thinking level");
    for level in snapshot.0 {
        let label = if level == snapshot.1 {
            format!("● {level} (current)")
        } else {
            format!("  {level}")
        };
        let slot = agent_slot.clone();
        let st = state.clone();
        let w = widgets.clone();
        menu_dialog_button(app, &dialog, &label, move || {
            if let Some(lvl) = crate::config::ThinkingLevel::parse(&level) {
                let msg = {
                    let mut g = slot.lock().unwrap();
                    match g.as_mut() {
                        Some(agent) => agent.set_thinking(lvl),
                        None => "· agent busy (turn running)".to_string(),
                    }
                };
                let mut s = st.lock().unwrap();
                s.push(ConvKind::System, &format!("· {msg}"));
                sync_textview(&w, &mut s);
            }
        });
    }
    for (label, show) in [("Show thinking", true), ("Hide thinking", false)] {
        let slot = agent_slot.clone();
        let st = state.clone();
        let w = widgets.clone();
        menu_dialog_button(app, &dialog, label, move || {
            let msg = {
                let mut g = slot.lock().unwrap();
                match g.as_mut() {
                    Some(agent) => {
                        let m = agent.set_show_thinking(show);
                        st.lock().unwrap().show_thinking = show;
                        m
                    }
                    None => "· agent busy (turn running)".to_string(),
                }
            };
            let mut s = st.lock().unwrap();
            s.push(ConvKind::System, &format!("· {msg}"));
            sync_textview(&w, &mut s);
        });
    }
    {
        let dialog_cancel = dialog.clone();
        menu_dialog_button(app, &dialog, "Cancel", move || {
            dialog_cancel.close();
        });
    }
    dialog.present();
}

/// Cycle one security-enum row to its next value, wrapping around. Pure over
/// plain strings so the cycle order is testable without an agent; the dialog
/// parses back through the same `parse` the typed commands accept.
fn cycle_security_value(current: &str, options: &[String]) -> String {
    let i = options.iter().position(|o| o == current).unwrap_or(0);
    options[(i + 1) % options.len()].clone()
}

/// Saved-policy rows the security editor renders, in dialog order:
/// (row label, current value, cycle options). Shared by the builder and the
/// tests so a row the dialog renders is a row the suite pins.
fn security_enum_rows(policy: &crate::security::SecurityPolicy) -> Vec<(String, String, Vec<String>)> {
    vec![
        ("level".to_string(), policy.level.as_str().to_string(), vec!["guard".to_string(), "off".to_string(), "sandbox".to_string(), "strict".to_string(), "worktree".to_string(), "mitigation".to_string(), "plan".to_string()]),
        ("apt".to_string(), policy.apt.as_str().to_string(), vec!["auto".to_string(), "human".to_string(), "stage".to_string(), "project".to_string()]),
        ("network".to_string(), policy.network.as_str().to_string(), vec!["on".to_string(), "allowlist".to_string(), "off".to_string()]),
        ("ask".to_string(), policy.ask.as_str().to_string(), vec!["ask".to_string(), "auto-yes".to_string(), "auto-no".to_string()]),
        ("read".to_string(), policy.read.as_str().to_string(), vec!["open".to_string(), "guarded-secrets".to_string()]),
    ]
}

/// Apply an edited security policy exactly like the terminal's Security
/// screen: save (session file, plus global defaults on demand), flip live
/// `user-security` with sidecar drop on change, push quarantine/level/apt/
/// network/ask/read into the live session. Returns the pane lines.
fn apply_gui_security_policy(
    agent_slot: &AgentSlot,
    policy: &crate::security::SecurityPolicy,
    editor_initial_su: bool,
    global: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    let saved = if global {
        let session_path = crate::config::pi_dir().join("agent").join("security.toml");
        let global_path = crate::security::global_defaults_file();
        let mut targets = vec![session_path];
        if global_path != targets[0] {
            targets.push(global_path);
        }
        let mut saved = Vec::new();
        let mut failed: Option<String> = None;
        for t in &targets {
            match crate::security::save_policy_to(policy, t) {
                Ok(p) => saved.push(p),
                Err(e) => {
                    failed = Some(format!("{e} [{}]", crate::security::save_diagnostic(t)));
                    break;
                }
            }
        }
        match failed {
            Some(e) => {
                out.push(format!("SECURITY SAVE FAILED ({e}) — toggles NOT persisted"));
                return out;
            }
            None => saved,
        }
    } else {
        match crate::security::save_policy(policy) {
            Ok(p) => vec![p],
            Err(e) => {
                let target = crate::config::pi_dir().join("agent").join("security.toml");
                out.push(format!(
                    "SECURITY SAVE FAILED ({e} [{}]) — toggles NOT persisted",
                    crate::security::save_diagnostic(&target)
                ));
                return out;
            }
        }
    };
    let list = saved
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if saved.len() > 1 {
        out.push(format!("security options saved to {list} (session file + global defaults for new projects)"));
    } else {
        out.push(format!("security options saved to {list} (new sessions of this project)"));
    }
    if policy.user_security != editor_initial_su {
        let mut g = agent_slot.lock().unwrap();
        if let Some(agent) = g.as_mut() {
            let reason = if policy.user_security {
                String::new()
            } else {
                "disabled via /menu security — agent authorized to act as the \
                 invoking user for this session"
                    .to_string()
            };
            out.push(agent.set_su_security(policy.user_security, &reason));
            agent.clear_persisted_su_security();
        }
    }
    {
        let mut g = agent_slot.lock().unwrap();
        if let Some(agent) = g.as_mut() {
            out.push(agent.apply_security_policy(policy));
        }
    }
    out
}

/// Open the security editor: enum rows cycle on click, bools are checkbuttons,
/// Save writes `security.toml` (+ global defaults on demand) and pushes the
/// result into the live session exactly like the terminal's Security screen.
fn open_security_dialog(
    app: &App,
    agent_slot: &AgentSlot,
    state: &Arc<Mutex<GuiState>>,
    widgets: &GuiWidgets,
) {
    let Ok(dialog) = app.create_dialog() else {
        let mut s = state.lock().unwrap();
        s.push(
            ConvKind::Error,
            "· could not open the security editor — edit security.toml directly",
        );
        sync_textview(widgets, &mut s);
        return;
    };
    dialog.set_title("pir — security options");
    let policy = crate::security::load_policy();
    let editor_initial_su = policy.user_security;
    // Enum rows: one button per row showing `label: value`; each click
    // advances to the next value and relabels. Pending values live on a shared
    // map the saver reads back, so there is exactly one store of in-progress
    // edits (a second store is how an editor silently saves stale rows).
    let pending: Rc<RefCell<std::collections::HashMap<String, String>>> =
        Rc::new(RefCell::new(
            security_enum_rows(&policy)
                .into_iter()
                .map(|(name, value, _)| (name, value))
                .collect(),
        ));
    for (name, current, options) in security_enum_rows(&policy) {
        let Ok(btn) = app.create_button(&format!("{name}: {current}")) else {
            continue;
        };
        // `Rc`, not a clone: the prelude `Button` is not `Clone`, and the
        // handler relabels the button it sits on.
        let btn = Rc::new(btn);
        let pending_row = pending.clone();
        let name_cb = name.clone();
        let mut value = current.clone();
        let opts = options.clone();
        let btn_cb = btn.clone();
        let _ = btn.on_click(move || {
            value = cycle_security_value(&value, &opts);
            btn_cb.set_text(&format!("{name_cb}: {value}"));
            pending_row.borrow_mut().insert(name_cb.clone(), value.clone());
        });
        dialog.append_content_area(&*btn);
    }
    // Bool rows: real checkbuttons, read at save time.
    let mut check_names: Vec<&'static str> = Vec::new();
    let mut check_widgets: Vec<GuiCheckButton> = Vec::new();
    for (name, on) in [
        ("quarantine", policy.quarantine),
        ("quarantine-project", policy.quarantine_project),
        ("user-security", policy.user_security),
    ] {
        let Ok(cb) = app.create_checkbutton(name) else {
            continue;
        };
        cb.set_active(on);
        dialog.append_content_area(&cb);
        check_names.push(name);
        check_widgets.push(cb);
    }
    // Save / Save global / Discard.
    for (label, global) in [("Save", false), ("Save as global defaults", true)] {
        let slot = agent_slot.clone();
        let st = state.clone();
        let w = widgets.clone();
        let pending_save = pending.clone();
        let names_save = check_names.clone();
        let widgets_save = check_widgets.clone();
        let dialog_save = dialog.clone();
        menu_dialog_button(app, &dialog, label, move || {
            let mut policy = crate::security::load_policy();
            // Re-read saved values into the edited policy, then overlay the
            // dialog's rows — same "mutate only on save" rule as the terminal
            // editor, so opening and discarding never touches disk.
            for (name, value) in pending_save.borrow().iter() {
                match name.as_str() {
                    "level" => {
                        if let Some(l) = crate::security::SecurityLevel::parse(value) {
                            policy.level = l;
                        }
                    }
                    "apt" => {
                        if let Some(a) = crate::security::AptMode::parse(value) {
                            policy.apt = a;
                        }
                    }
                    "network" => {
                        if let Some(n) = crate::security::NetworkMode::parse(value) {
                            policy.network = n;
                        }
                    }
                    "ask" => {
                        if let Some(a) = crate::security::AskMode::parse(value) {
                            policy.ask = a;
                        }
                    }
                    "read" => {
                        if let Some(r) = crate::security::ReadMode::parse(value) {
                            policy.read = r;
                        }
                    }
                    _ => {}
                }
            }
            for (i, name) in names_save.iter().enumerate() {
                if let Some(cb) = widgets_save.get(i) {
                    let on = cb.is_active();
                    match *name {
                        "quarantine" => policy.quarantine = on,
                        "quarantine-project" => policy.quarantine_project = on,
                        "user-security" => policy.user_security = on,
                        _ => {}
                    }
                }
            }
            let lines = apply_gui_security_policy(&slot, &policy, editor_initial_su, global);
            dialog_save.close();
            let mut s = st.lock().unwrap();
            for line in lines {
                s.push(ConvKind::System, &line);
            }
            sync_textview(&w, &mut s);
        });
    }
    {
        let dialog_cancel = dialog.clone();
        menu_dialog_button(app, &dialog, "Discard", move || {
            dialog_cancel.close();
        });
    }
    dialog.present();
}

/// Apply one settings step: cycle thinking through the offered levels, toggle
/// the four booleans. Pure over plain values so the step logic is testable
/// without an agent; the dialog pairs it with the agent setters.
fn step_gui_setting(
    thinking_levels: &[String],
    thinking: &mut String,
    show: &mut bool,
    incremental: &mut bool,
    stop_skill: &mut bool,
    full_auto: &mut bool,
    row: usize,
) {
    match row {
        0 => {
            if thinking_levels.is_empty() {
                return;
            }
            let i = thinking_levels
                .iter()
                .position(|l| l == thinking)
                .unwrap_or(0);
            *thinking = thinking_levels[(i + 1) % thinking_levels.len()].clone();
        }
        1 => *show = !*show,
        2 => *incremental = !*incremental,
        3 => *stop_skill = !*stop_skill,
        4 => *full_auto = !*full_auto,
        _ => {}
    }
}

/// Read the live settings snapshot the settings dialog edits: model label,
/// offered thinking levels, and every toggle from the agent (never from
/// launch-time locals — a resumed or policy-edited session shows what it is
/// actually doing, exactly like the terminal's Settings screen).
fn gui_settings_snapshot(
    agent_slot: &AgentSlot,
) -> Option<(String, Vec<String>, String, bool, bool, bool, bool)> {
    let g = agent_slot.lock().unwrap();
    let agent = g.as_ref()?;
    let model = agent.model();
    let kind = agent.provider().model_api(&model);
    let levels = crate::modal::offered_thinking_levels(
        kind,
        model.context.unwrap_or(0),
        Some(&model),
    );
    Some((
        agent.label(),
        levels,
        agent.thinking_level().as_str().to_string(),
        agent.show_thinking(),
        agent.incremental_md(),
        agent.stop_skill(),
        agent.full_auto(),
    ))
}

/// Apply an edited settings snapshot through the same agent setters the slash
/// commands use, so a value shown in the dialog and a value set by `/thinking`
/// cannot drift apart. Returns the pane lines. The full-auto change is
/// persisted to `security.toml` like the terminal does — best-effort, the
/// live change stands either way.
fn apply_gui_settings(
    agent_slot: &AgentSlot,
    before: &(String, Vec<String>, String, bool, bool, bool, bool),
    chosen: &(String, Vec<String>, String, bool, bool, bool, bool),
) -> Vec<String> {
    let mut out = Vec::new();
    let mut g = agent_slot.lock().unwrap();
    let Some(agent) = g.as_mut() else {
        out.push("· agent busy (turn running) — settings not applied".to_string());
        return out;
    };
    if chosen.2 != before.2
        && let Some(lvl) = crate::config::ThinkingLevel::parse(&chosen.2)
    {
        out.push(agent.set_thinking(lvl));
    }
    if chosen.3 != before.3 {
        agent.set_show_thinking(chosen.3);
        out.push(format!(
            "· show thinking {}",
            if chosen.3 { "on" } else { "off" }
        ));
    }
    if chosen.4 != before.4 {
        out.push(agent.set_incremental_md(chosen.4));
    }
    if chosen.5 != before.5 {
        out.push(agent.set_stop_skill(chosen.5));
    }
    if chosen.6 != before.6 {
        let msg = agent.set_full_auto(chosen.6);
        out.push(msg);
        if let Some(p) = agent.security_policy() {
            match crate::security::save_policy(&p) {
                Ok(path) => out.push(format!("ask-mode saved to {}", path.display())),
                Err(e) => out.push(format!(
                    "· could not save ask-mode to security.toml ({e}) — the change applies to this session only"
                )),
            }
        }
    }
    if out.is_empty() {
        out.push("· settings unchanged".to_string());
    }
    out
}

/// Open the settings editor: thinking cycles through the offered levels, four
/// checkbuttons cover show/incremental/stop-skill/full-auto, Save applies
/// through the same setters the slash commands use. Mirrors the terminal's
/// Settings screen row for row, minus the alternate-screen key handling.
fn open_settings_dialog(
    app: &App,
    agent_slot: &AgentSlot,
    state: &Arc<Mutex<GuiState>>,
    widgets: &GuiWidgets,
) {
    let Some(before) = gui_settings_snapshot(agent_slot) else {
        let mut s = state.lock().unwrap();
        s.push(ConvKind::System, "· agent busy (turn running) — try again when idle");
        sync_textview(widgets, &mut s);
        return;
    };
    let Ok(dialog) = app.create_dialog() else {
        let mut s = state.lock().unwrap();
        s.push(
            ConvKind::Error,
            "· could not open the settings editor — set values with /thinking and friends",
        );
        sync_textview(widgets, &mut s);
        return;
    };
    dialog.set_title("pir — settings");
    // Model line: orientation only, like the terminal screen.
    if let Ok(model_line) = app.create_label(&format!("model: {}", before.0)) {
        dialog.append_content_area(&model_line);
    }
    // Thinking row: a button cycling the offered levels (inert when the model
    // offers none — same rule as the terminal picker). The pending value lives
    // on one shared cell the button mutates and the saver reads.
    let pending_thinking: Rc<RefCell<String>> = Rc::new(RefCell::new(before.2.clone()));
    let thinking_levels = before.1.clone();
    if !thinking_levels.is_empty() {
        let cur = pending_thinking.clone();
        let dialog_levels = thinking_levels.clone();
        if let Ok(btn) = app.create_button(&format!("thinking: {}", cur.borrow().clone())) {
            let btn = Rc::new(btn);
            let btn_cb = btn.clone();
            let _ = btn.on_click(move || {
                let mut t = cur.borrow_mut();
                let i = dialog_levels.iter().position(|l| l == &*t).unwrap_or(0);
                *t = dialog_levels[(i + 1) % dialog_levels.len()].clone();
                btn_cb.set_text(&format!("thinking: {t}"));
            });
            dialog.append_content_area(&*btn);
        }
    }
    // Bool rows: real checkbuttons, read at save time.
    let mut check_widgets: Vec<GuiCheckButton> = Vec::new();
    for (label, on) in [
        ("show thinking", before.3),
        ("incremental md", before.4),
        ("stop-skill", before.5),
        ("full-auto", before.6),
    ] {
        let Ok(cb) = app.create_checkbutton(label) else {
            continue;
        };
        cb.set_active(on);
        dialog.append_content_area(&cb);
        check_widgets.push(cb);
    }
    {
        let slot = agent_slot.clone();
        let st = state.clone();
        let w = widgets.clone();
        let dialog_save = dialog.clone();
        let thinking_save = pending_thinking.clone();
        let checks_save = check_widgets.clone();
        menu_dialog_button(app, &dialog, "Save", move || {
            let mut chosen = before.clone();
            chosen.2 = thinking_save.borrow().clone();
            for (i, cb) in checks_save.iter().enumerate() {
                let on = cb.is_active();
                match i {
                    0 => chosen.3 = on,
                    1 => chosen.4 = on,
                    2 => chosen.5 = on,
                    3 => chosen.6 = on,
                    _ => {}
                }
            }
            let show = chosen.3;
            let lines = apply_gui_settings(&slot, &before, &chosen);
            dialog_save.close();
            let mut s = st.lock().unwrap();
            s.show_thinking = show;
            for line in lines {
                s.push(ConvKind::System, &line);
            }
            sync_textview(&w, &mut s);
        });
    }
    {
        let dialog_cancel = dialog.clone();
        menu_dialog_button(app, &dialog, "Discard", move || {
            dialog_cancel.close();
        });
    }
    dialog.present();
}

/// The about text: same facts as the terminal's About screen (version, git,
/// build profile, license, deps), as plain pane lines — no ANSI, no tty.
fn about_lines() -> Vec<String> {
    vec![
        format!("pir {}", env!("CARGO_PKG_VERSION")),
        format!("git: {}", env!("GIT_HASH")),
        format!(
            "build: {} · opt-level {} · lto {}",
            if cfg!(debug_assertions) { "debug" } else { "release" },
            option_env!("OPT_LEVEL").unwrap_or("?"),
            option_env!("LTO").unwrap_or("?"),
        ),
        "license: GPL-3.0".to_string(),
        String::new(),
        "deps: pulldown-cmark · streamdown-parser · rustyline · lsb-curl · smol · crossterm".to_string(),
    ]
}

/// Run one `/menu <sub>` action: the single implementation every reach path
/// shares — the typed line, the topbar item (which routes `command: "menu",
/// arg: "<sub>"` through the same `run_command_line`), and the menu dialog
/// button. `extra` is the rest of the typed line: resume/model/thinking accept
/// it as a direct value (mirroring the "fill the argument from the prompt"
/// convention), the rest ignore it.
#[allow(clippy::too_many_arguments)]
fn run_menu_action(
    sub: &str,
    extra: &[&str],
    agent_slot: &AgentSlot,
    providers: &[Provider],
    app: &App,
    sv: &SessionView,
) {
    let state = &sv.state;
    let widgets = &sv.widgets;
    let hist_nav = &sv.hist_nav;
    match sub {
        "resume" => {
            let token = extra.join(" ");
            if token.trim().is_empty() {
                open_resume_dialog(app, agent_slot, state, widgets, hist_nav, false);
                return;
            }
            match crate::session::resolve_unfinished(&token) {
                Some(path) => resume_session_in_gui(agent_slot, &path, state, widgets, hist_nav),
                None => {
                    let mut s = state.lock().unwrap();
                    s.push(
                        ConvKind::Error,
                        &format!("· no unfinished session matches '{token}' (see /unfinished)"),
                    );
                    sync_textview(widgets, &mut s);
                }
            }
        }
        "backgrounded" => {
            open_resume_dialog(app, agent_slot, state, widgets, hist_nav, true);
        }
        "model" => {
            let sel = extra.join(" ");
            if sel.trim().is_empty() {
                open_model_dialog(app, agent_slot, providers, state, widgets);
                return;
            }
            switch_model_in_gui(agent_slot, providers, &sel, state, widgets);
        }
        "thinking" => {
            let arg = extra.join(" ");
            if arg.trim().is_empty() {
                open_thinking_dialog(app, agent_slot, state, widgets);
                return;
            }
            // Direct set, mirroring the `/thinking` arm's parse order (level
            // first, optional trailing show/hide).
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
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                drop(g);
                let mut s = state.lock().unwrap();
                s.push(ConvKind::System, "· agent busy (turn running) — try again when idle");
                sync_textview(widgets, &mut s);
                return;
            };
            if !level_arg.is_empty() {
                match crate::config::ThinkingLevel::parse(&level_arg) {
                    Some(lvl) => {
                        let msg = agent.set_thinking(lvl);
                        drop(g);
                        let mut s = state.lock().unwrap();
                        s.push(ConvKind::System, &format!("· {msg}"));
                        sync_textview(widgets, &mut s);
                        return;
                    }
                    None => {
                        drop(g);
                        let mut s = state.lock().unwrap();
                        s.push(
                            ConvKind::Error,
                            &format!("· usage: /menu thinking [<off|minimal|low|medium|high|xhigh|max>] [show|hide] (got '{level_arg}')"),
                        );
                        sync_textview(widgets, &mut s);
                        return;
                    }
                }
            }
            if let Some(on) = show {
                let msg = agent.set_show_thinking(on);
                state.lock().unwrap().show_thinking = on;
                drop(g);
                let mut s = state.lock().unwrap();
                s.push(ConvKind::System, &format!("· {msg}"));
                sync_textview(widgets, &mut s);
            }
        }
        "security" => {
            open_security_dialog(app, agent_slot, state, widgets);
        }
        "settings" => {
            open_settings_dialog(app, agent_slot, state, widgets);
        }
        "worktrees" => {
            let on = crate::config::worktrees_default();
            match crate::config::set_worktrees_default(!on) {
                Ok(p) => {
                    let mut s = state.lock().unwrap();
                    s.push(
                        ConvKind::System,
                        &format!(
                            "· worktrees {} by default (next session). saved to {}",
                            if !on { "ON" } else { "OFF" },
                            p.display()
                        ),
                    );
                    sync_textview(widgets, &mut s);
                }
                Err(e) => {
                    let mut s = state.lock().unwrap();
                    s.push(
                        ConvKind::Error,
                        &format!("· could not persist worktree setting: {e}"),
                    );
                    sync_textview(widgets, &mut s);
                }
            }
        }
        "coverage" => {
            // Same flag `/autoclean coverage on|off` flips; turning it on also
            // shows which coverage tools will run, mirroring the pre-turn
            // report; turning it off drops a queued one-shot too.
            let on = !crate::AUTOCLEAN_COVERAGE.load(Ordering::SeqCst);
            crate::AUTOCLEAN_COVERAGE.store(on, Ordering::SeqCst);
            let mut s = state.lock().unwrap();
            if on {
                s.push(
                    ConvKind::System,
                    "· autoclean coverage ON — passes will drive branch coverage toward 100% where possible",
                );
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                for line in coverage_plan_lines(&crate::lints::coverage_status(&cwd)) {
                    s.push(ConvKind::System, &line);
                }
            } else {
                crate::AUTOCLEAN_COVERAGE_ONCE.store(false, Ordering::SeqCst);
                s.push(ConvKind::System, "· autoclean coverage OFF");
            }
            sync_textview(widgets, &mut s);
        }
        "help" => {
            let mut s = state.lock().unwrap();
            s.push(ConvKind::System, HELP_GUI);
            sync_textview(widgets, &mut s);
        }
        "about" => {
            let mut s = state.lock().unwrap();
            for line in about_lines() {
                s.push(ConvKind::System, &line);
            }
            sync_textview(widgets, &mut s);
        }
        "quit" => {
            let mut s = state.lock().unwrap();
            s.status = "bye".into();
            drop(s);
            app.quit();
        }
        other => {
            let mut s = state.lock().unwrap();
            let known: Vec<&str> = MENU_DIALOG_ACTIONS.iter().map(|(x, _)| *x).collect();
            s.push(
                ConvKind::Error,
                &format!("· unknown menu action '{other}' (choose one of: {})", known.join(", ")),
            );
            sync_textview(widgets, &mut s);
        }
    }
}

/// Open the GUI's main menu dialog: one button per [`MENU_DIALOG_ACTIONS`]
/// row. Non-blocking (see [`menu_dialog_button`]).
fn open_menu_dialog(
    app: &App,
    agent_slot: &AgentSlot,
    providers: &[Provider],
    sv: &SessionView,
) {
    let Ok(dialog) = app.create_dialog() else {
        let mut s = sv.state.lock().unwrap();
        s.push(
            ConvKind::Error,
            "· could not open the menu — every action is still available as /menu <action>",
        );
        sync_textview(&sv.widgets, &mut s);
        return;
    };
    dialog.set_title("pir — menu");
    fill_menu_list(app, &dialog, agent_slot, providers, sv);
    {
        let dialog_cancel = dialog.clone();
        menu_dialog_button(app, &dialog, "Cancel", move || {
            dialog_cancel.close();
        });
    }
    dialog.present();
}
/// Fill `dialog` with the main menu's items — one button per
/// [`MENU_DIALOG_ACTIONS`] row, stacked vertically. The terminal `/menu` is a
/// single list of rows; appending the buttons straight into the dialog's
/// content area (as this used to) put them side by side across one line.
fn fill_menu_list(
    app: &App,
    dialog: &GuiDialog,
    agent_slot: &AgentSlot,
    providers: &[Provider],
    sv: &SessionView,
) {
    let Ok(list) = app.create_box(Orientation::Vertical, 2) else {
        return;
    };
    for (sub, label) in MENU_DIALOG_ACTIONS {
        let sub = sub.to_string();
        let slot = agent_slot.clone();
        let provs = providers.to_vec();
        let sv_cb = sv.clone();
        let app2 = app.clone();
        menu_box_button(app, dialog, &list, label, move || {
            run_menu_action(&sub, &[], &slot, &provs, &app2, &sv_cb);
        });
    }
    list.set_vexpand(true);
    dialog.append_content_area(&list);
}

/// Build the `/menu` bar: one clickable item per [`MENU_DIALOG_ACTIONS`] row,
/// in the terminal's own order.
///
/// Why this exists next to the menu bar: the menu bar sorts commands by
/// *category* (Session / Model / Turn / View / Safety / Help), which is the
/// right shape for finding a command you already know — but the operator's
/// question was "where are all the /menu options?", and the answer used to be
/// "behind a dialog nothing in the window mentioned". This strip is the
/// terminal's menu made visible: `Resume / pick session`, `Backgrounded
/// sessions`, `Model`, `Thinking`, `Security`, `Settings`, `Worktrees by
/// default`, `Autoclean branch coverage`, `Help`, `About`, `Quit` — the same
/// rows in the same order.
///
/// It is not a second implementation: each item runs [`run_menu_action`], the
/// same function the dialog button and the typed `/menu <sub>` run.
fn build_menu_strip(
    app: &App,
    sessions: &Rc<RefCell<Vec<SessionView>>>,
    tabs: &TabView,
    providers: &[Provider],
    agent_slot: &AgentSlot,
) -> Result<GuiBox, String> {
    let strip = app
        .create_box(Orientation::Horizontal, 2)
        .map_err(|e| format!("create_box: {e}"))?;
    for (sub, label) in MENU_DIALOG_ACTIONS {
        let btn = app
            .create_button(label)
            .map_err(|e| format!("create_button({label}): {e}"))?;
        let sub_cb = sub.to_string();
        let sub_dbg = sub.to_string();
        let sessions = sessions.clone();
        let tabs = tabs.clone();
        let providers = providers.to_vec();
        let slot = agent_slot.clone();
        // The `&App` this function borrows cannot be captured by a `'static`
        // callback; carry its address as a `usize` and reborrow at the call
        // site — the same technique and the same safety argument as
        // `build_menu_bar`: `run` owns the `App` for as long as any callback
        // can fire.
        let app_addr: usize = app as *const App as usize;
        if btn
            .on_click(move || {
                // Read the current tab at click time: the strip outlives any
                // one tab, and a stale `SessionView` would paint into a closed
                // one.
                let sv = { sessions.borrow().get(tabs.current_tab()).cloned() };
                let Some(sv) = sv else { return };
                let mut s = sv.state.lock().unwrap();
                s.push(ConvKind::System, &format!("· menu: {sub_dbg}"));
                drop(s);
                // SAFETY: `app_addr` is the `App` passed to `run`, which does
                // not return while any callback can fire (see above).
                let app_ref = unsafe { &*(app_addr as *const App) };
                run_menu_action(&sub_cb, &[], &slot, &providers, app_ref, &sv);
            })
            .is_err()
        {
            // A button that never fires is exactly the "menu does nothing"
            // report; say so rather than swallow it.
            return Err(format!("menu item {sub:?} not connected"));
        }
        strip.append(&btn);
    }
    Ok(strip)
}

/// Handle a slash command inline (a GUI-appropriate subset). Mirrors the
/// streaming REPL's commands but reads/writes the GUI's own state/agent slot.
///
/// `sv` is the tab's `SessionView`, passed only for `/menu`, which hands it to
/// [`run_menu_action`] (the same view the strip items act on).
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
    hist_nav: &Arc<Mutex<HistoryNav>>,
    sv: &SessionView,
) {
    // `hist_nav` is the same `sv.hist_nav` the callers pass; `/menu` reads it
    // off the view so the dispatch and the strip cannot disagree about which
    // navigation state an action resets.
    let _ = hist_nav;
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
        "compact" => {
            // `/compact` — summarise the conversation so far and replace the
            // history with that summary. The GUI had no arm for it at all: the
            // command was neither completable nor implemented, so a session that
            // ran into "input + requested output exceeds the context window" (the
            // message the agent prints) had no way to recover inside the GUI.
            //
            // The summariser is a blocking model call over the whole
            // conversation — the one call that can take minutes on a large
            // session — so it runs on a worker that takes the agent out of the
            // slot, exactly like `spawn_goal`. Running it inline froze every
            // window and made `/cancel` untypable.
            let arg = rest.first().copied().unwrap_or("").trim().to_ascii_lowercase();
            match arg.as_str() {
                "status" | "stat" => {
                    let g = agent_slot.lock().unwrap();
                    let Some(agent) = g.as_ref() else {
                        s.push(ConvKind::System, "· agent busy (turn running) — try again when idle");
                        return;
                    };
                    let used = agent.context_tokens() as u64;
                    let cap = agent.context_budget() as u64;
                    let pct = if cap > 0 { (used * 100) / cap } else { 0 };
                    let mut line = format!(
                        "· context ~{} / {} used ({pct}%) across {} messages",
                        fmt_tok(used),
                        fmt_tok(cap),
                        agent.history_messages()
                    );
                    if agent.needs_compaction() {
                        line.push_str(" — near the cap: /compact to summarise");
                    }
                    s.push(ConvKind::System, &line);
                }
                "on" | "auto" => {
                    let mut g = agent_slot.lock().unwrap();
                    match g.as_mut() {
                        Some(agent) => {
                            agent.set_auto_compact(true);
                            s.push(ConvKind::System, "· auto-compact ON");
                        }
                        None => s.push(ConvKind::System, "· agent busy (turn running) — try again when idle"),
                    }
                }
                "off" => {
                    let mut g = agent_slot.lock().unwrap();
                    match g.as_mut() {
                        Some(agent) => {
                            agent.set_auto_compact(false);
                            s.push(ConvKind::System, "· auto-compact OFF");
                        }
                        None => s.push(ConvKind::System, "· agent busy (turn running) — try again when idle"),
                    }
                }
                "" => {
                    let busy = {
                        let g = agent_slot.lock().unwrap();
                        g.is_none()
                    };
                    if busy || s.running {
                        s.push(ConvKind::System, "· agent busy (turn running) — /compact needs the history, try when idle");
                        return;
                    }
                    let used = {
                        let g = agent_slot.lock().unwrap();
                        g.as_ref().map(|a| a.context_tokens()).unwrap_or(0)
                    };
                    s.push(
                        ConvKind::System,
                        &format!(
                            "· compacting conversation (~{} est. tokens) — this calls the model, please wait…",
                            fmt_tok(used as u64)
                        ),
                    );
                    s.running = true;
                    s.awaiting_input = false;
                    s.status = running_status(&s, 0);
                    sync_textview(widgets, &mut s);
                    spawn_compact(agent_slot);
                }
                other => s.push(
                    ConvKind::Error,
                    &format!(
                        "· usage: /compact [status|on|off]   (no arg = summarise now) — got {other:?}"
                    ),
                ),
            }
            sync_textview(widgets, &mut s);
        }

        "new-project" => {
            // Open a project directory: an existing one, or a new one to
            // create. With a path the launch is immediate; with no argument the
            // picker opens (pre-filled with this workspace). The same three
            // guards as the sidebar button and the Session menu item — all of
            // them reach `open_project` / `open_project_picker`.
            let path = rest.join(" ");
            // Take the pane lock back only around the report; the dialog and
            // any spawn must not run under it.
            drop(s);
            if path.trim().is_empty() {
                open_project_picker(app, state, widgets, full_auto, None);
            } else {
                open_project(app, &path, false, state, widgets, full_auto);
            }
            return;
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
            if rest.is_empty() {
                // Bare `/model` is the browse. It used to print the current
                // model and stop, so the only way to change models from the GUI
                // was to type a selector from memory — while the terminal opened
                // a picker for the same command. Opens the dialog the
                // Model → "Pick model…" item runs.
                let current = {
                    let g = agent_slot.lock().unwrap();
                    g.as_ref().map(|a| a.label())
                };
                let Some(label) = current else {
                    s.push(ConvKind::System, "· agent busy (turn running) — try again when idle");
                    return;
                };
                s.push(ConvKind::System, &format!("current model: {label}"));
                drop(s);
                open_model_dialog(app, agent_slot, providers, state, widgets);
                return;
            }
            let mut g = agent_slot.lock().unwrap();
            let Some(agent) = g.as_mut() else {
                s.push(ConvKind::System, "· agent busy (turn running) — try again when idle");
                return;
            };
            match crate::config::select(providers, &rest.join(" ")) {
                Ok((p, m)) => match agent.switch(p.clone(), m.clone()) {
                    Ok(()) => s.push(ConvKind::System, &format!("→ {}", agent.label())),
                    Err(e) => s.push(ConvKind::Error, &format!("{e}")),
                },
                Err(e) => s.push(ConvKind::Error, &format!("{e}")),
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
        "markup_demo" | "md_demo" => {
            // The streaming REPL's `/markup_demo`: show the canned markdown
            // sample (`MARKUP_DEMO` in main.rs — headings, emphasis, lists,
            // blockquote, fenced code, table, link) through the pane's own
            // markdown path, not the terminal renderer. Rich mode keeps raw
            // markdown in the model and each assistant line is Pango-rendered
            // at paint time (exactly like a drained model reply in
            // `drain_session_text`); plain mode stores `md::render` output,
            // the same fallback the log drain uses.
            #[cfg(feature = "gui-rich-text")]
            {
                s.push(ConvKind::Assistant, crate::MARKUP_DEMO);
            }
            #[cfg(not(feature = "gui-rich-text"))]
            {
                s.push(ConvKind::Assistant, &crate::md::render(crate::MARKUP_DEMO, false));
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
        "menu" => {
            // The GUI's main menu (see [`MENU_DIALOG_ACTIONS`]): a GTK dialog
            // with one button per action, each running the same
            // [`run_menu_action`] path a typed `/menu <sub>` takes. The dialog
            // is fire-and-forget from the GTK main thread (see
            // [`menu_dialog_button`]), so the arm returns immediately.
            //
            // With an argument this runs the action directly — which is also
            // what every topbar item does: `MENU_ACTIONS` entries with
            // `command: "menu"` build `/menu <sub>` lines through the same
            // `run_command_line` a typed line takes.
            let sub = rest.first().copied().unwrap_or("");
            if sub.trim().is_empty() {
                drop(s);
                open_menu_dialog(app, agent_slot, providers, sv);
                return;
            }
            drop(s);
            run_menu_action(sub, &rest[1..], agent_slot, providers, app, sv);
            return;
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
                // "try /help" was the streaming REPL's line, and in this GUI
                // it pointed at a help text that did not list `/menu`.
                None => s.push(
                    ConvKind::Error,
                    &format!(
                        "unknown command /{other} — /help lists the GUI's commands \
                         (unlisted terminal commands run in the streaming REPL: pir --no-tui)"
                    ),
                ),
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
        // `menu`, `model` and `models` share no prefix past `/m`, so `/m` has
        // no common completion — the same rule that keeps `/markup_demo` out
        // of the list. `/men` and `/mode` are unambiguous.
        assert_eq!(complete_idle("/m"), None);
        assert_eq!(complete_idle("/men"), Some("/menu".to_string()));
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
    ///
    /// NOTE: the pane keeps `can_focus(true)` so text stays SELECTABLE (a
    /// non-focusable view can never hold a selection, so there is nothing to
    /// copy). What changed is that typing can never land in it: it is
    /// `editable(false)`, and typed keys go to the Entry because focus
    /// normally lives there. Assert exactly that contract here.
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

        // The fix: the GUI's configuration keeps the pane read-only (no
        // typing into it) but focusable (so text stays selectable for copy).
        let Ok(fixed) = app.create_textview() else {
            eprintln!("skipping: could not create textview");
            return;
        };
        configure_conversation_textview(&fixed);
        assert!(!fixed.get_editable(), "conversation TextView must be read-only");
        assert!(fixed.get_can_focus(), "conversation TextView must stay focusable so its text is selectable for copy");
    }

    #[test]
    fn conversation_pane_supports_copy() {
        // The copy path needs every link in the chain: has_focus / copy
        // affordances exist on the adapter and the Edit menu offers Copy.
        // No display needed: these are source properties of the shipped code.
        let src = include_str!("gui.rs");
        for (what, needle) in [
            ("TextView::has_focus wrapper", "pub fn has_focus(&self)"),
            ("TextView::copy_clipboard wrapper", "pub fn copy_clipboard(&self)"),
            ("Edit > Copy menu item", "\"Edit\", \"Copy\", \"edit-copy\""),
            ("Copy menu handler", "view_action_cb == \"edit-copy\""),
            ("Ctrl-C focus guard", "textview.has_focus()"),
        ] {
            assert!(
                src.contains(needle),
                "{what} missing ({needle:?} not found): copy has no working path"
            );
        }
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
            "help", "menu", "model", "models", "goal", "continue", "clear",
            "undo", "finished", "fix", "shell", "sh", "bg", "jobs", "thinking",
            "cancel", "reload", "reexec", "exit", "checkpoints", "rewind",
            "quarantine", "limits", "skills", "scratchpad",
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

    /// Every command the GUI implements must be documented in its own `/help`
    /// text. The failure this pins: `/menu` had a full `handle_command` arm, a
    /// dialog, and a menu-bar entry for every one of its actions — but no line
    /// in `HELP_GUI` and no entry in `COMMANDS`. An operator who ran `/help`
    /// (or tab-completed `/m`) saw a GUI with no menu at all, and the "unknown
    /// command /menu" a terminal build prints made that conclusion look
    /// confirmed.
    #[test]
    fn every_gui_command_is_documented_in_its_own_help() {
        let help: &str = HELP_GUI;
        // A command is documented if its line starts with `/name`, or if that
        // line names it in parens (`(aliases: /shell; …)`, `(alias: /resources)`).
        let documented = |cmd: &str| {
            let pat = format!("/{cmd}");
            help.lines().any(|l| {
                let t = l.trim_start();
                t.starts_with(&pat)
                    || t.contains(&format!("{pat} ")) && t.contains("alias")
                    || t.contains(&format!("{pat},"))
                    || t.contains(&format!("{pat})"))
            })
        };
        // Aliases are documented under the canonical command's line (the same
        // rule the menu bar follows), so check that line too.
        let canonical = |cmd: &str| canonical_command(cmd);
        for cmd in COMMANDS {
            assert!(
                documented(cmd) || documented(&canonical(cmd)),
                "/{cmd} is completable but missing from HELP_GUI"
            );
        }
        for cmd in handled_commands() {
            // `/sessions` has no `handle_command` arm (see
            // `every_completable_command_is_actually_handled`), so it is
            // covered through `COMMANDS` above.
            if cmd == "sessions" {
                continue;
            }
            assert!(
                documented(&cmd) || documented(&canonical(&cmd)),
                "/{cmd} is handled but missing from HELP_GUI"
            );
        }
    }

    /// `/menu` must be discoverable from the two places an operator looks
    /// without being told: Tab completion and its own `/help`. Both missed it
    /// while the command worked, which is how it came to read as "unknown
    /// command /menu".
    #[test]
    fn slash_menu_is_offered_by_tab_and_help() {
        assert!(
            COMMANDS.contains(&"menu"),
            "/menu is implemented but Tab completion does not offer it"
        );
        assert_eq!(complete_idle("/me").or(complete_idle("/men")), Some("/menu".to_string()));
        assert_eq!(complete_idle("/men"), Some("/menu".to_string()));
        assert!(
            HELP_GUI.lines().any(|l| l.trim_start().starts_with("/menu")),
            "/help must list /menu — it is the GUI's whole command surface"
        );
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
        // `/compact` is the longest single model call a session makes (a whole
        // transcript in, a summary out), so it carries the same rule: the arm
        // dispatches to a worker and never calls `agent.compact()` itself, or
        // the window freezes for the entire summarisation and `/cancel` cannot
        // even be typed. The GUI previously had no `/compact` at all.
        assert!(
            !body.contains(".compact()"),
            "the /compact arm calls compact() inline, which blocks the GTK main thread — \
             route it through spawn_compact()"
        );
        assert_eq!(
            body.matches("spawn_compact(").count(),
            1,
            "/compact must dispatch via spawn_compact()"
        );
    }

    /// `/compact` had no GUI arm, no completion entry and no help line, so a GUI
    /// session that ran out of context window had no way to recover and the
    /// agent's own "run /compact" advice named a command the GUI did not have.
    #[test]
    fn compact_is_reachable_from_the_gui() {
        assert!(COMMANDS.contains(&"compact"), "/compact must be tab-completable");
        assert!(
            HELP_GUI.contains("/compact "),
            "HELP_GUI must document /compact"
        );
        assert_eq!(complete_idle("/comp"), Some("/compact".to_string()));
        assert_eq!(complete_idle("/com"), Some("/compact".to_string()));
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
        // the bottom re-arms it. Positions are (value, upper, page): the pane
        // is at the bottom exactly when value + page reaches upper.
        let mut s = GuiState::new();
        // Viewport shows 0..20 of a 100px extent: top, not bottom.
        s.observe_scroll(0.0, 100.0, 20.0);
        assert!(!s.follow, "scrolling away from the bottom must disarm follow");
        s.observe_scroll(0.0, 100.0, 20.0); // another append while scrolled up
        assert!(!s.follow, "an append while scrolled up must not re-arm follow");
        // Value 80 + page 20 == upper 100: bottom — and the empty-extent
        // case (0, 0, 0) is the top == bottom too.
        s.observe_scroll(80.0, 100.0, 20.0);
        assert!(s.follow, "returning to the bottom must re-arm follow");
        s.observe_scroll(0.0, 0.0, 0.0);
        assert!(s.follow, "an empty pane is trivially at the bottom");
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

    // -- S6: keyboard / menu scroll commands --------------------------------

    #[test]
    fn scroll_commands_move_the_window_and_arm_follow_at_the_edges() {
        // S6: the Page_Up/Page_Down/Home/End keys and the View menu's scroll
        // items share `scroll_page_up/down`, `scroll_home` and `scroll_end` —
        // one implementation, so key and menu can never move different windows.
        let mut s = GuiState::new();
        for i in 0..100 {
            s.push(ConvKind::User, &format!("line {i}"));
        }
        let total = s.conv.len();
        // Live: the first PageUp starts one full page above the tail.
        scroll_page_up(&mut s);
        assert!(!s.follow, "paging up must disarm follow");
        assert_eq!(
            s.scroll.offset,
            s.scroll.max_offset(total).saturating_sub(scroll_step(&s)),
            "first PageUp starts one page above the tail, not from a stale offset"
        );
        // Home jumps to the top; End returns to the live tail and re-arms.
        scroll_home(&mut s);
        assert_eq!(s.scroll.offset, 0, "Home must jump to the top");
        assert!(!s.follow);
        scroll_end(&mut s);
        assert!(s.follow, "End must re-arm follow");
        assert_eq!(s.scroll.offset, s.scroll.max_offset(total));
        // PageDown at the bottom edge re-arms follow; short of it, it stays off.
        scroll_home(&mut s);
        scroll_page_down(&mut s);
        assert!(!s.follow, "a mid-buffer PageDown must not re-arm follow");
        assert!(s.scroll.offset > 0, "PageDown must advance");
        for _ in 0..50 {
            scroll_page_down(&mut s);
        }
        assert!(s.follow, "paging down to the bottom edge must re-arm follow");
        assert_eq!(s.scroll.offset, s.scroll.max_offset(total));
        // The window helper paints exactly the clamped offset..offset+page slice.
        s.scroll.page = 10;
        s.scroll.set(95, total);
        assert_eq!(s.scroll.window(total), (90, 100), "window must clamp to the tail");
        s.scroll.set(200, total);
        assert_eq!(s.scroll.window(total), (90, 100), "a stale offset must clamp, not overrun");
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
        // S7: the TUI renders assistant markdown (tui.rs:1697). What "rendered"
        // means depends on the pane:
        //
        // * plain (`--no-default-features`): the model holds ANSI-stripped
        //   `md::render(_, false)` output — the same renderer the TUI calls,
        //   so the assertion is equality with it, not a re-implementation.
        // * rich (default `gui-rich-text`): the model keeps RAW markdown and
        //   `sync_textview` converts with `md::render_pango`; asserting
        //   equality with the ANSI renderer would pin the wrong pipeline.
        //   Instead assert the raw text survives into the model (the rich
        //   conversion is covered by md::pango_tests + the drain test below).
        let raw = "# Heading\n\n- one\n- two\n\n```\ncode\n```\n";
        let mut s = GuiState::new();
        // Feed one assistant log line through the same text-drain path the
        // periodic drain uses, so the test covers the real route and not a
        // parallel re-implementation of it.
        drain_session_text(&jsonl_assistant_line(raw), &mut s);
        #[cfg(not(feature = "gui-rich-text"))]
        {
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
        #[cfg(feature = "gui-rich-text")]
        {
            let stored = s.conv.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
            assert!(
                stored.contains("# Heading"),
                "rich mode keeps raw markdown in the model (Pango runs at paint): {stored:?}"
            );
            let pango = crate::md::render_pango(raw);
            assert!(pango.contains("Heading"), "rich render must keep the text: {pango:?}");
            assert!(!pango.contains("# Heading"), "rich render must drop markers: {pango:?}");
        }
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
        // T7: a port of the TUI's update_tui_hint (tui.rs:1140) for the two
        // drafts it special-cased. Everything else is derived from the
        // completion candidates instead (next test).
        assert_eq!(
            hint_text("/thinking", None),
            "[Tab] levels: off | minimal | low | medium | high | xhigh | max · show | hide"
        );
        // `/` is every command: listed (capped), and it says to keep typing.
        let all = hint_text("/", None);
        assert!(all.starts_with("[Tab] /help"), "got {all:?}");
        assert!(all.contains("… +"), "a capped list must say how many are hidden: {all:?}");
        assert!(all.contains("keep typing to narrow"), "got {all:?}");
        // Prose and complete lines carry no hint.
        for d in ["", "hello", "/thinking high", "/model x", "  ", "/sessions"] {
            assert_eq!(hint_text(d, None), "", "draft {d:?} must not show a hint");
        }
    }

    /// The report: `/men` completes to `/menu`, and nothing anywhere said which
    /// completions exist — Tab was silent whether it completed, was ambiguous,
    /// or matched nothing.
    #[test]
    fn a_draft_shows_what_it_can_be_completed_to() {
        // One candidate: name the completion Tab will produce.
        assert_eq!(
            completion_hint("/men", None, true),
            "[Tab] completes to /menu",
            "typing /men must say where Tab goes"
        );
        // Several: list them all — the GUI's stand-in for the terminal's list
        // under the prompt.
        let h = completion_hint("/m", None, true);
        for c in ["/menu", "/model", "/models"] {
            assert!(h.contains(c), "{c} missing from the ambiguity list: {h:?}");
        }
        assert!(!h.starts_with("[Tab] —"), "a list needs a lead-in: {h:?}");
        // After Tab: say it could not complete, and still show the list.
        let h = completion_hint("/m", Some("[Tab] 3 matches"), true);
        assert!(h.starts_with("[Tab] 3 matches — "), "got {h:?}");
        assert!(h.contains("/menu"), "the list must survive a failed Tab: {h:?}");
        // A note with nothing left to offer is still shown (never swallowed).
        assert_eq!(
            completion_hint("hello", Some("[Tab] no match"), true),
            "[Tab] no match"
        );
        // An already-complete command is not re-suggested.
        assert_eq!(completion_hint("/menu", None, true), "");
        // After a *successful* Tab the note is the whole hint — it must not
        // then advise the completion it just performed.
        assert_eq!(
            completion_hint("/men", Some("[Tab] completed to /menu"), true),
            "[Tab] completed to /menu"
        );
        // A sub-argument list shows the arguments, not the command again.
        let think = completion_hint("/thinking ", None, true);
        assert!(think.contains("off") && think.contains("minimal"), "got {think:?}");
        assert!(!think.contains("/thinking off"), "the command repeats on every entry: {think:?}");
    }

    /// The live path, on real widgets: the Entry's text is what the hint is
    /// derived from, so typing `/men` must put "completes to /menu" in the hint
    /// label *through the same code the drain runs*, and pressing Tab must then
    /// report what it did. The pure tests above cannot catch the hint being
    /// wired to the wrong widget, or the draft mirror never seeing the text.
    ///
    /// Skips without a display, like every display-backed test here.
    #[test]
    fn typing_a_prefix_paints_the_completion_hint() {
        let Some((app, _guard)) = gtk_test_app() else { return };
        let entry = app.create_entry().expect("create_entry");
        let status = app.create_label("x").expect("create_label");
        let hint = app.create_label("").expect("create_label");
        let tv = app.create_textview().expect("create_textview");
        let scroller = app.create_scrolled_window().expect("create_scrolled_window");
        let widgets = GuiWidgets { textview: tv, status, scroller, hint: hint.clone() };
        let mut s = GuiState::new();

        entry.set_text("/men");
        // Exactly what the drain does with the Entry's text.
        s.draft = entry.get_text().unwrap_or_default();
        let shown = hint_text(&s.draft, s.tab_note.as_deref());
        hint.set_markup(&status_markup(&shown));
        assert_eq!(
            hint.get_text().as_deref(),
            Some("[Tab] completes to /menu"),
            "typing /men must say where Tab goes"
        );

        // Tab could not decide on a shorter prefix: the note carries it.
        s.tab_note = Some("[Tab] 3 matches".to_string());
        let shown = hint_text(&s.draft, s.tab_note.as_deref());
        hint.set_markup(&status_markup(&shown));
        assert!(
            hint.get_text().unwrap_or_default().starts_with("[Tab] 3 matches"),
            "got {:?}",
            hint.get_text()
        );
    }

    /// The list the hint shows and the string Tab produces must come from the
    /// same match set — otherwise the hint could advertise a command Tab
    /// refuses, or hide one Tab would take.
    #[test]
    fn the_completion_list_is_exactly_what_tab_matches() {
        let frags = ["", "/", "/m", "/men", "/mo", "/mode", "/models", "/th", "/thinking l", "/zzz", "hello"];
        for f in frags {
            let cands = completion_candidates(f);
            // Whatever Tab completes to must be IN that list (or the list must
            // be empty, meaning Tab has nothing to do).
            match complete_idle(f) {
                Some(done) => assert!(
                    cands.iter().any(|c| c.starts_with(&done) || *c == done),
                    "Tab completes {f:?} to {done:?}, which the hint does not list: {cands:?}"
                ),
                None => assert!(
                    cands.len() != 1,
                    "Tab refuses {f:?} but the hint names exactly one candidate {cands:?}"
                ),
            }
        }
        // And the specific report: `/men` -> one candidate, `/menu`.
        assert_eq!(completion_candidates("/men"), vec!["/menu".to_string()]);
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

    #[test]
    fn model_menu_tree_groups_by_provider_and_marks_active() {
        // The Model menu tree (Providers > Models, with org/ groups): one
        // subtree per provider, every model listed, the active `provider/id`
        // carrying the `●` marker. Action names are positional
        // (`model-p<pi>-m<mi>`) because model ids carry `/`, `:`, `~` that
        // cannot survive GTK dispatch.
        use super::ModelMenuEntry;
        // Flatten one provider's entries to `(label, action, full_id)` leaves.
        fn leaves(es: &[ModelMenuEntry]) -> Vec<(&str, &str, &str)> {
            let mut out = Vec::new();
            for e in es {
                match e {
                    ModelMenuEntry::Leaf(l, a, id) => out.push((l.as_str(), a.as_str(), id.as_str())),
                    ModelMenuEntry::Group(_, ls) => {
                        for (l, a, id) in ls {
                            out.push((l.as_str(), a.as_str(), id.as_str()));
                        }
                    }
                }
            }
            out
        }
        fn prov(id: &str, models: &[&str]) -> Provider {
            Provider {
                id: Some(id.into()),
                name: None,
                api: Some("openai-completions".into()),
                base_url: None,
                api_key: None,
                models: models
                    .iter()
                    .map(|m| crate::config::Model {
                        id: (*m).into(),
                        name: None,
                        context: Some(1000),
                        max_tokens: None,
                        api_override: None,
                        url_override: None,
                        no_reasoning_effort: false,
                        reasoning: false,
                        thinking_format: None,
                        supports_reasoning_effort: None,
                        thinking_level_map: Default::default(),
                        price_per_1k: None,
                    })
                    .collect(),
            }
        }
        let provs = vec![prov("acme", &["small", "big"]), prov("other", &["small"])];
        let tree = super::model_menu_tree(&provs, "acme/big");
        assert_eq!(tree.len(), 2, "one subtree per provider: {tree:?}");
        assert_eq!(tree[0].0, "acme");
        assert_eq!(tree[1].0, "other");
        // Same model id under two providers: both listed, actions distinct.
        assert_eq!(leaves(&tree[0].1).len(), 2);
        assert_eq!(leaves(&tree[1].1).len(), 1);
        // No `/` in these ids: everything is a flat leaf, no groups.
        assert!(
            tree.iter().flat_map(|(_, es)| es).all(|e| matches!(e, ModelMenuEntry::Leaf(..))),
            "slash-free ids must not group: {tree:?}"
        );
        // Active marker on exactly the selected model.
        let marked: Vec<_> = tree
            .iter()
            .flat_map(|(_, es)| leaves(es))
            .filter(|(l, _, _)| l.starts_with("●"))
            .collect();
        assert_eq!(marked.len(), 1, "exactly one active marker: {tree:?}");
        assert!(marked[0].0.contains("big"), "marker on acme/big: {marked:?}");
        assert_eq!(marked[0].2, "big", "marker carries the model id: {marked:?}");
        // Positional, unique action names across the whole tree.
        let mut actions: Vec<_> = tree
            .iter()
            .flat_map(|(_, es)| leaves(es).into_iter().map(|(_, a, _)| a.to_string()))
            .collect();
        actions.sort();
        actions.dedup();
        assert_eq!(actions.len(), 3, "action names must be unique: {actions:?}");
        assert!(actions.iter().all(|a| a.starts_with("model-p")), "{actions:?}");
        // Empty active: no marker anywhere, tree still complete.
        let tree = super::model_menu_tree(&provs, "");
        assert!(
            tree.iter().flat_map(|(_, es)| leaves(es)).all(|(l, _, _)| !l.starts_with("●")),
            "no active model means no marker"
        );
    }

    #[test]
    fn model_menu_tree_splits_slash_ids_into_groups() {
        // `openrouter > anthropic/claude-x` becomes `openrouter > anthropic >
        // claude-x`: ids with an `org/` prefix nest one layer, flat ids stay
        // flat, and dispatch still resolves the full id.
        use super::ModelMenuEntry;
        fn prov(id: &str, models: &[&str]) -> Provider {
            Provider {
                id: Some(id.into()),
                name: None,
                api: Some("openai-completions".into()),
                base_url: None,
                api_key: None,
                models: models
                    .iter()
                    .map(|m| crate::config::Model {
                        id: (*m).into(),
                        name: None,
                        context: Some(1000),
                        max_tokens: None,
                        api_override: None,
                        url_override: None,
                        no_reasoning_effort: false,
                        reasoning: false,
                        thinking_format: None,
                        supports_reasoning_effort: None,
                        thinking_level_map: Default::default(),
                        price_per_1k: None,
                    })
                    .collect(),
            }
        }
        let provs = vec![prov(
            "openrouter",
            &["anthropic/claude-x", "anthropic/claude-y", "flat-model"],
        )];
        let tree = super::model_menu_tree(&provs, "openrouter/anthropic/claude-y");
        assert_eq!(tree.len(), 1);
        let entries = &tree[0].1;
        assert_eq!(entries.len(), 2, "one group + one flat leaf: {entries:?}");
        // Group first (first-encounter order), flat leaf after.
        let (org, grp) = match &entries[0] {
            ModelMenuEntry::Group(org, leaves) => (org.as_str(), leaves),
            other => panic!("first entry must be the anthropic group: {other:?}"),
        };
        assert_eq!(org, "anthropic");
        assert_eq!(grp.len(), 2, "both org models nest: {grp:?}");
        // Leaf labels show the tail only; the full id rides the dispatch.
        assert!(grp[0].0.contains("claude-x") && !grp[0].0.contains("anthropic/"), "tail label: {:?}", grp[0].0);
        assert_eq!(grp[0].2, "anthropic/claude-x", "full id preserved: {:?}", grp[0]);
        // Active marker on the nested leaf, matched on the full label.
        assert!(grp[1].0.starts_with("●"), "active nested leaf marked: {:?}", grp[1].0);
        assert_eq!(grp[1].2, "anthropic/claude-y");
        // Flat leaf keeps the whole id in its label.
        match &entries[1] {
            ModelMenuEntry::Leaf(label, _, id) => {
                assert!(label.contains("flat-model"), "{label:?}");
                assert_eq!(id, "flat-model");
            }
            other => panic!("second entry must be the flat leaf: {other:?}"),
        }
        // Actions stay positional and unique across groups.
        let mut actions: Vec<String> = Vec::new();
        for e in entries {
            match e {
                ModelMenuEntry::Leaf(_, a, _) => actions.push(a.clone()),
                ModelMenuEntry::Group(_, ls) => {
                    actions.extend(ls.iter().map(|(_, a, _)| a.clone()));
                }
            }
        }
        actions.sort();
        actions.dedup();
        assert_eq!(actions.len(), 3, "unique across groups: {actions:?}");
    }

    // -- G2: the menu bar ---------------------------------------------------

    #[test]
    fn prompt_entry_shows_a_placeholder_hint() {
        // The prompt Entry must carry the "Enter prompt here" placeholder so
        // an empty REPL still says where to type. Source-level (the hint is a
        // native GTK placeholder, not observable without a display): assert
        // the spawn_session Entry gets it, and that every Entry layer in the
        // toolkit chain actually offers the method — a missing link would
        // fail the build only on the backend that lacks it.
        let src = include_str!("gui.rs");
        let start = src
            .find("\nfn spawn_session(")
            .expect("spawn_session definition not found in gui.rs")
            + 1;
        let body = &src[start..];
        assert!(
            body.contains("set_placeholder_text(\"Enter prompt here\")"),
            "spawn_session must set the prompt placeholder on the Entry"
        );
    }

    #[test]
    fn help_menu_is_last_in_the_topbar() {
        // Help sits far right, the platform convention. `menu_order` is what
        // `build_menu_bar` seeds from, so asserting on it pins the bar itself;
        // a reorder of either table that let "Edit" (or anything else) slip
        // past Help fails here rather than as a visual report.
        let order = super::menu_order();
        assert_eq!(order.last(), Some(&"Help"), "Help must be last: {order:?}");
        // And it must actually exist in the tables — last of nothing is not
        // a Help menu.
        assert!(
            super::MENU_ACTIONS.iter().any(|a| a.menu == "Help"),
            "no Help submenu in the menu tables"
        );
    }

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
    // Layer 1 — G6: clickable links (pure span resolution)
    // ================================================================

    #[test]
    fn link_spans_resolve_against_inserted_text() {
        // G6: spans resolve against the exact string the buffer receives —
        // prefix included — in buffer chars (what GtkTextIter counts), not
        // bytes. The `💭` prefix is one char but four bytes: byte math would
        // point every span in a thinking line four columns too far right.
        let pairs = vec![("docs".to_string(), "https://example.com/a".to_string())];
        let spans = super::resolve_link_spans("  see docs here", &pairs);
        assert_eq!(spans.len(), 1, "got: {spans:?}");
        let (line, scol, ecol, url) = &spans[0];
        assert_eq!((*line, *scol, *ecol), (0, 6, 10), "got: {spans:?}");
        assert_eq!(url, "https://example.com/a");
    }

    #[test]
    fn link_spans_use_successive_search_for_repeated_text() {
        // G6: `[docs](u1)` … `[docs](u2)` must tag each occurrence with its
        // own URL — first-occurrence search would point both clicks at u1.
        let pairs = vec![
            ("docs".to_string(), "https://u1.example/".to_string()),
            ("docs".to_string(), "https://u2.example/".to_string()),
        ];
        let spans = super::resolve_link_spans("docs then docs", &pairs);
        assert_eq!(spans.len(), 2, "got: {spans:?}");
        assert_eq!(spans[0].3, "https://u1.example/");
        assert_eq!(spans[1].3, "https://u2.example/");
        assert!(spans[0].1 < spans[1].1, "second span must start later: {spans:?}");
    }

    #[test]
    fn link_spans_stay_in_step_with_conv_on_push_clear_trim() {
        // G6: `line_links` is parallel to `conv` — push/clear/trim move both,
        // so a click can never resolve against another line's URLs.
        let mut s = super::GuiState::new();
        s.push(super::ConvKind::Assistant, "see https://a.example/x");
        s.push(super::ConvKind::User, "no links here");
        assert_eq!(s.conv.len(), 2);
        assert_eq!(s.line_links.len(), 2, "one inventory entry per line");
        assert_eq!(s.line_links[0].len(), 1, "got: {:?}", s.line_links[0]);
        assert!(s.line_links[1].is_empty(), "got: {:?}", s.line_links[1]);
        // Multi-line push records one entry per paragraph.
        s.push(super::ConvKind::Assistant, "one https://b.example/\ntwo");
        assert_eq!(s.line_links.len(), 4, "split paragraphs each get an entry");
        s.clear();
        assert!(s.line_links.is_empty() && s.buf_links.is_empty(), "clear drops both maps");
    }

    #[test]
    fn lookup_link_hits_only_inside_a_span() {
        // G6: the click rule — the span containing the column, half-open, so
        // a click on the char just past a link does not open it.
        let spans = vec![(6u32, 10u32, "https://example.com/a".to_string())];
        assert_eq!(super::lookup_link(&spans, 6), Some("https://example.com/a"));
        assert_eq!(super::lookup_link(&spans, 9), Some("https://example.com/a"));
        assert_eq!(super::lookup_link(&spans, 5), None, "one char before must miss");
        assert_eq!(super::lookup_link(&spans, 10), None, "end is exclusive");
        assert_eq!(super::lookup_link(&[], 6), None, "no spans must miss");
    }

    #[test]
    fn link_spans_resolve_against_nwg_plain_line_text() {
        // G6 (NWG): the Windows pane inserts `set_markup(line_markup(line))`,
        // whose text is the markup with tags and entities stripped. Spans must
        // resolve against THAT string, not the markup: a span computed on
        // `<span foreground="#4ec9b0">· </span>see [docs](https://x.example/a)`
        // would sit hundreds of columns off. The strip rule is mirrored here
        // (same tag/entity grammar the adapter's `plain_line` implements) so
        // the contract is pinned on Linux CI too, where the NWG adapter is not
        // compiled.
        let line = ConvLine {
            kind: ConvKind::System,
            text: "see [docs](https://x.example/a) & more".to_string(),
        };
        let markup = line_markup(&line);
        let inserted = crate::md::pango_visible_text(&markup);
        assert!(!inserted.contains('<'), "tags must be gone: {inserted:?}");
        assert!(inserted.contains("& more"), "entities must be decoded: {inserted:?}");
        let pairs = record_link_pairs(&line.text);
        let spans = resolve_link_spans(&inserted, &pairs);
        assert_eq!(spans.len(), 1, "got: {spans:?}");
        let (line_no, scol, ecol, _url) = &spans[0];
        // `\u{b7} ` (2 chars) + `see ` (4) + `[` (1) => `docs` starts at char 7,
        // and is 4 chars wide. Columns are CHARS, not bytes: the `\u{b7}`
        // prefix glyph is two bytes, so byte maths would put every span one
        // column too far right — which is exactly the bug this pins.
        assert_eq!((*line_no, *scol, *ecol), (0, 7, 11), "got: {spans:?}");
        let hit: Vec<(u32, u32, String)> =
            spans.iter().map(|(_, s, e, u)| (*s, *e, u.clone())).collect();
        assert_eq!(lookup_link(&hit, *scol), Some("https://x.example/a"));
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

    #[test]
    fn windows_sync_builds_link_spans_from_the_inserted_text() {
        // G6 (NWG): the Windows `sync_textview` must record a span entry per
        // inserted line, resolved against the markup-stripped text the EDIT
        // control receives (`TextView::plain_line`), or `connect_link_clicks`
        // has no map to look a click up in and links silently stop working.
        let src = include_str!("gui.rs");
        let start = src
            .find("#[cfg(windows)]\nfn sync_textview(")
            .expect("the Windows sync_textview definition not found")
            + 1;
        let body = src[start..]
            .split("\n/// One row of the GUI's main menu")
            .next()
            .expect("the Windows sync_textview body not found");
        assert!(
            body.contains("plain_line("),
            "the Windows sync must strip exactly as `set_markup` does (TextView::plain_line)"
        );
        assert!(
            body.contains("buf_links.push("),
            "the Windows sync must push one span entry per inserted line"
        );
        assert!(
            body.contains("set_text(\"\")") && body.contains("buf_links.clear()"),
            "a rebuild replaces the buffer, so it must clear the span map too"
        );
    }

    #[test]
    fn windows_click_handler_guards_like_the_gtk_one() {
        // G6 (NWG): same guards as the GTK handler, in the same order — a
        // release ending a selection is a copy gesture, and the position must
        // land inside a recorded span before anything is opened. The two
        // backends must not disagree about what a click means.
        let src = include_str!("gui.rs");
        let start = src
            .find("#[cfg(windows)]\nfn connect_link_clicks(")
            .expect("the Windows connect_link_clicks definition not found")
            + 1;
        let body = src[start..]
            .split("\n/// Read a `ScrolledWindow`")
            .next()
            .expect("the Windows connect_link_clicks body not found");
        let sel = body
            .find("buffer_has_selection()")
            .expect("the Windows click handler must guard on buffer_has_selection()");
        let hit = body
            .find("char_index_at(")
            .expect("the Windows click handler must hit-test with char_index_at()");
        let map = body
            .find("buf_links")
            .expect("the Windows click handler must consult the buf_links span map");
        assert!(
            sel < hit && hit < map,
            "order must be: selection guard, hit-test, span lookup (got {sel} {hit} {map})"
        );
        // And the opener must be the same policy: re-validated, http/https only.
        let opener = src
            .find("#[cfg(windows)]\nfn open_link_safely(")
            .expect("the Windows open_link_safely definition not found")
            + 1;
        let opener_body = &src[opener..];
        let opener_body = opener_body
            .split("\n/// Wire link clicks on")
            .next()
            .unwrap_or(opener_body);
        assert!(
            opener_body.contains("sanitize_url") && opener_body.contains("open_url("),
            "the Windows opener must re-validate and call the adapter's open_url"
        );
    }

    #[test]
    fn click_handler_checks_selection_before_opening() {
        // G6: a button release that ends a text selection is a copy gesture,
        // never a link activation — selecting link text must not launch a
        // browser. The handler must consult `buffer_has_selection` before it
        // consults the span map (`buf_links`).
        let src = include_str!("gui.rs");
        let start = src
            .find("\nfn connect_link_clicks(")
            .expect("connect_link_clicks definition not found in gui.rs")
            + 1;
        let body = src[start..]
            .split("\nfn spawn_session(")
            .next()
            .expect("connect_link_clicks body not found");
        let sel = body
            .find("buffer_has_selection()")
            .expect("the click handler must guard on buffer_has_selection()");
        let map = body
            .find("buf_links")
            .expect("the click handler must consult the buf_links span map");
        assert!(
            sel < map,
            "the selection guard must come before the span-map lookup, or a \
             drag-release over a link opens it mid-copy"
        );
    }

    // ================================================================
    // Layer 1/2 — G7: New Project
    // ================================================================

    #[test]
    fn new_project_is_reachable_from_button_menu_and_command() {
        // G7: one action, three ways in (the sidebar button, the Session menu,
        // and `/new-project <dir>`) — the same rule every other GUI action
        // follows. The menu item must route through `handle_command` like every
        // other entry, so the menu and the typed line cannot diverge.
        assert!(
            COMMANDS.contains(&"new-project"),
            "/new-project must be Tab-completable, or the command is undiscoverable"
        );
        let item = MENU_ACTIONS
            .iter()
            .find(|a| a.command == "new-project")
            .expect("the Session menu must offer New project");
        assert!(
            item.label.to_lowercase().contains("project"),
            "the menu label must say what it does: {:?}",
            item.label
        );
        // The sidebar button: the label is the operator-facing entry point.
        let src = include_str!("gui.rs");
        assert!(
            src.contains("create_button(\"New Project\")"),
            "the sidebar must carry a literal `New Project` button"
        );
    }

    #[test]
    fn project_launch_detaches_and_never_moves_this_process() {
        // G7: the launcher starts a *new* process in the chosen directory.
        // It must not `set_current_dir` this process (that would repoint every
        // open tab, since the Agent, Registry, session log and workspace label
        // all read the process cwd), and it must detach with `setsid` — never
        // `process_group(0)`, which makes the child a group leader and can leave
        // it able to grab pir's terminal.
        let src = include_str!("project_launch.rs");
        assert!(
            !src.contains("set_current_dir"),
            "the launcher must not change this process's cwd — open a new process instead"
        );
        assert!(
            src.contains("current_dir(dir)"),
            "the child must run in the chosen directory"
        );
        assert!(
            src.contains("libc::setsid()"),
            "the child must detach with setsid()"
        );
        // Match the call form, not the bare name: the module doc explains why
        // `process_group(0)` is avoided, so a bare-substring check would match
        // its own rationale.
        assert!(
            !src.contains(".process_group("),
            "setsid, not process_group(0): a group leader can still grab the terminal"
        );
    }

    #[test]
    fn opener_only_allows_http_https() {
        // G6: `open_link_safely` re-validates and narrows to http/https.
        // `sanitize_url` also passes `mailto:`/`file:` — a `file:` URL from
        // model output would otherwise turn the log into a local-file read
        // primitive. The opener must name both allowed schemes and refuse the
        // rest by construction, not by blocklist.
        let src = include_str!("gui.rs");
        let start = src
            .find("\nfn open_link_safely(")
            .expect("open_link_safely definition not found in gui.rs")
            + 1;
        let body = src[start..]
            .split("\nfn connect_link_clicks(")
            .next()
            .expect("open_link_safely body not found");
        assert!(
            body.contains("sanitize_url"),
            "the opener must re-validate the URL even though the span map only holds sanitized ones"
        );
        assert!(
            body.contains("http://") && body.contains("https://"),
            "the opener must allow exactly http:// and https://"
        );
        // Match the `starts_with(` call form, not bare words: the doc comment
        // above the function discusses file:/mailto: (which is why they are
        // refused), so a bare-substring check would match its own rationale.
        assert!(
            !body.contains("starts_with(\"file") && !body.contains("starts_with(\"mailto"),
            "the opener must not pass file:/mailto: URLs to a browser"
        );
        assert!(
            body.contains("xdg-open"),
            "the opener must delegate to xdg-open (no shell, URL as one argv element)"
        );
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
        let (vpos, vmax, page) =
            scrolled_window_range(&sw).expect("a ScrolledWindow must report a scroll position");
        assert!(vpos >= 0.0, "a scroll position must not be negative, got {vpos}");
        assert!(page >= 0.0, "a viewport height must not be negative, got {page}");
        assert!(
            vmax >= vpos,
            "the scroll extent ({vmax}) must not be below the position ({vpos}): \
             follow-the-tail compares value + page against upper to tell 'at the bottom' from \
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
    fn picker_width_fits_the_current_model_label() {
        // The picker is pinned to just the current model ("provider/model"),
        // not to the longest catalog id and not to a fixed 220px: GTK3 sizes
        // a ComboBoxText to its longest item, so either extreme leaves a fat
        // box around a short label and steals Entry space. Pure math — no
        // display needed.
        assert!(picker_width_for("a/b") < picker_width_for("acme/big-model-name"), "wider label must request more width");
        assert!(picker_width_for("ollama-cloud/deepseek-v4.1-flash") <= 420, "long ids clamp instead of stretching the row");
        assert!(picker_width_for("x") >= 80, "even a tiny label keeps a usable box");
    }

    #[test]
    fn expand_flags_are_set_before_pack_so_the_pane_fills_the_window() {
        // Blank-window guard: GTK3's gtk_box_pack_start snapshots the child's
        // expand/fill flags AT PACK TIME. Setting hexpand/vexpand *after*
        // append/pack leaves the packed copy non-expanding, so the scroller,
        // the prompt row, the session box and the tab container all keep
        // their minimum size and the window is mostly blank space. This pins
        // the fixed order — flags before append — at every site in the
        // non-test code. A display is not needed: the order is a source
        // property, and the shipped binary is built from this source.
        //
        // NOTE: do not strip `#[cfg(test)]` modules here (the helper used
        // elsewhere cuts at the FIRST one, which sits at line ~708 — long
        // before `spawn_session`/`run`). The pack sites live after it, and
        // the test bodies never contain these exact pack strings, so
        // searching the whole file is both safe and required.
        let src = include_str!("gui.rs");
        for (site, pack, flags) in [
            // spawn_session: the conversation scroller claims the free space.
            ("session scroller", "vbox.append(&scroller);", "scroller.set_vexpand(true);"),
            // spawn_session: the prompt row stretches across the pane.
            ("prompt row", "vbox.append(&prompt_row);", "prompt_row.set_hexpand(true);"),
            // spawn_session: the session box fills its tab panel.
            ("session box", "panel.append(&vbox);", "vbox.set_vexpand(true);"),
            // run: the tab container fills the content box. (The root box
            // itself needs no flags: window.set_child is not a pack.)
            ("tab container", "content.append(&tabs);", "tabs.set_vexpand(true);"),
            // spawn_session: the Send button must be PACKED before the pane
            // is revealed — "Session" (built pre-present) shows it either
            // way, but every later tab ("Session 1", ...) is revealed only
            // by the explicit vbox.show_all(), so a Send appended after that
            // stays hidden while Session's shows.
            ("send button", "vbox.show_all();", "prompt_row.append(&send_btn);"),
        ] {
            let pack_at = src.find(pack).unwrap_or_else(|| panic!("{site}: pack site {pack:?} not found"));
            let flags_at = src.find(flags).unwrap_or_else(|| panic!("{site}: expand flag {flags:?} not found"));
            assert!(
                flags_at < pack_at,
                "{site}: {flags:?} must come BEFORE {pack:?} (GTK3 snapshots expand at pack time)"
            );
        }
    }

    #[test]
    fn the_sidebar_carries_a_new_project_button() {
        // G7 (display): the button must exist as a real widget with the exact
        // operator-facing label. It is the discoverable entry point for the
        // whole New Project feature, so a rename or a wiring regression should
        // fail here rather than as a "the button disappeared" report.
        let Some((app, _guard)) = try_app() else { return };
        let Ok(btn) = app.create_button("New Project") else { return };
        assert_eq!(btn.get_text().as_deref(), Some("New Project"));
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

    #[test]
    fn frozen_sync_holds_the_pane_and_rearming_catches_up() {
        // S2/S3 (display): while the user is scrolled away (`follow` off), a
        // sync must NOT append — GTK pins the pane to the bottom on every
        // append, so appending would yank a mid-read user back down. New lines
        // wait in the model; re-arming follow repaints the full tail.
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
        s.push(ConvKind::User, "first");
        sync_textview(&w, &mut s);
        let frozen = tv.get_text().unwrap_or_default();
        assert!(frozen.contains("first"), "live sync must paint: {frozen:?}");
        // Scroll away: freeze. New output must not reach the buffer…
        s.follow = false;
        s.push(ConvKind::User, "second");
        sync_textview(&w, &mut s);
        let held = tv.get_text().unwrap_or_default();
        assert_eq!(held, frozen, "frozen sync must hold the pane, got {held:?}");
        // …until follow re-arms, when the live path catches up on its own
        // (`rendered` is just a cursor into the model).
        s.follow = true;
        sync_textview(&w, &mut s);
        let caught = tv.get_text().unwrap_or_default();
        assert!(caught.contains("second"), "re-armed sync must catch up: {caught:?}");
    }

    #[test]
    fn keyboard_scroll_paints_the_windowed_slice() {
        // S6 (display): PageUp repaints the buffer with the scrolled window —
        // the only programmatic scroll the toolkit allows (no
        // `gtk_adjustment_set_value`). End returns to the live tail.
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
        s.scroll.page = 10;
        for i in 0..50 {
            s.push(ConvKind::System, &format!("line {i:02}"));
        }
        sync_textview(&w, &mut s);
        assert!(
            tv.get_text().unwrap_or_default().contains("line 49"),
            "live sync must show the tail"
        );
        scroll_page_up(&mut s);
        sync_textview(&w, &mut s);
        let win = tv.get_text().unwrap_or_default();
        assert!(!win.contains("line 49"), "the window must exclude the tail: {win:?}");
        // 50 lines, page 10: tail offset 40, one page up => lines 30..39.
        assert!(win.contains("line 30"), "one page above the tail: {win:?}");
        assert!(win.contains("line 39"), "one page above the tail: {win:?}");
        scroll_end(&mut s);
        sync_textview(&w, &mut s);
        assert!(
            tv.get_text().unwrap_or_default().contains("line 49"),
            "End must return to the live tail"
        );
    }
}

#[cfg(all(test, feature = "gui-rich-text"))]
mod markup_demo_tests {
    //! `/markup_demo` regression tests: the assistant block must reach the pane
    //! as ONE Pango unit.
    //!
    //! The bug these pin: `sync_textview` used to render each assistant line on
    //! its own, so every multi-line markdown construct was split. On the demo
    //! sample the table came out as four literal `| … |` rows (no grid), each
    //! code body line got its own ```` ``` ```` fence, the opening `fn main() {`
    //! was left unhighlighted, and a blockquote opened a span on one line and
    //! closed it on the next (invalid markup — GTK inserts nothing for that).
    use super::*;

    /// One JSONL session-log line: an assistant message with a single text
    /// block — the exact shape `drain_session_log` parses.
    fn jsonl_assistant_line(text: &str) -> String {
        let v = serde_json::json!({
            "role": "assistant",
            "blocks": [{ "type": "text", "text": text }],
        });
        format!("{v}\n")
    }

    fn pane() -> Option<(App, TextView, GuiWidgets, std::sync::MutexGuard<'static, ()>)> {
        let (app, guard) = gtk_test_app()?;
        let tv = app.create_textview().ok()?;
        configure_conversation_textview(&tv);
        let status = app.create_label("x").ok()?;
        let hint = app.create_label("").ok()?;
        let scroller = app.create_scrolled_window().ok()?;
        let w = GuiWidgets { textview: tv.clone(), status, scroller, hint };
        Some((app, tv, w, guard))
    }

    #[test]
    fn the_demo_table_reaches_the_pane_as_one_grid() {
        // The reported bug. Before the fix the pane showed `| Language | … |`
        // as four separate lines; now the whole table is one monospace grid.
        let Some((_app, tv, w, _guard)) = pane() else { return };
        let mut s = GuiState::new();
        drain_session_text(&jsonl_assistant_line(crate::MARKUP_DEMO), &mut s);
        sync_textview(&w, &mut s);
        let got = tv.get_text().unwrap_or_default();
        assert!(
            got.contains("┌──────────┬──────────┬─────────┐"),
            "the table must render as a box-drawing grid, not raw `|` rows: {got:?}"
        );
        // Alignment: every grid line carries the SAME assistant prefix, so the
        // borders and the rows beneath them start in the same column. Emitting
        // the prefix once per block (the old bug) shifted only the top border.
        // Only box-drawing lines count — a blockquote also uses `│` but is not
        // part of the grid (it has no second grid char on the line).
        let is_grid_line = |l: &str| {
            let t = l.trim_start();
            t.starts_with(['┌', '├', '└'])
                || (t.starts_with('│') && t.chars().skip(1).any(|c| c == '│' || c == '┐'))
        };
        let table: Vec<&str> = got.lines().filter(|l| is_grid_line(l)).collect();
        assert_eq!(table.len(), 6, "the whole grid must be present: {got:?}");
        let col_of = |l: &str| l.find(['┌', '├', '└', '│']).unwrap_or(0);
        let col = table.iter().map(|l| col_of(l)).max().unwrap_or(0);
        for l in &table {
            assert_eq!(
                col_of(l),
                col,
                "every table line must start in the same column (got {l:?}, expected col {col}): {got:?}"
            );
        }
        assert!(
            !got.lines().any(|l| l.trim_start().starts_with("| Language")),
            "no raw markdown table row may survive into the pane: {got:?}"
        );
        // The write cursor deliberately parks on the trailing block (see
        // `live_boundary`), repainted whole on the next sync — so a finished
        // drain may leave at most that one block unmarked. What must hold is
        // that the cursor never runs past the model and that nothing is
        // painted twice (each table line appears exactly once).
        assert!(
            s.rendered <= s.conv.len(),
            "the cursor must never pass the model: {} > {}",
            s.rendered,
            s.conv.len()
        );
        for needle in ["│ Rust     │", "│ JS       │", "┌──────────┬──────────┬─────────┐"] {
            assert_eq!(
                got.matches(needle).count(),
                1,
                "each grid line must be painted exactly once ({needle:?}): {got:?}"
            );
        }
    }

    #[test]
    fn the_demo_code_fence_is_one_block() {
        // Each body line used to be its own one-line fence (` ``` ` /
        // `  let xs …` / ` ``` `), and the fence's first line was rendered
        // outside the code span. A whole-document render puts the opening
        // brace inside the fenced block.
        let Some((_app, tv, w, _guard)) = pane() else { return };
        let mut s = GuiState::new();
        drain_session_text(&jsonl_assistant_line(crate::MARKUP_DEMO), &mut s);
        sync_textview(&w, &mut s);
        let got = tv.get_text().unwrap_or_default();
        // Three fenced blocks (rust/js/json); the opening fence carries its
        // language (` ```rust `), so only the closing fences are bare.
        let fences = got.lines().filter(|l| l.trim() == "```").count();
        assert_eq!(fences, 3, "one closing fence per fenced block (rust/js/json): {got:?}");
        // Indentation inside the fence survives (the block is not re-wrapped
        // line by line), and the opening brace is part of the code block.
        assert!(got.contains("fn main() {"), "the block's first line must render: {got:?}");
        assert!(got.contains("    let xs = vec![1, 2, 3];"), "indentation must survive: {got:?}");
    }

    #[test]
    fn blockquote_markup_is_balanced() {
        // A per-line render emitted `<span …>` on one line and `</span>` on the
        // next: invalid Pango, which GTK rejects wholesale.
        let raw = "> a blockquote, because models love a good aside.";
        let mut s = GuiState::new();
        drain_session_text(&jsonl_assistant_line(raw), &mut s);
        let markup = assistant_block_markup(ConvKind::Assistant, &s.conv);
        let opens = markup.matches("<span").count();
        let closes = markup.matches("</span>").count();
        assert_eq!(opens, closes, "unbalanced spans are rejected by GTK: {markup:?}");
    }

    #[test]
    fn a_link_paragraph_renders_whole() {
        let Some((_app, tv, w, _guard)) = pane() else { return };
        let raw = "See our [GitHub page](https://github.com/gmatht/pir) for more info.";
        let mut s = GuiState::new();
        drain_session_text(&jsonl_assistant_line(raw), &mut s);
        sync_textview(&w, &mut s);
        let got = tv.get_text().unwrap_or_default();
        assert!(got.contains("See our GitHub page (https://github.com/gmatht/pir)"), "{got:?}");
        assert!(!got.contains("[GitHub page]"), "link syntax must not survive: {got:?}");
    }

    #[test]
    fn a_streaming_block_is_not_painted_twice() {
        // The model grows one drained line at a time; a block is only whole
        // once its blank-line terminator arrives. Until then the live sync must
        // show each block exactly once — not a partial copy per drain tick.
        //
        // "Exactly once" only means anything for text that has ALREADY
        // streamed: on the first iteration only `## Heading` exists, so
        // asserting `After.` is present then is unsatisfiable by any
        // implementation (the old version of this test did, and failed on
        // iteration 1 for that reason alone).
        let Some((_app, tv, w, _guard)) = pane() else { return };
        let raw = "## Heading\n\n- one\n- two\n\nAfter.";
        let mut s = GuiState::new();
        let mut streamed: Vec<&str> = Vec::new();
        for line in raw.split('\n') {
            s.push(ConvKind::Assistant, line);
            streamed.push(line);
            sync_textview(&w, &mut s);
            let got = tv.get_text().unwrap_or_default();
            // Anything already streamed and non-blank must appear exactly once —
            // no duplicate, no partial-then-whole double paint.
            for needle in ["Heading", "- one", "- two", "After."] {
                if !streamed.iter().any(|l| l.contains(needle)) {
                    continue;
                }
                assert_eq!(
                    got.matches(needle).count(),
                    1,
                    "'{needle}' streamed already and must be painted exactly once, got {got:?}"
                );
            }
        }
        // And the finished document is complete: the list rendered, the
        // paragraph after it rendered.
        let got = tv.get_text().unwrap_or_default();
        assert!(got.contains("Heading") && got.contains("two") && got.contains("After."), "{got:?}");
    }

}

// ---------------------------------------------------------------------------
// G7: the /menu dialog covers every terminal menu action (the topbar items
// had no dispatch — `/menu` answered "unknown command").
// ---------------------------------------------------------------------------
#[cfg(test)]
mod gui_menu_tests {
    use super::*;
    /// Display-backed pane harness for the menu behavior tests: a real
    /// TextView + widgets, skipped gracefully headless. Mirrors the `pane()`
    /// helper in the markup tests (private there) so menu behavior is pinned
    /// against the same sync path the user sees.
    fn menu_pane() -> Option<(App, TextView, GuiWidgets, std::sync::MutexGuard<'static, ()>)> {
        let (app, guard) = gtk_test_app()?;
        let tv = app.create_textview().ok()?;
        configure_conversation_textview(&tv);
        let status = app.create_label("x").ok()?;
        let hint = app.create_label("").ok()?;
        let scroller = app.create_scrolled_window().ok()?;
        let w = GuiWidgets { textview: tv.clone(), status, scroller, hint };
        Some((app, tv, w, guard))
    }

    /// A `SessionView` over a fresh pane and no agent: enough for the read-only
    /// menu rows (about/help), which only touch the pane. The window is a real
    /// (unpresented) one because the struct carries it; those rows never use it.
    fn menu_view(app: &App, w: &GuiWidgets) -> (SessionView, AgentSlot) {
        let slot: AgentSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let state = std::sync::Arc::new(std::sync::Mutex::new(GuiState::new()));
        let (done_tx, _done_rx) = smol::channel::bounded::<()>(1);
        let sv = SessionView {
            state,
            widgets: w.clone(),
            entry: app.create_entry().expect("create_entry"),
            window: app.create_window().expect("create_window"),
            agent_slot: slot.clone(),
            fg_cancel: Arc::new(AtomicBool::new(false)),
            fg_quiet: Arc::new(AtomicBool::new(false)),
            hist_nav: std::sync::Arc::new(std::sync::Mutex::new(HistoryNav::default())),
            done_tx,
            app: app.clone(),
        };
        (sv, slot)
    }

    #[test]
    fn menu_about_and_help_paint_into_the_pane() {
        // `/menu about` and `/menu help` are read-only: they must paint into
        // the pane without an agent and without a display failure.
        let Some((app, tv, w, _guard)) = menu_pane() else { return };
        let (sv, slot) = menu_view(&app, &w);
        let app2 = app.clone();
        run_menu_action("about", &[], &slot, &[], &app2, &sv);
        run_menu_action("help", &[], &slot, &[], &app2, &sv);
        sync_textview(&w, &mut sv.state.lock().unwrap());
        let got = tv.get_text().unwrap_or_default();
        assert!(got.contains("pir ") && got.contains("license: GPL-3.0"), "about must print version + license: {got:?}");
        assert!(got.contains("/sessions"), "help must print the command list: {got:?}");
    }

    #[test]
    fn menu_unknown_subcommand_is_an_error_not_a_panic() {
        // A typo'd subcommand (`/menu frobnicate`, or a stale topbar arg)
        // must be a pane error listing the valid subs — never a panic, never
        // silent.
        let Some((app, tv, w, _guard)) = menu_pane() else { return };
        let (sv, slot) = menu_view(&app, &w);
        let app2 = app.clone();
        run_menu_action("frobnicate", &[], &slot, &[], &app2, &sv);
        sync_textview(&w, &mut sv.state.lock().unwrap());
        let got = tv.get_text().unwrap_or_default();
        assert!(got.contains("unknown menu action"), "got: {got:?}");
        assert!(got.contains("resume"), "the error must list valid subs: {got:?}");
    }

    #[test]
    fn menu_dialog_lists_every_terminal_menu_action() {
        // The terminal `/menu` offers eleven actions (modal::MenuAction minus
        // None); the GUI dialog must offer all eleven, or the topbar/dialog
        // silently drops something the terminal user can reach.
        let subs: Vec<&str> = MENU_DIALOG_ACTIONS.iter().map(|(s, _)| *s).collect();
        assert_eq!(
            subs,
            vec![
                "resume",
                "backgrounded",
                "model",
                "thinking",
                "security",
                "settings",
                "worktrees",
                "coverage",
                "help",
                "about",
                "quit",
            ],
            "MENU_DIALOG_ACTIONS drifted from modal::MenuAction: {subs:?}"
        );
        // Every row has a label, and labels are unique — two rows sharing a
        // label is a menu that lies about how many things there are.
        let mut labels: Vec<&str> =
            MENU_DIALOG_ACTIONS.iter().map(|(_, l)| *l).collect();
        labels.sort();
        labels.dedup();
        assert_eq!(
            labels.len(),
            MENU_DIALOG_ACTIONS.len(),
            "duplicate menu labels"
        );
        // And every sub resolves back to its own label (the dispatch table is
        // total over the dialog table by construction).
        for (sub, label) in MENU_DIALOG_ACTIONS {
            assert_eq!(menu_action_label(sub), Some(*label));
        }
        assert_eq!(menu_action_label("nope"), None);
    }

    #[test]
    fn every_menu_action_has_a_topbar_entry() {
        // The operator's report — "there isn't really an equivalent to all the
        // /menu options in topbar" — pinned where it is checkable: the `/menu`
        // strip must be built from `MENU_DIALOG_ACTIONS`, in order, and run the
        // shared dispatcher. Building the strip needs a presented window, but
        // the property is structural (which table the loop walks), so reading
        // the source is a faithful check of what the UI does.
        let src = include_str!("gui.rs");
        let start = src
            .find("\nfn build_menu_strip(")
            .expect("build_menu_strip not found in gui.rs")
            + 1;
        let body = &src[start..];
        let body = &body[..body.find("\n}\n").expect("build_menu_strip body")];
        assert!(
            body.contains("for (sub, label) in MENU_DIALOG_ACTIONS"),
            "the /menu strip must be built from MENU_DIALOG_ACTIONS, not a \n\
             hand-kept copy of it — that copy is what drifts"
        );
        assert!(
            body.contains("run_menu_action("),
            "a strip item must run run_menu_action, the same path as /menu <sub>"
        );
        // And it is part of the window, not merely constructed.
        assert!(
            src.contains("build_menu_strip(&app, &sessions, &tabs, providers, agent_slot)"),
            "the /menu strip is never added to the window"
        );
        // The menu bar keeps the categorised view of the same commands, so the
        // whole menu must stay reachable there as well.
        let args: Vec<&str> = MENU_ACTIONS
            .iter()
            .filter(|a| a.command == "menu")
            .map(|a| a.arg)
            .collect();
        assert!(
            args.contains(&""),
            "no menu-bar item opens the whole menu dialog: {args:?}"
        );
        assert!(
            MENU_ACTIONS.iter().any(|a| a.command == "help"),
            "the /menu help action must stay reachable via the Commands item"
        );
        assert!(
            MENU_ACTIONS.iter().any(|a| a.command == "exit"),
            "the /menu quit action must stay reachable via the Quit item"
        );
    }

    #[test]
    fn menu_arm_dispatches_every_dialog_row() {
        // The `/menu` arm routes through `run_menu_action`, and every dialog
        // row must be a subcommand it accepts — otherwise the dialog offers a
        // button that answers "unknown menu action". `help`/`about` run
        // inline here; picker subcommands need a display and are covered by
        // the arm's direct-call parity with the topbar (same function).
        let slot: AgentSlot = Arc::new(Mutex::new(None));
        let state = Arc::new(Mutex::new(GuiState::new()));
        let nav: Arc<Mutex<HistoryNav>> = Arc::new(Mutex::new(HistoryNav::default()));
        // No display needed: unknown-sub handling is pure pane output, but it
        // still needs widgets for sync_textview — covered by the pane tests
        // below. Here, pin the table-to-dispatch contract without widgets:
        for (sub, _) in MENU_DIALOG_ACTIONS {
            assert!(menu_action_label(sub).is_some(), "no label for {sub}");
        }
        assert_eq!(menu_action_label("nope"), None);
        let _ = (&slot, &state, &nav);
    }

    #[test]
    fn strip_ansi_removes_sgr_escapes_but_keeps_text() {
        // The pane has no ANSI renderer: escapes would show up as literal
        // junk, so resumed banners and coverage reports are stripped first.
        assert_eq!(strip_ansi("\x1b[1mbold\x1b[0m plain"), "bold plain");
        assert_eq!(strip_ansi("\x1b[36mcyan\x1b[0m"), "cyan");
        assert_eq!(strip_ansi("no escapes here"), "no escapes here");
        assert_eq!(strip_ansi(""), "");
        // A bare ESC not followed by `[` is text, not an escape.
        assert_eq!(strip_ansi("a\x1bb"), "a\x1bb");
    }

    #[test]
    fn coverage_plan_lines_name_the_tools() {
        // The pane-native coverage report names the same tools the terminal
        // report would run, with install hints for the missing ones.
        use crate::lints::{CoverageStatus, CoverageTool, MissingCoverage, SelectedCoverage};
        let st = CoverageStatus {
            available: vec![SelectedCoverage {
                lang: "rust",
                detected_by: "Cargo.toml".to_string(),
                chosen: vec![CoverageTool {
                    tool: "cargo-tarpaulin",
                    purpose: "branch coverage",
                    command: "cargo tarpaulin",
                    install: "cargo install cargo-tarpaulin",
                }],
                extras: vec![],
            }],
            missing: vec![MissingCoverage {
                lang: "python",
                detected_by: "*.py".to_string(),
                candidates: vec![CoverageTool {
                    tool: "coverage.py",
                    purpose: "branch coverage",
                    command: "coverage run",
                    install: "pip install coverage",
                }],
            }],
        };
        let lines = coverage_plan_lines(&st);
        assert_eq!(lines.len(), 2, "got: {lines:?}");
        assert!(lines[0].contains("rust") && lines[0].contains("cargo-tarpaulin"), "got: {lines:?}");
        assert!(
            lines[1].contains("python") && lines[1].contains("pip install coverage"),
            "got: {lines:?}"
        );
        // Empty coverage: no lines, no crash (mirrors report_coverage_plan's
        // early return).
        let empty = CoverageStatus { available: vec![], missing: vec![] };
        assert!(coverage_plan_lines(&empty).is_empty());
    }

    #[test]
    fn settings_step_logic_cycles_and_toggles() {
        // The settings dialog's step logic without a display: thinking cycles
        // through exactly the offered levels and wraps; the rest toggle.
        let levels = vec!["off".to_string(), "high".to_string(), "max".to_string()];
        let (mut t, mut s, mut i, mut ss, mut f) =
            ("off".to_string(), false, false, false, false);
        step_gui_setting(&levels, &mut t, &mut s, &mut i, &mut ss, &mut f, 0);
        assert_eq!(t, "high");
        step_gui_setting(&levels, &mut t, &mut s, &mut i, &mut ss, &mut f, 0);
        assert_eq!(t, "max");
        step_gui_setting(&levels, &mut t, &mut s, &mut i, &mut ss, &mut f, 0);
        assert_eq!(t, "off", "thinking cycles with wraparound");
        step_gui_setting(&levels, &mut t, &mut s, &mut i, &mut ss, &mut f, 1);
        assert!(s);
        step_gui_setting(&levels, &mut t, &mut s, &mut i, &mut ss, &mut f, 4);
        assert!(f);
        step_gui_setting(&levels, &mut t, &mut s, &mut i, &mut ss, &mut f, 9);
        // Unknown rows are ignored, never panic.
    }

    #[test]
    fn security_cycle_wraps_around_the_options() {
        // Enum rows cycle forward with wraparound; an unknown current value
        // restarts at the first option rather than panicking.
        assert_eq!(cycle_security_value("on", &["on".to_string(), "off".to_string()]), "off");
        assert_eq!(cycle_security_value("off", &["on".to_string(), "off".to_string()]), "on");
        assert_eq!(
            cycle_security_value("???", &["ask".to_string(), "auto-yes".to_string()]),
            "auto-yes"
        );
    }

    // -- Sidebar status icon (running / stopped-unseen / stopped-seen) -------

    /// The settle edge: a resting session is `○` until it is *seen* to run,
    /// then amber `●` the moment it comes to rest, and `○` again — for good —
    /// once acknowledged. Uses the same `sidebar_row_status` the sidebar tick
    /// calls, so this asserts the value the widget pass actually paints.
    #[test]
    fn sidebar_icon_settles_from_running_to_unseen_then_seen() {
        use crate::session_tree::TreeStatus;
        let mut map = std::collections::HashMap::new();
        let status = |m: &mut std::collections::HashMap<String, TreeStatus>, active: bool| {
            sidebar_row_status(m, "s1", active)
        };
        // A session that has never been seen running is settled, not unread:
        // opening the GUI onto finished sessions must not paint amber rows.
        assert_eq!(status(&mut map, false), TreeStatus::StoppedSeen, "first sight, at rest");
        // It starts working -> running.
        assert_eq!(status(&mut map, true), TreeStatus::Running, "work in flight");
        // It comes to rest in the background -> stopped, unseen.
        assert_eq!(status(&mut map, false), TreeStatus::StoppedUnseen, "settled unseen");
        // ...and *stays* amber on later ticks (the naive signal says "stopped"
        // every tick; only looking at it may settle the row).
        assert_eq!(status(&mut map, false), TreeStatus::StoppedUnseen, "sticky until seen");
        // The operator selects it.
        let mut side = SidebarState { tree_status: map.clone(), ..SidebarState::default() };
        side.acknowledge("s1");
        map = side.tree_status;
        assert_eq!(status(&mut map, false), TreeStatus::StoppedSeen, "seen -> settled");
        // A running session is not settled by looking at it.
        assert_eq!(status(&mut map, true), TreeStatus::Running);
        side = SidebarState { tree_status: map, ..SidebarState::default() };
        side.acknowledge("s1");
        assert_eq!(
            side.tree_status.get("s1").copied(),
            Some(TreeStatus::Running),
            "acknowledging a running session must not mark it stopped"
        );
    }

    /// Acknowledging settles only the session named, and clears the cached
    /// signature so the next tick repaints the icon even if nothing else moved.
    #[test]
    fn sidebar_acknowledge_is_per_session_and_forces_a_repaint() {
        use crate::session_tree::TreeStatus;
        let mut side = SidebarState::default();
        side.tree_status.insert("a".into(), TreeStatus::StoppedUnseen);
        side.tree_status.insert("b".into(), TreeStatus::StoppedUnseen);
        side.signature = "cached".into();
        side.acknowledge("a");
        assert_eq!(side.tree_status["a"], TreeStatus::StoppedSeen, "a settles");
        assert_eq!(side.tree_status["b"], TreeStatus::StoppedUnseen, "b stays amber");
        assert!(side.signature.is_empty(), "signature cleared => icon repaints");
        // Acknowledging the same session twice is a no-op, not a repaint storm.
        side.signature = "cached-again".into();
        side.acknowledge("a");
        assert_eq!(side.signature, "cached-again");
    }

    /// The icon label is built from the glyph + colour pair of the status the
    /// row is in — the exact markup string `sync_sidebar` hands to the Label.
    #[test]
    fn sidebar_icon_markup_carries_the_status_glyph_and_colour() {
        use crate::session_tree::TreeStatus;
        let markup = |st: TreeStatus| {
            format!(
                "<span foreground=\"{}\" size=\"x-large\">{}</span>",
                st.color(),
                escape_markup(st.glyph())
            )
        };
        let running = markup(TreeStatus::Running);
        let unseen = markup(TreeStatus::StoppedUnseen);
        let seen = markup(TreeStatus::StoppedSeen);
        assert!(running.contains('▶') && unseen.contains('●') && seen.contains('○'));
        assert_ne!(running, unseen);
        assert_ne!(unseen, seen);
        assert_ne!(running, seen);
        for (st, colour) in [
            (TreeStatus::Running, TreeStatus::Running.color()),
            (TreeStatus::StoppedUnseen, TreeStatus::StoppedUnseen.color()),
            (TreeStatus::StoppedSeen, TreeStatus::StoppedSeen.color()),
        ] {
            assert!(markup(st).contains(colour), "colour must be the status's own");
        }
    }

    /// The status icon sits LEFT of the session title, not above it. Icon +
    /// button packed as siblings into the vertical rows box stack (icon
    /// above, button below); wrapping both in a horizontal row box keeps
    /// them on one line with the icon left. A display is not needed: the
    /// packing is a source property, and the shipped binary is built from
    /// this source (same shape as the expand-flags order test above).
    #[test]
    fn sidebar_rows_pack_icon_and_button_side_by_side() {
        let src = include_str!("gui.rs");
        let mk = src
            .find("create_box sidebar row")
            .expect("a horizontal row box must wrap each session row");
        let icon = src
            .find("row_box.append(&icon_label);")
            .expect("icon packed into the row box");
        let btn = src
            .find("row_box.append(&btn);")
            .expect("button packed into the row box");
        let pack = src
            .find("rows_box.append(&row_box);")
            .expect("row box packed into the rows box");
        assert!(
            mk < icon && icon < btn && btn < pack,
            "icon then button into a horizontal row box, row box into the rows box"
        );
        // No session row may pack the icon straight into the vertical rows
        // box — that is the above-not-left stacking this replaces. Built by
        // concat so this very assertion does not itself contain the needle.
        let stacked = ["rows_box.append(&", "icon_label);"].concat();
        assert!(
            !src.contains(&stacked),
            "icon must not be a direct child of the vertical rows box (it stacks above the button)"
        );
    }
}
