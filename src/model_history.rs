//! Most-recently-used model history for `/model`.
//!
//! A bare `/model` offers the [`TOP_N`] most recently used models instead of
//! the whole catalog; `/model PREFIX` narrows that to history entries starting
//! with PREFIX. History persists at `<pi_dir>/agent/model-history.json` as a
//! most-recent-first `[{label, last_used}]` list (epoch seconds), capped at
//! [`MAX_ENTRIES`] so the file stays trivial.
//!
//! Labels are `provider/model` (see `Provider::label`), the same string the
//! picker highlights and `config::select` resolves, so a history entry always
//! maps back without a second lookup scheme.
//!
//! All writes are best-effort and silent: switching models must never fail
//! because telemetry couldn't persist. The `fake` provider (tests, puppetry)
//! is never recorded, so unit tests can't pollute a real history.

use serde::{Deserialize, Serialize};

/// How many recent models `/model` offers.
pub const TOP_N: usize = 5;
/// Cap on stored entries (only the head ever matters).
pub const MAX_ENTRIES: usize = 50;
/// Provider id that is never recorded (test/puppetry stand-in).
const SKIP_PROVIDER: &str = "fake";

/// One history entry: what was used, and when (epoch seconds).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub label: String,
    pub last_used: u64,
}

/// History file location.
pub fn path() -> std::path::PathBuf {
    crate::config::pi_dir().join("agent").join("model-history.json")
}

/// Load the history, most-recent-first. A missing or corrupt file is an empty
/// history, not an error (a fresh user simply has none yet).
pub fn load() -> Vec<Entry> {
    load_from(&path())
}

/// [`load`] from an explicit path (pure core for tests: no env involved).
pub(crate) fn load_from(path: &std::path::Path) -> Vec<Entry> {
    let mut entries: Vec<Entry> = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<Entry>>(&s).ok())
        .unwrap_or_default();
    entries.retain(|e| !e.label.trim().is_empty());
    entries.sort_by_key(|e| std::cmp::Reverse(e.last_used));
    entries.truncate(MAX_ENTRIES);
    entries
}

/// Record a use of `provider/model-id` as now (dedupe + truncate + persist).
/// Silent on any failure; the `fake` provider is skipped outright.
pub fn record(provider_pid: &str, model_id: &str) {
    record_to(&path(), provider_pid, model_id, crate::term::epoch());
}

/// [`record`] into an explicit file at an explicit timestamp (pure core for
/// tests: no env, no clock).
pub(crate) fn record_to(path: &std::path::Path, provider_pid: &str, model_id: &str, now: u64) {
    if provider_pid.eq_ignore_ascii_case(SKIP_PROVIDER) {
        return;
    }
    let label = format!("{provider_pid}/{model_id}");
    let mut entries = load_from(path);
    entries.retain(|e| e.label != label);
    entries.insert(0, Entry { label, last_used: now });
    entries.truncate(MAX_ENTRIES);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, serde_json::to_string_pretty(&entries).unwrap_or_default());
}

/// Command words that take a model selector: the same set the rustyline
/// completer/hinter, `config::complete_model_buffer`, and the TUI all match.
/// Shared here so the pre-Enter picker and those paths can never disagree
/// about what counts as "a `/model` line".
pub const MODEL_COMMANDS: [&str; 4] = ["/model", "/m", "/default-model", "/dm"];

/// Split an idle line into (`command word`, `argument prefix`) when it is a
/// `/model`-family command, `None` otherwise. The prefix is the raw text after
/// the command word (leading blanks stripped, interior kept) — the same string
/// [`filter_ranked`] narrows on, so the picker and the post-Enter `select`
/// path always filter identically.
///
/// A numeric suffix (`/model2`, `/m3`) counts as a model line too, with an
/// *empty* prefix: the digits are the row key, not a filter, so the list still
/// renders and Up/Down still step through it. It is recognised only via
/// [`parse_number`] (so `/models` and `/model*` keep declining).
///
/// Pure: `/models`, `/model*`, and plain prompts all decline, so callers can
/// use this as the single "is the inline picker active here" predicate.
pub fn parse_model_line(buf: &str) -> Option<(String, String)> {
    let trimmed = buf.trim_start();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    if !MODEL_COMMANDS.contains(&cmd) && parse_number(cmd).is_none() {
        return None;
    }
    let prefix = parts.next().unwrap_or("").trim_start().to_string();
    Some((cmd.to_string(), prefix))
}

