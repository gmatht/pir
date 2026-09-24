//! Taste: durable, operator-recorded preferences (Feature 12).
//!
//! AGENTS.md is a *document*: the operator writes it once, in prose, and it is
//! the same for every session. Taste is the complement — small, atomic
//! preferences the agent or the operator records *as they come up* ("use
//! `rg`, never `grep`", "this project's tests need `--features tui`", "don't
//! commit without asking"). Each entry carries the evidence that produced it and
//! a confidence, and the whole set is injected into the system prompt so it
//! survives into every later session without the operator repeating themselves.
//!
//! # On-disk shape
//!
//! ```text
//!   <.pir or config>/taste/taste.md        human-readable rendered view
//!   <.pir or config>/taste/taste.json      the machine-readable store
//! ```
//!
//! The markdown file is the *rendered view*, so the operator can read and — if
//! they want — hand-edit the store without a special tool. On load, if the
//! markdown contains entries the JSON doesn't, they are **adopted** (see
//! [`Store::load`]); this is what makes hand-editing safe rather than
//! self-defeating. Deleting a line from the markdown does not delete it from the
//! store (the JSON is authoritative for deletions, so a hand-edit can't silently
//! drop a preference) — use `/taste forget`, which updates both.
//!
//! # Where it lives
//!
//! Two scopes, mirroring how the project's own instructions work:
//!
//! * `<cwd>/.pir/taste/` — project-local, travels with the repo;
//! * `<config>/taste/` — user-global (`~/.pi/taste/`), applies everywhere.
//!
//! Both are injected, project entries after global ones so the project wins on
//! a conflicting instruction.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Soft cap on an entry's text, so the store can't grow into a document dump.
pub const MAX_TEXT_CHARS: usize = 400;

/// Soft cap on the number of entries injected into the prompt. Beyond this the
/// catalog stops being a nudge and starts being a second system prompt.
pub const MAX_INJECTED: usize = 40;

/// One recorded preference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// The preference, as one imperative line. Keyed by its slug.
    pub text: String,
    /// Why we believe it — the prompt, correction or observation behind it.
    #[serde(default)]
    pub evidence: String,
    /// 0.0-1.0. Entries below the threshold are kept but not injected.
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    /// Unix epoch seconds of the last write.
    #[serde(default)]
    pub updated: u64,
    /// How many times it has been reinforced (re-recorded or confirmed).
    #[serde(default)]
    pub hits: u32,
}

fn default_confidence() -> f32 {
    0.6
}

/// Which store an entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    /// `<config>/taste` — applies to every project.
    Global,
    /// `<cwd>/.pir/taste` — applies to this project.
    Project,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::Global => "user",
            Scope::Project => "project",
        }
    }
}

/// An entry together with where it lives.
#[derive(Debug, Clone, PartialEq)]
pub struct Scoped {
    pub entry: Entry,
    pub scope: Scope,
}

impl Scoped {
    pub fn slug(&self) -> String {
        slug(&self.entry.text)
    }
}

/// The persisted store for one scope.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub entries: Vec<Entry>,
}

/// Root directory for a scope.
pub fn dir(scope: Scope, cwd: &Path) -> PathBuf {
    match scope {
        Scope::Project => cwd.join(".pir").join("taste"),
        // `PI_DIR` (and the invoking-user home) aware, like every other pir
        // store — so a sandboxed session reads the operator's real taste.
        Scope::Global => crate::config::pi_dir().join("taste"),
    }
}

impl Store {
    pub fn path(scope: Scope, cwd: &Path) -> PathBuf {
        dir(scope, cwd).join("taste.json")
    }

    /// Markdown rendering path (the operator-facing view).
    pub fn markdown_path(scope: Scope, cwd: &Path) -> PathBuf {
        dir(scope, cwd).join("taste.md")
    }