/// Parse a `/model<N>` command word (`/model2`, `/m2`) into the 1-based row
/// number it names. The Nth entry of the top-[`TOP_N`] list is selected
/// *directly*, without printing it or reading a key — `/model2` is the second
/// model. Returns the number [`format_rows`] prints on that row, so callers
/// subtract one to index.
///
/// A leading `/` is optional: the REPL dispatches the command word *without*
/// it (`handle_command` strips the slash), while the line parser
/// ([`parse_model_line`]) sees it — accepting both keeps a single source of
/// truth for the mapping rather than two parsers that can drift.
///
/// Deliberately narrow so it cannot shadow an existing command or a real
/// selector:
///   * only the two short forms (`/model1`…, `/m1`…) are accepted — not
///     `/models`, `/model*`, `/default-model` or `/dm`, which are separate
///     commands;
///   * the suffix must be entirely digits (`/model1x` is *not* a number line,
///     it is a selector looking for a model named `1x`);
///   * `0` is rejected: the list is 1-based, so `/model0` is a typo, not "the
///     first" (an off-by-one that would silently pick a different model).
pub fn parse_number(cmd: &str) -> Option<usize> {
    let word = cmd.strip_prefix('/').unwrap_or(cmd);
    let digits = word.strip_prefix("model").or_else(|| word.strip_prefix('m'))?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // `digits` is non-empty and all-ASCII-digit, so this cannot fail; a huge
    // number saturates and is then rejected by the `0` check (it can never
    // address a five-row list anyway).
    let n: usize = digits.parse().ok()?;
    if n == 0 { None } else { Some(n) }
}

/// Render the picker as one line per candidate: `  ▸2 provider/model`. The
/// marker tracks `selected` (the row the arrows are on; `None` before the
/// first keypress) and the number is the key the user types (`/model2`). The
/// number is always the *position* in `labels` (+1), never a `:N` catalog
/// index, so the digits on screen and the digits in `/modelN` denote the same
/// thing.
///
/// Pure, so the initial paint and every highlight redraw are produced by the
/// same function and can be tested without a terminal.
pub fn format_rows(labels: &[String], selected: Option<usize>, current: &str) -> Vec<String> {
    labels
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let marker = if selected == Some(i) { "▸" } else { " " };
            let cur = if label == current { "  •current" } else { "" };
            format!("  {marker}{} {label}{cur}", i + 1)
        })
        .collect()
}

/// One Up/Down step of the inline picker, stateless: `candidates` are the
/// current top-N for the typed prefix (see [`filter_ranked`]), `arg` is what
/// the buffer holds now. When the buffer already holds exactly candidate `i`,
/// step to `i±1` (clamped at the ends, like the modal picker and history
/// recall); when it holds a partial prefix, jump to the first (Down) or last
/// (Up) entry — the standard closed-list behavior. Returns the newly selected
/// label, or `None` when there is nothing to pick.
///
/// Stateless on purpose: the selection *is* the buffer text, so Enter always
/// submits exactly what is on screen and no hidden index can desync.
pub fn step_pick(candidates: &[String], arg: &str, up: bool) -> Option<String> {
    if candidates.is_empty() {
        return None;
    }
    let idx = match candidates.iter().position(|c| c == arg) {
        Some(i) if up => i.saturating_sub(1),
        Some(i) => (i + 1).min(candidates.len() - 1),
        None if up => candidates.len() - 1,
        None => 0,
    };
    candidates.get(idx).cloned()
}