    /// Load a scope's store. A missing file is an empty store, not an error.
    ///
    /// If the markdown view contains a line that the JSON store does not, it is
    /// adopted (with default confidence). That is what lets an operator add a
    /// preference by hand in `taste.md` and have it take effect on the next
    /// session, instead of the file being a write-only artifact.
    pub fn load(scope: Scope, cwd: &Path) -> Store {
        let mut store = fs::read_to_string(Self::path(scope, cwd))
            .ok()
            .and_then(|s| serde_json::from_str::<Store>(&s).ok())
            .unwrap_or_default();
        if let Ok(md) = fs::read_to_string(Self::markdown_path(scope, cwd)) {
            for text in markdown_entries(&md) {
                let key = slug(&text);
                if !key.is_empty()
                    && !store.entries.iter().any(|e| slug(&e.text) == key)
                {
                    store.entries.push(Entry {
                        text,
                        evidence: "recorded by hand in taste.md".to_string(),
                        confidence: default_confidence(),
                        updated: crate::term::epoch(),
                        hits: 0,
                    });
                }
            }
        }
        store
    }

    /// Persist the store, rendering the markdown view alongside it. Best-effort:
    /// a store we cannot write must not fail a turn (the in-memory copy still
    /// applies for this session).
    pub fn save(&self, scope: Scope, cwd: &Path) -> Result<(), String> {
        let dir = dir(scope, cwd);
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(Self::path(scope, cwd), json)
            .map_err(|e| format!("cannot write {}: {e}", Self::path(scope, cwd).display()))?;
        fs::write(Self::markdown_path(scope, cwd), self.render_markdown())
            .map_err(|e| format!("cannot write {}: {e}", Self::markdown_path(scope, cwd).display()))?;
        Ok(())
    }

    /// Render the operator-facing markdown view.
    pub fn render_markdown(&self) -> String {
        let mut s = String::from(
            "# Taste\n\n\
             Preferences pir has recorded for this scope. The machine-readable store is \
             `taste.json` next to this file — edit *that* (or use `/taste`) to remove an entry; \
             a line added here by hand is adopted on the next load.\n\n",
        );
        if self.entries.is_empty() {
            s.push_str("_(nothing recorded yet)_\n");
            return s;
        }
        let mut sorted: Vec<&Entry> = self.entries.iter().collect();
        sorted.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));
        for e in sorted {
            s.push_str(&format!(
                "- {} (confidence {:.1}{})\n",
                e.text,
                e.confidence,
                if e.hits > 0 { format!(", confirmed {}×", e.hits) } else { String::new() }
            ));
            if !e.evidence.is_empty() {
                s.push_str(&format!("  - evidence: {}\n", e.evidence));
            }
        }
        s
    }

    /// Record (or reinforce) a preference. Returns `(slug, created)`.
    ///
    /// Re-recording an existing preference *reinforces* it: confidence nudges up
    /// and `hits` increments, so a preference the operator keeps restating
    /// becomes more prominent while a one-off stays at its initial weight.
    pub fn remember(&mut self, text: &str, evidence: &str, confidence: Option<f32>) -> (String, bool) {
        let text = text.trim();
        let key = slug(text);
        let conf = confidence.unwrap_or(default_confidence()).clamp(0.0, 1.0);
        if let Some(e) = self.entries.iter_mut().find(|e| slug(&e.text) == key) {
            e.hits += 1;
            // Reinforcing raises confidence toward 1.0 without ever exceeding it.
            e.confidence = (e.confidence + 0.1).min(1.0).max(conf);
            e.text = text.to_string();
            if !evidence.trim().is_empty() {
                e.evidence = evidence.trim().to_string();
            }
            e.updated = crate::term::epoch();
            return (key, false);
        }
        self.entries.push(clamp_entry(Entry {
            text: text.to_string(),
            evidence: evidence.trim().to_string(),
            confidence: conf,
            updated: crate::term::epoch(),
            hits: 0,
        }));
        (key, true)
    }

    /// Forget a preference by slug or by a unique prefix of its text. Returns
    /// the removed entry, or an error naming what *is* stored.
    pub fn forget(&mut self, query: &str) -> Result<Entry, String> {
        let q = slug(query);
        if q.is_empty() {
            return Err("taste: give a slug or a fragment of the preference to forget".into());
        }
        let exact = self.entries.iter().position(|e| slug(&e.text) == q);
        let idx = exact.or_else(|| {
            let mut matches = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| slug(&e.text).contains(&q));
            match (matches.next(), matches.next()) {
                (Some((i, _)), None) => Some(i),
                _ => None,
            }
        });
        let Some(idx) = idx else {
            let known = self
                .entries
                .iter()
                .map(|e| slug(&e.text))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "taste: nothing matches '{query}'{}",
                if known.is_empty() { String::new() } else { format!(" (stored: {known})") }
            ));
        };
        Ok(self.entries.remove(idx))
    }
}