/// Whether Up/Down on this line should drive the inline `/model` picker
/// rather than history recall: only when the operator *typed* the line. A
/// history-recalled `/model` line (up-arrowed to, not typed) keeps navigating
/// history, so arrowing *onto* `/model foo` and pressing Up again reaches
/// older prompts instead of trapping in the picker.
///
/// Pure, so both front-ends (the rustyline `ModelPickHandler` and the TUI idle
/// loop) share one definition and can never disagree about when the arrows
/// are rewired. Callers pass the line up to the cursor, exactly as they do
/// for [`pick_next`].
pub fn arrows_drive_picker(line: &str, recalled: bool) -> bool {
    !recalled && parse_model_line(line).is_some()
}

/// One picker keypress, pure: given the idle line, the cursor byte offset,
/// and the current candidates, return the rebuilt buffer after an Up (`up =
/// true`) / Down step, or `None` when picking is inactive here (not a
/// `/model`-family line, caret still inside the command word, or no
/// candidates). The rustyline `ModelPickHandler` and the TUI idle loop both
/// call this with their own candidate source, so the two front-ends can never
/// disagree about what a step does.
pub fn pick_next(line: &str, pos: usize, candidates: &[String], up: bool) -> Option<String> {
    let (cmd, arg) = parse_model_line(&line[..pos.min(line.len())])?;
    if pos < cmd.len() {
        return None;
    }
    // A numbered line (`/model2`) already names its row: stepping it into
    // `/model2 a/x` would produce a selector that matches nothing. Leave it to
    // Enter, which resolves the number.
    if parse_number(&cmd).is_some() {
        return None;
    }
    let next = step_pick(candidates, &arg, up)?;
    Some(apply_pick(&cmd, &next))
}

/// Rebuild the idle buffer with the picked label, preserving the typed command
/// word (`/m` stays `/m`). The result is a complete submittable line, so the
/// picker never needs an "accept highlight" key: what you see is what Enter runs.
pub fn apply_pick(cmd: &str, label: &str) -> String {
    format!("{cmd} {label}")
}

/// Render the candidates as the single-line hint the idle prompt shows *before*
/// Enter: `1 a/x · ▸2 b/y · 3 c/z [↑↓ select]`. The entry matching `arg`
/// exactly wears the `▸` (after an Up/Down step the buffer holds the full
/// label, so the mark tracks the selection with no extra state); a partial
/// prefix marks nothing. Empty in → empty out (callers treat that as inactive).
pub fn format_options(candidates: &[String], arg: &str) -> String {
    if candidates.is_empty() {
        return String::new();
    }
    let sel = candidates.iter().position(|c| c == arg);
    let items: Vec<String> = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if Some(i) == sel {
                format!("▸{} {c}", i + 1)
            } else {
                format!("{} {c}", i + 1)
            }
        })
        .collect();
    format!("{} [↑↓ select]", items.join(" · "))
}

/// The [`TOP_N`] most-recent labels, narrowed to entries starting with
/// `prefix` (case-insensitive) when it is non-empty. A label matches when the
/// full `provider/model` label *or* the bare model id starts with it, so both
/// `/model opencode-go/mu` and `/model mu` find `opencode-go/muse-spark…`.
/// Pass `n` to override the take count (tests).
pub fn top_matching(prefix: &str, n: usize) -> Vec<String> {
    filter_ranked(&load(), prefix, n)
}

/// The sole recent model matching a typed `prefix`, when the prefix is
/// non-empty and narrows the history to exactly one entry — `None` otherwise
/// (blank prefix, no match, or several matches).
///
/// This is the rule behind `/model <prefix>` switching straight to a model
/// instead of opening a picker: a single candidate is not a choice to make,
/// so listing it (a "recent models" line plus a one-item picker) is pure
/// friction. Blank prefixes always keep the picker — a bare `/model` is a
/// browse, not a selection — and an ambiguous prefix must too, so the user can
/// still see which models it matched.
pub fn sole_match(prefix: &str) -> Option<String> {
    if prefix.trim().is_empty() {
        return None;
    }
    let matches = top_matching(prefix, TOP_N);
    if matches.len() == 1 {
        matches.into_iter().next()
    } else {
        None
    }
}