/// Truncate an entry's text/evidence to the cap.
fn clamp_entry(mut e: Entry) -> Entry {
    if e.text.chars().count() > MAX_TEXT_CHARS {
        e.text = format!("{}…", e.text.chars().take(MAX_TEXT_CHARS).collect::<String>());
    }
    if e.evidence.chars().count() > MAX_TEXT_CHARS {
        e.evidence = format!("{}…", e.evidence.chars().take(MAX_TEXT_CHARS).collect::<String>());
    }
    e
}

/// Read every scope's store, project entries last so they win on conflicts.
pub fn load_all(cwd: &Path) -> Vec<Scoped> {
    let mut out = Vec::new();
    for scope in [Scope::Global, Scope::Project] {
        for entry in Store::load(scope, cwd).entries {
            out.push(Scoped { entry, scope });
        }
    }
    out
}

/// Render the prompt section. Empty string when there is nothing to say, so a
/// session with no taste has a byte-identical prompt to before this feature.
///
/// Entries are sorted by confidence (descending) so the most-reinforced
/// preferences appear first, and only the top [`MAX_INJECTED`] are included — a
/// store that has grown into a document must not crowd out the actual task.
pub fn catalog(entries: &[Scoped]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut sorted: Vec<&Scoped> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        b.entry
            .confidence
            .partial_cmp(&a.entry.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            // Project entries break ties in their own favour.
            .then_with(|| b.scope.cmp(&a.scope))
            .then_with(|| a.entry.text.cmp(&b.entry.text))
    });
    let shown: Vec<&Scoped> = sorted.iter().take(MAX_INJECTED).copied().collect();
    let mut s = String::from(
        "\n# Taste (recorded preferences)\n\n\
         Preferences the operator has recorded for this project/user. Follow them; they exist so \
         you don't have to be told twice. When you learn a durable preference, record it with the \
         `remember` tool.\n\n",
    );
    for e in &shown {
        s.push_str(&format!(
            "- {}{}\n",
            e.entry.text,
            if e.entry.confidence >= 0.8 { "" } else { " (tentative)" }
        ));
    }
    if sorted.len() > shown.len() {
        s.push_str(&format!(
            "\n({} more recorded preference(s) not shown; use `/taste` to see them all)\n",
            sorted.len() - shown.len()
        ));
    }
    s
}

/// Lines of `taste.md` that look like preferences (`- text`), with the
/// confidence suffix and evidence sub-bullets stripped.
fn markdown_entries(md: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in md.lines() {
        let Some(rest) = line.trim().strip_prefix("- ") else { continue };
        // Strip `(confidence 0.7, confirmed 3×)` and anything after it.
        let text = match rest.find(" (confidence ") {
            Some(i) => &rest[..i],
            None => rest,
        };
        let text = text.trim();
        // Skip the evidence sub-bullets (`  - evidence: …`), which are nested
        // and therefore not at the top level, and any placeholder.
        if text.is_empty() || text.starts_with("evidence:") || text.starts_with('_') {
            continue;
        }
        out.push(text.to_string());
    }
    out
}