/// [`top_matching`] over an explicit entry list (pure for tests).
pub(crate) fn filter_ranked(entries: &[Entry], prefix: &str, n: usize) -> Vec<String> {
    let needle = prefix.trim().to_lowercase();
    let mut ranked: Vec<&Entry> = entries.iter().collect();
    ranked.sort_by_key(|e| std::cmp::Reverse(e.last_used));
    ranked
        .into_iter()
        .filter(|e| {
            if needle.is_empty() {
                return true;
            }
            let label = e.label.to_lowercase();
            if label.starts_with(&needle) {
                return true;
            }
            // Bare model id (`muse-spark…` in `opencode-go/muse-spark…`).
            e.label
                .rsplit('/')
                .next()
                .map(|mid| mid.to_lowercase().starts_with(&needle))
                .unwrap_or(false)
        })
        .take(n)
        .map(|e| e.label.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The up-arrow trap: a `/model` line the operator typed rewires the
    /// arrows to the picker, but a `/model` line that arrived via history
    /// recall (up-arrowed to, not typed) must keep navigating history — so
    /// arrowing *onto* `/model foo` and pressing Up again reaches older
    /// prompts instead of stepping through models.
    #[test]
    fn recalled_model_line_does_not_rewire_arrows() {
        // Typed lines (not recalled) rewire: bare, prefixed, and short form.
        assert!(arrows_drive_picker("/model", false));
        assert!(arrows_drive_picker("/model foo", false));
        assert!(arrows_drive_picker("/m foo", false));
        assert!(arrows_drive_picker("  /model foo", false));
        // Recalled lines never rewire — even a bare `/model`.
        assert!(!arrows_drive_picker("/model", true));
        assert!(!arrows_drive_picker("/model foo", true));
        assert!(!arrows_drive_picker("/m foo", true));
        // Not a model line: never rewired, typed or recalled.
        assert!(!arrows_drive_picker("hello", false));
        assert!(!arrows_drive_picker("hello", true));
        assert!(!arrows_drive_picker("", false));
        assert!(!arrows_drive_picker("/models", false));
    }

    fn entries() -> Vec<Entry> {
        // Deliberately out of recency order: ranking sorts by `last_used`.
        vec![
            Entry { label: "b/old".into(), last_used: 10 },
            Entry { label: "a/new".into(), last_used: 30 },
            Entry { label: "opencode-go/muse-spark-1.3-contributor".into(), last_used: 20 },
        ]
    }

    /// The `/model <prefix>` short-circuit relies on this: a typed prefix that
    /// narrows the history to exactly one entry is an unambiguous choice, so
    /// the caller switches to it instead of opening a one-item picker. A
    /// prefix matching several entries must NOT collapse (the picker stays),
    /// and a blank prefix must return the whole (capped) recent list.
    #[test]
    fn single_match_prefix_is_unambiguous() {
        let entries = vec![
            Entry { label: "a/one".into(), last_used: 10 },
            Entry { label: "b/two".into(), last_used: 20 },
            Entry { label: "c/three".into(), last_used: 30 },
        ];
        // Exactly one entry matches → len 1 (the caller picks it silently).
        assert_eq!(filter_ranked(&entries, "a/one", TOP_N), vec!["a/one"]);
        assert_eq!(filter_ranked(&entries, "three", TOP_N), vec!["c/three"]);
        // Several match → the picker must stay (len > 1).
        assert_eq!(filter_ranked(&entries, "", TOP_N).len(), 3);
        // None match → the caller falls through to the fuzzy `select` path.
        assert!(filter_ranked(&entries, "nope", TOP_N).is_empty());
    }

    /// `sole_match` is the single source of the "select it, don't list it"
    /// rule, so pin all three outcomes. The pure predicate is exercised
    /// against an explicit entry list (via `filter_ranked`) because
    /// `sole_match` itself reads the on-disk history; the two must agree on
    /// what "exactly one match" means.
    #[test]
    fn sole_match_requires_a_nonempty_unambiguous_prefix() {
        let entries = vec![
            Entry { label: "a/one".into(), last_used: 10 },
            Entry { label: "b/two".into(), last_used: 20 },
            Entry { label: "c/three".into(), last_used: 30 },
        ];
        // The predicate's three inputs, expressed on the pure core:
        //  - non-empty + one match  → pick silently
        assert_eq!(filter_ranked(&entries, "one", TOP_N).len(), 1);
        //  - blank                  → browse (picker)
        assert!(filter_ranked(&entries, "  ", TOP_N).len() > 1);
        //  - ambiguous              → picker
        assert!(filter_ranked(&entries, "t", TOP_N).len() > 1);
        // Blank never yields a sole match, regardless of the history contents.
        assert_eq!(sole_match(""), None);
        assert_eq!(sole_match("   "), None);
    }

    #[test]
    fn rank_is_most_recent_first_and_capped() {
        let top = filter_ranked(&entries(), "", TOP_N);
        assert_eq!(top, vec!["a/new", "opencode-go/muse-spark-1.3-contributor", "b/old"]);
        assert_eq!(filter_ranked(&entries(), "", 2).len(), 2, "take count honored");
    }

    #[test]
    fn prefix_matches_label_or_bare_model_id_case_insensitively() {
        // Full-label prefix.
        assert_eq!(
            filter_ranked(&entries(), "opencode-go/mu", TOP_N),
            vec!["opencode-go/muse-spark-1.3-contributor"]
        );
        // Bare model-id prefix.
        assert_eq!(
            filter_ranked(&entries(), "MUSE", TOP_N),
            vec!["opencode-go/muse-spark-1.3-contributor"]
        );
        // No match, and blank prefix means everything.
        assert!(filter_ranked(&entries(), "zzz", TOP_N).is_empty());
        assert_eq!(filter_ranked(&entries(), "  ", TOP_N).len(), 3);
    }

    #[test]
    fn picker_next_rebuilds_only_on_model_lines_with_candidates() {
        let cands = vec!["a/x".to_string(), "b/y".to_string()];
        let line = "/model a";
        assert_eq!(pick_next(line, line.len(), &cands, false), Some("/model a/x".to_string()));
        assert_eq!(pick_next(line, line.len(), &cands, true), Some("/model b/y".to_string()));
        // Caret inside the command word: inactive (history recall owns it).
        assert_eq!(pick_next(line, 3, &cands, false), None);
        // Not a model line, or no candidates: inactive.
        assert_eq!(pick_next("hello", 5, &cands, false), None);
        assert_eq!(pick_next(line, line.len(), &[], false), None);
        // A selected entry steps onward; the command word is preserved.
        assert_eq!(
            pick_next("/m a/x", "/m a/x".len(), &cands, false),
            Some("/m b/y".to_string())
        );
        // A numbered line already names its row: Up/Down must not rewrite it
        // into `/model2 a/x`, which would resolve against nothing.
        assert_eq!(pick_next("/model2", "/model2".len(), &cands, false), None);
        assert_eq!(pick_next("/model2", "/model2".len(), &cands, true), None);
        assert_eq!(pick_next("/m3", "/m3".len(), &cands, true), None);
    }

    /// The pre-Enter picker contract: what counts as a `/model` line, how
    /// Up/Down steps through the candidates, how the buffer is rebuilt, and
    /// how the options render. Pure (no env, no clock), so parallel
    /// prompt-building tests can never observe it.
    #[test]
    fn picker_parses_only_model_family_lines() {
        assert_eq!(
            parse_model_line("/model"),
            Some(("/model".to_string(), String::new()))
        );
        assert_eq!(
            parse_model_line("/model   ollama"),
            Some(("/model".to_string(), "ollama".to_string()))
        );
        assert_eq!(
            parse_model_line("  /m mu"),
            Some(("/m".to_string(), "mu".to_string()))
        );
        // `/models`, `/model*`, and plain prompts are NOT picker lines.
        assert_eq!(parse_model_line("/models"), None);
        assert_eq!(parse_model_line("/model* x"), None);
        assert_eq!(parse_model_line("hello"), None);
        assert_eq!(parse_model_line(""), None);
        assert_eq!(parse_model_line("/"), None);
    }

    #[test]
    fn picker_steps_through_candidates_with_clamping() {
        let cands = vec!["a/x".to_string(), "b/y".to_string(), "c/z".to_string()];
        // From a partial prefix: Down jumps to the first, Up to the last.
        assert_eq!(step_pick(&cands, "", false), Some("a/x".to_string()));
        assert_eq!(step_pick(&cands, "", true), Some("c/z".to_string()));
        assert_eq!(step_pick(&cands, "b", false), Some("a/x".to_string()));
        // From a selected entry: step one, clamped at the ends.
        assert_eq!(step_pick(&cands, "a/x", false), Some("b/y".to_string()));
        assert_eq!(step_pick(&cands, "a/x", true), Some("a/x".to_string()));
        assert_eq!(step_pick(&cands, "c/z", false), Some("c/z".to_string()));
        assert_eq!(step_pick(&cands, "b/y", true), Some("a/x".to_string()));
        // Empty list picks nothing; the index is the visible order.
        let empty: Vec<String> = Vec::new();
        assert_eq!(step_pick(&empty, "", false), None);
        assert_eq!(step_pick(&cands, "not-in-list", false), Some("a/x".to_string()));
    }

    #[test]
    fn picker_applies_and_formats() {
        // The rebuild keeps the typed command word...
        assert_eq!(apply_pick("/m", "a/x"), "/m a/x");
        assert_eq!(apply_pick("/model", "b/y"), "/model b/y");
        // ...and the result is always a submittable `/model` line.
        let line = apply_pick("/dm", "c/z");
        let (cmd, prefix) = parse_model_line(&line).expect("rebuilt line must parse");
        assert_eq!(cmd, "/dm");
        assert_eq!(prefix, "c/z");
        // The hint marks the selection and lists every candidate in order.
        let cands = vec!["a/x".to_string(), "b/y".to_string()];
        let shown = format_options(&cands, "b/y");
        assert!(shown.contains("▸2 b/y"), "selected entry must wear the marker: {shown:?}");
        assert!(shown.contains("1 a/x"), "unselected entries listed bare: {shown:?}");
        assert!(shown.contains("[↑↓ select]"), "affordance shown: {shown:?}");
        // A partial prefix marks nothing; empty in means inactive.
        let partial = format_options(&cands, "a");
        assert!(!partial.contains('▸'), "no marker on a partial prefix: {partial:?}");
        assert!(format_options(&Vec::new(), "").is_empty());
    }

    /// The number path of the requirement: `/model2` selects the second entry.
    /// `parse_number` is the single source of that mapping, so pin every shape
    /// it must accept and the shapes it must leave to the selector path.
    #[test]
    fn numbered_command_selects_by_position() {
        // Accepted: both spellings, any 1-based number.
        assert_eq!(parse_number("/model1"), Some(1));
        assert_eq!(parse_number("/model2"), Some(2));
        assert_eq!(parse_number("/model5"), Some(5));
        assert_eq!(parse_number("/m3"), Some(3));
        // Rejected: `0` is not a row (the list is 1-based)...
        assert_eq!(parse_number("/model0"), None);
        // ...and every non-number command keeps its own meaning.
        assert_eq!(parse_number("/models"), None);
        assert_eq!(parse_number("/model*"), None);
        assert_eq!(parse_number("/model"), None);
        assert_eq!(parse_number("/m"), None);
        assert_eq!(parse_number("/dm"), None);
        assert_eq!(parse_number("/default-model"), None);
        // A bare model id starting with a digit is a selector, not a row key.
        assert_eq!(parse_number("/model1x"), None);
        assert_eq!(parse_number("/model x"), None);
        assert_eq!(parse_number(""), None);
        // The REPL dispatches the command word without its slash, so the parser
        // must accept both spellings — otherwise `/model2` would work from the
        // TUI but not the streaming REPL (or vice versa).
        assert_eq!(parse_number("model2"), Some(2));
        assert_eq!(parse_number("m3"), Some(3));
        assert_eq!(parse_number("model"), None);
        assert_eq!(parse_number("models"), None);
    }

    /// A numeric `/modelN` line is still a *picker* line to the front-ends (the
    /// list must render and the arrows must step), but its prefix is empty: the
    /// digits are the row key, never a filter string.
    #[test]
    fn numbered_lines_are_picker_lines_with_empty_prefix() {
        assert_eq!(
            parse_model_line("/model2"),
            Some(("/model2".to_string(), String::new()))
        );
        assert_eq!(
            parse_model_line("/m4"),
            Some(("/m4".to_string(), String::new()))
        );
        // A real prefix after a numeric command is still just a prefix.
        assert_eq!(
            parse_model_line("/model2 ollama"),
            Some(("/model2".to_string(), "ollama".to_string()))
        );
        // The non-picker commands keep declining.
        assert_eq!(parse_model_line("/models"), None);
        assert_eq!(parse_model_line("/model*"), None);
        assert_eq!(parse_model_line("/model0"), None);
    }

    /// Rows are what the operator reads *and* types: the number on a line is
    /// the number in `/modelN`, and the marker tracks the arrow selection.
    #[test]
    fn rows_number_each_model_and_mark_the_selection() {
        let labels = vec!["a/x".to_string(), "b/y".to_string(), "c/z".to_string()];
        let rows = format_rows(&labels, Some(1), "a/x");
        assert_eq!(rows.len(), 3, "one row per model");
        assert!(rows[0].contains('1') && rows[0].contains("a/x"), "{:?}", rows[0]);
        assert!(rows[1].contains("▸2") && rows[1].contains("b/y"), "{:?}", rows[1]);
        assert!(rows[2].contains('3') && rows[2].contains("c/z"), "{:?}", rows[2]);
        // Only the selected row wears the marker.
        assert_eq!(rows.iter().filter(|r| r.contains('▸')).count(), 1);
        // No selection yet: nothing is marked (the prompt is still open).
        assert!(format_rows(&labels, None, "").iter().all(|r| !r.contains('▸')));
        // The active model is flagged so the operator can see where they are.
        assert!(rows[0].contains("•current"), "{:?}", rows[0]);
        assert!(!rows[1].contains("•current"), "{:?}", rows[1]);
        assert!(format_rows(&[], None, "").is_empty());
    }

    /// Recording keeps one entry per label at the head, orders by timestamp,
    /// and skips the `fake` provider — the property the picker relies on.
    /// Exercised against a throwaway file (no env, no clock), so parallel
    /// prompt-building tests can never observe it.
    #[test]
    fn record_dedupes_and_orders() {
        let dir = std::env::temp_dir().join(format!("pir-mhist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("agent").join("model-history.json");
        // Timestamps drive the order, not call order.
        record_to(&file, "p", "m1", 10);
        record_to(&file, "p", "m2", 30);
        record_to(&file, "p", "m1", 20);
        record_to(&file, "fake", "m9", 40);
        let labels: Vec<String> = load_from(&file).into_iter().map(|e| e.label).collect();
        assert_eq!(labels, vec!["p/m2", "p/m1"], "recency-ordered, fake skipped: {labels:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