/// Normalize a preference into a stable key: lowercase, punctuation dropped,
/// first ~8 words. Long prose and its short restatement must collide.
pub fn slug(s: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    for w in s.split_whitespace() {
        let cleaned: String = w
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect::<String>()
            .to_ascii_lowercase();
        if !cleaned.is_empty() {
            words.push(cleaned);
        }
        if words.len() >= 8 {
            break;
        }
    }
    words.join("-")
}

/// Case-insensitive substring lookup over the whole store, for `/taste find`.
pub fn find<'a>(entries: &'a [Scoped], needle: &str) -> Vec<&'a Scoped> {
    let n = needle.trim().to_ascii_lowercase();
    if n.is_empty() {
        return entries.iter().collect();
    }
    entries
        .iter()
        .filter(|e| e.entry.text.to_ascii_lowercase().contains(&n) || e.entry.evidence.to_ascii_lowercase().contains(&n))
        .collect()
}

/// A compact multi-scope listing for the REPL.
pub fn listing(entries: &[Scoped]) -> String {
    if entries.is_empty() {
        return "(nothing recorded yet — the agent records preferences with its `remember` tool, or add one with `/taste remember <text>`)".to_string();
    }
    let mut by_scope: BTreeMap<Scope, Vec<&Scoped>> = BTreeMap::new();
    for e in entries {
        by_scope.entry(e.scope).or_default().push(e);
    }
    let mut s = String::new();
    for scope in [Scope::Global, Scope::Project] {
        let Some(list) = by_scope.get(&scope) else { continue };
        s.push_str(&format!(
            "{} ({} entr{}, {})\n",
            scope.label(),
            list.len(),
            if list.len() == 1 { "y" } else { "ies" },
            Store::path(scope, &std::env::current_dir().unwrap_or_default()).display()
        ));
        let mut sorted = list.clone();
        sorted.sort_by(|a, b| b.entry.confidence.partial_cmp(&a.entry.confidence).unwrap_or(std::cmp::Ordering::Equal));
        for e in sorted {
            s.push_str(&format!(
                "  [{:.1}] {}\n",
                e.entry.confidence, e.entry.text
            ));
            if !e.entry.evidence.is_empty() {
                s.push_str(&format!("        {}\n", crate::term::dim(&e.entry.evidence)));
            }
        }
    }
    s.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test works in a throwaway cwd + config dir, so a real `.pir/taste`
    /// or `~/.pi/taste` is never touched.
    fn with_dirs<T>(tag: &str, f: impl FnOnce(&Path, &Path) -> T) -> T {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("pir_taste_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let cwd = base.join("proj");
        let cfg = base.join("cfg");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&cfg).unwrap();
        let old = std::env::var_os("PI_DIR");
        // SAFETY: test-local env mutation under the shared env lock.
        unsafe { std::env::set_var("PI_DIR", &cfg); }
        let out = f(&cwd, &cfg);
        match old {
            Some(v) => unsafe { std::env::set_var("PI_DIR", v) },
            None => unsafe { std::env::remove_var("PI_DIR") },
        }
        let _ = fs::remove_dir_all(&base);
        out
    }

    #[test]
    fn slug_is_stable_and_collapses_restatements() {
        assert_eq!(slug("Use rg, never grep"), "use-rg-never-grep");
        assert_eq!(slug("  USE RG — never grep!  "), "use-rg-never-grep");
        assert_eq!(slug(""), "");
        assert_eq!(slug("!!!"), "");
        // Long prose and its short restatement collide on the first 8 words.
        let long = slug("Prefer NBD over SSHFS for remote filesystem access because it is faster");
        let short = slug("Prefer NBD over SSHFS for remote filesystem access");
        assert_eq!(long, short, "a restatement must reinforce, not duplicate");
    }

    #[test]
    fn remember_creates_then_reinforces() {
        let mut s = Store::default();
        let (key, created) = s.remember("Use rg, never grep", "operator said so", Some(0.6));
        assert!(created);
        assert_eq!(key, "use-rg-never-grep");
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.entries[0].confidence, 0.6);
        assert_eq!(s.entries[0].hits, 0);
        // Re-recording reinforces: hits up, confidence up, still one entry.
        let (_, created) = s.remember("use rg never grep", "restated", None);
        assert!(!created, "a restatement must not create a second entry");
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.entries[0].hits, 1);
        assert!(s.entries[0].confidence > 0.6, "reinforcement raises confidence");
        assert_eq!(s.entries[0].evidence, "restated", "evidence is refreshed");
    }

    #[test]
    fn confidence_is_clamped_and_never_exceeds_one() {
        let mut s = Store::default();
        s.remember("x", "", Some(5.0));
        assert_eq!(s.entries[0].confidence, 1.0);
        s.remember("y", "", Some(-1.0));
        assert_eq!(s.entries[1].confidence, 0.0);
        // Repeated reinforcement saturates at 1.0 rather than overflowing.
        for _ in 0..20 {
            s.remember("x", "", None);
        }
        assert_eq!(s.entries[0].confidence, 1.0);
    }

    #[test]
    fn long_text_is_truncated() {
        let mut s = Store::default();
        let long = "word ".repeat(300);
        s.remember(&long, &long, None);
        assert!(s.entries[0].text.chars().count() <= MAX_TEXT_CHARS + 1);
        assert!(s.entries[0].evidence.chars().count() <= MAX_TEXT_CHARS + 1);
    }

    #[test]
    fn forget_matches_exactly_then_by_unique_fragment() {
        let mut s = Store::default();
        s.remember("Use rg never grep", "", None);
        s.remember("Tests need --features tui", "", None);
        // Exact slug.
        let removed = s.forget("tests need --features tui").unwrap();
        assert!(removed.text.contains("features tui"));
        assert_eq!(s.entries.len(), 1);
        // Unique fragment.
        let removed = s.forget("rg never").unwrap();
        assert!(removed.text.contains("rg"));
        assert!(s.entries.is_empty());
        // Nothing left to forget: an error that lists what is stored.
        let err = s.forget("anything").unwrap_err();
        assert!(err.contains("nothing matches"), "got: {err}");
    }

    #[test]
    fn forget_refuses_an_ambiguous_fragment() {
        let mut s = Store::default();
        s.remember("Use rg for searches", "", None);
        s.remember("Use rg for file lists", "", None);
        let err = s.forget("use rg").unwrap_err();
        assert!(err.contains("nothing matches"), "ambiguous must not delete: {err}");
        assert_eq!(s.entries.len(), 2);
    }

    #[test]
    fn round_trips_through_json_and_renders_markdown() {
        with_dirs("rt", |cwd, _cfg| {
            let mut s = Store::default();
            s.remember("Prefer rg over grep", "operator correction", Some(0.9));
            s.save(Scope::Project, cwd).expect("save");
            assert!(Store::path(Scope::Project, cwd).is_file(), "json store missing");
            let md_path = Store::markdown_path(Scope::Project, cwd);
            assert!(md_path.is_file(), "markdown view missing");
            let md = fs::read_to_string(&md_path).unwrap();
            assert!(md.contains("Prefer rg over grep"), "got: {md}");
            assert!(md.contains("confidence 0.9"), "confidence must be visible: {md}");
            assert!(md.contains("evidence: operator correction"), "got: {md}");
            // Reload is lossless.
            let back = Store::load(Scope::Project, cwd);
            assert_eq!(back.entries.len(), 1);
            assert_eq!(back.entries[0].text, "Prefer rg over grep");
            assert_eq!(back.entries[0].evidence, "operator correction");
        });
    }

    #[test]
    fn load_adopts_a_hand_written_markdown_line() {
        with_dirs("adopt", |cwd, _cfg| {
            // No JSON at all — only a hand-written markdown view.
            let dir = dir(Scope::Project, cwd);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                Store::markdown_path(Scope::Project, cwd),
                "# Taste\n\n- Always run the formatter before committing (confidence 0.7)\n  - evidence: whatever\n",
            )
            .unwrap();
            let s = Store::load(Scope::Project, cwd);
            assert_eq!(s.entries.len(), 1, "a hand-written line must be adopted");
            assert_eq!(s.entries[0].text, "Always run the formatter before committing");
            assert!(s.entries[0].evidence.contains("by hand"), "provenance recorded");
        });
    }

    #[test]
    fn markdown_entries_ignores_prose_and_placeholders() {
        let md = "# Taste\n\nIntro prose.\n\n_(nothing recorded yet)_\n- Real one\n  - evidence: x\n";
        assert_eq!(markdown_entries(md), vec!["Real one".to_string()]);
        assert!(markdown_entries("").is_empty());
    }

    #[test]
    fn catalog_is_empty_without_entries_and_orders_by_confidence() {
        assert_eq!(catalog(&[]), "", "no taste => no prompt change");
        let mk = |t: &str, c: f32, scope: Scope| Scoped {
            entry: Entry { text: t.into(), evidence: String::new(), confidence: c, updated: 0, hits: 0 },
            scope,
        };
        let entries = vec![
            mk("low confidence thing", 0.3, Scope::Global),
            mk("high confidence thing", 0.95, Scope::Project),
        ];
        let c = catalog(&entries);
        assert!(c.contains("# Taste"), "got: {c}");
        assert!(
            c.find("high confidence thing").unwrap() < c.find("low confidence thing").unwrap(),
            "highest confidence must come first: {c}"
        );
        // Below-0.8 entries are marked tentative; a firm one is not.
        assert!(c.contains("low confidence thing (tentative)"), "got: {c}");
        assert!(!c.contains("high confidence thing (tentative)"), "got: {c}");
    }

    #[test]
    fn catalog_caps_the_injected_entries() {
        let entries: Vec<Scoped> = (0..MAX_INJECTED + 10)
            .map(|i| Scoped {
                entry: Entry {
                    text: format!("preference number {i}"),
                    evidence: String::new(),
                    confidence: 0.8,
                    updated: 0,
                    hits: 0,
                },
                scope: Scope::Global,
            })
            .collect();
        let c = catalog(&entries);
        assert!(c.contains("10 more recorded preference(s) not shown"), "got: {c}");
    }

    #[test]
    fn load_all_reads_both_scopes_project_last() {
        with_dirs("all", |cwd, _cfg| {
            let mut g = Store::default();
            g.remember("global preference", "", Some(0.8));
            g.save(Scope::Global, cwd).unwrap();
            let mut p = Store::default();
            p.remember("project preference", "", Some(0.8));
            p.save(Scope::Project, cwd).unwrap();
            let all = load_all(cwd);
            assert_eq!(all.len(), 2);
            assert_eq!(all[0].scope, Scope::Global, "global first");
            assert_eq!(all[1].scope, Scope::Project, "project last (it wins)");
            let l = listing(&all);
            assert!(l.contains("user (1 entry"), "got: {l}");
            assert!(l.contains("project (1 entry"), "got: {l}");
        });
    }

    #[test]
    fn find_is_case_insensitive_and_matches_evidence() {
        let mk = |t: &str, ev: &str| Scoped {
            entry: Entry {
                text: t.into(),
                evidence: ev.into(),
                confidence: 0.7,
                updated: 0,
                hits: 0,
            },
            scope: Scope::Global,
        };
        let entries = vec![mk("Use rg", "operator said grep is slow"), mk("Run fmt", "")];
        assert_eq!(find(&entries, "RG").len(), 1);
        assert_eq!(find(&entries, "grep is slow").len(), 1, "evidence is searchable");
        assert_eq!(find(&entries, "").len(), 2, "empty needle matches all");
        assert_eq!(find(&entries, "zzz").len(), 0);
    }

    #[test]
    fn listing_says_so_when_empty() {
        assert!(listing(&[]).contains("nothing recorded yet"));
    }
}
