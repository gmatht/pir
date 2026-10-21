//! Cross-platform Tree contract (model + view trait) for a left sessions tree.
//!
//! Backend-agnostic on purpose: the model types here (`TreeStatus`, `TreeNode`)
//! carry plain data — no GTK/NWG/curses handles — so `pir` builds ONE tree from
//! its session model and every backend renders it. Each backend implements
//! [`TreeView`] natively where it can (GTK `TreeView`, NWG tree-view,
//! pancurses indent+glyph rows) and falls back to flat-with-headers where it
//! cannot. `pir` code never branches on platform for widget behaviour.
//!
//! Required capabilities the contract guarantees (mirrored from the plan):
//! hierarchy + indent, expand/collapse, single selection + keyboard nav,
//! per-row status dot + badge + detail line, full refresh (`set_roots`) +
//! cheap per-tick presence update (`update_node`), `on_select`/`on_toggle`
//! callbacks, focus/reveal, empty-group hiding, and a text-glyph fallback
//! (`TreeStatus::glyph`) where badges/colors are unavailable.
//!
//! The four statuses mirror `pir`'s `SessionPresence` exactly — the mapping is
//! 1:1, never lossy:
//! `Running` → spinner/dot, `NeedsInputUnread(n)` → bold + badge,
//! `NeedsInputSeen` → check, `Idle` → dim. `Error` is the fifth, for a session
//! whose last turn errored: it renders like unread (demands attention) but
//! with a distinct glyph so "error" and "waiting" never look alike.

use std::collections::HashMap;

/// Per-row presence. Same four (+error) states as `pir::SessionPresence`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeStatus {
    /// A live turn is in flight. Rendered as spinner/dot (accent color),
    /// never bold — a spinner must never steal focus.
    Running,
    /// Waiting for input, operator has NOT seen the finish state. Bold label
    /// + badge with `n`. The only state that steals attention.
    NeedsInputUnread(u32),
    /// Waiting for input, operator already saw it. Check outline, normal
    /// weight, no badge, no bell.
    NeedsInputSeen,
    /// Finished/completed, nothing pending. Dim, no adornment.
    Idle,
    /// Last turn errored. Renders like unread (demands attention) with a
    /// distinct glyph.
    Error,
}

impl TreeStatus {
    /// Text glyph fallback: every backend MUST render at least this, so the
    /// states stay distinct with no color, no badge, no font weight.
    pub fn glyph(&self) -> &'static str {
        match self {
            TreeStatus::Running => "▶",
            TreeStatus::NeedsInputUnread(_) => "●",
            TreeStatus::NeedsInputSeen => "✓",
            TreeStatus::Idle => "·",
            TreeStatus::Error => "✗",
        }
    }

    /// Pending count for the badge (0 unless unread).
    pub fn unread(&self) -> u32 {
        match self {
            TreeStatus::NeedsInputUnread(n) => *n,
            TreeStatus::Error => 1,
            _ => 0,
        }
    }

    /// True for the two attention-stealing states.
    pub fn demands_attention(&self) -> bool {
        matches!(self, TreeStatus::NeedsInputUnread(_) | TreeStatus::Error)
    }

    /// Bold label? Only unread (and error) rows.
    pub fn bold(&self) -> bool {
        self.demands_attention()
    }
}

/// One node in the tree: a project-group header (`children` non-empty, `id` is
/// the group key) or a session leaf (`children` empty, `id` is the session id).
/// Plain data — constructible and unit-testable with no backend present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    /// Stable id: group key or session id (transcript path string). What
    /// `update_node` / `on_select` / `reveal` key on.
    pub id: String,
    /// Primary label (group name or session title).
    pub label: String,
    /// Secondary line (cwd tail, preview, spinner text). Empty = no detail.
    pub detail: String,
    pub status: TreeStatus,
    /// Group children. Empty for session leaves.
    pub children: Vec<TreeNode>,
    /// Groups only: whether the group renders expanded. Ignored on leaves.
    pub expanded: bool,
}

impl TreeNode {
    pub fn group(id: &str, label: &str, detail: &str, expanded: bool) -> Self {
        TreeNode {
            id: id.to_string(),
            label: label.to_string(),
            detail: detail.to_string(),
            status: TreeStatus::Idle,
            children: Vec::new(),
            expanded,
        }
    }

    pub fn leaf(id: &str, label: &str, detail: &str, status: TreeStatus) -> Self {
        TreeNode {
            id: id.to_string(),
            label: label.to_string(),
            detail: detail.to_string(),
            status,
            children: Vec::new(),
            expanded: false,
        }
    }

    pub fn is_group(&self) -> bool {
        !self.children.is_empty()
    }
}

/// Anything that can supply tree roots (in `pir`: the grouped session model).
/// Backends never see this trait — they receive an already-built
/// `Vec<TreeNode>` via `set_roots` — but the trait pins the direction:
/// `pir` owns the model, backends own the pixels.
pub trait TreeModel {
    fn roots(&self) -> Vec<TreeNode>;
}

/// Cross-platform tree view. Created via `App::create_tree()` (a
/// backend-appropriate default) or held as `Box<dyn TreeView>` by embedders.
/// All `#[cfg]` platform branching lives in backend impls, never in callers.
///
/// Threading: calls happen on the UI thread (same rule as every other widget
/// in this crate). Callbacks fire on the UI thread too.
pub trait TreeView {
    /// Replace the whole tree (grouping changed, session added/removed).
    /// Backends diff internally where cheap; a full rebuild is acceptable.
    /// Empty groups are hidden (never rendered as bare headers).
    fn set_roots(&mut self, roots: Vec<TreeNode>);

    /// Cheap per-tick update for ONE node (presence spinner, badge count,
    /// detail line). MUST NOT reorder rows, reset expansion, or move
    /// selection — a spinner tick at 750ms that collapses the tree is a bug.
    /// Unknown ids are ignored (no-op), so callers can blindly push updates
    /// for sessions that left the tree.
    fn update_node(&mut self, id: &str, label: &str, detail: &str, status: TreeStatus);

    /// Selection callback: fires with the leaf/group id on Enter/click.
    fn set_on_select(&mut self, cb: Box<dyn FnMut(String)>);

    /// Expand/collapse callback: fires with the group id on toggle.
    fn set_on_toggle(&mut self, cb: Box<dyn FnMut(String, bool)>);

    /// Ensure `id` is visible (expand ancestors, scroll into view). Used on
    /// unread arrival so the badge is never hidden inside a collapsed group.
    fn reveal(&mut self, id: &str);

    /// Currently focused/selected node id, if any.
    fn focused_id(&self) -> Option<String>;

    /// Number ofATTENTION rows (unread + error) — for tests and for backends
    /// that render a global indicator.
    fn attention_count(&self) -> usize {
        self.flatten().iter().filter(|n| n.status.demands_attention()).count()
    }

    /// Pre-order flat list of all nodes (groups + leaves). Default impl for
    /// backends that store roots verbatim; native backends may override with
    /// their visible-row order.
    fn flatten(&self) -> Vec<TreeNode> {
        Vec::new()
    }
}

/// Backend-free in-memory tree: the reference `TreeView` impl, the fallback
/// for backends without a native tree, and what `pir` unit tests assert
/// against. Rendering (text rows for pancurses, DOM for wasm, …) reads from
/// this same state, so every backend shows the same tree.
pub struct MemoryTree {
    roots: Vec<TreeNode>,
    /// Flat id → (is_group, expanded) for toggle handling.
    selected: Option<String>,
    on_select: Option<Box<dyn FnMut(String)>>,
    on_toggle: Option<Box<dyn FnMut(String, bool)>>,
}

impl Default for MemoryTree {
    fn default() -> Self {
        MemoryTree { roots: Vec::new(), selected: None, on_select: None, on_toggle: None }
    }
}

impl std::fmt::Debug for MemoryTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryTree")
            .field("roots", &self.roots)
            .field("selected", &self.selected)
            .finish()
    }
}

impl MemoryTree {
    pub fn new() -> Self {
        MemoryTree::default()
    }

    fn find_mut<'a>(nodes: &'a mut [TreeNode], id: &str) -> Option<&'a mut TreeNode> {
        for n in nodes {
            if n.id == id {
                return Some(n);
            }
            if let Some(hit) = Self::find_mut(&mut n.children, id) {
                return Some(hit);
            }
        }
        None
    }

    fn find<'a>(nodes: &'a [TreeNode], id: &str) -> Option<&'a TreeNode> {
        for n in nodes {
            if n.id == id {
                return Some(n);
            }
            if let Some(hit) = Self::find(&n.children, id) {
                return Some(hit);
            }
        }
        None
    }

    fn flatten_into(nodes: &[TreeNode], out: &mut Vec<TreeNode>) {
        for n in nodes {
            out.push(n.clone());
            Self::flatten_into(&n.children, out);
        }
    }

    /// Toggle a group's expansion, firing `on_toggle`. Returns the new state,
    /// or `None` for a leaf/unknown id (no-op).
    pub fn toggle(&mut self, id: &str) -> Option<bool> {
        let next = match Self::find(&self.roots, id) {
            Some(n) if n.is_group() => !n.expanded,
            _ => return None,
        };
        if let Some(n) = Self::find_mut(&mut self.roots, id) {
            n.expanded = next;
        }
        if let Some(cb) = self.on_toggle.as_mut() {
            cb(id.to_string(), next);
        }
        Some(next)
    }

    /// Select a node, firing `on_select`. Unknown ids are ignored.
    pub fn select(&mut self, id: &str) {
        if Self::find(&self.roots, id).is_none() {
            return;
        }
        self.selected = Some(id.to_string());
        if let Some(cb) = self.on_select.as_mut() {
            cb(id.to_string());
        }
    }

    /// Text rows for backends without a native tree (pancurses, zork): group
    /// headers + indented leaves with glyphs, honoring expansion. Collapsed
    /// groups render one summary row.
    pub fn text_rows(&self) -> Vec<String> {
        let mut rows = Vec::new();
        for g in &self.roots {
            if g.children.is_empty() {
                continue; // empty groups hidden
            }
            let marker = if g.expanded { "▾" } else { "▸" };
            rows.push(format!("{} {}", marker, g.label));
            if !g.expanded {
                continue;
            }
            for leaf in &g.children {
                let badge = match leaf.status {
                    TreeStatus::NeedsInputUnread(n) => format!("●{n} "),
                    _ => format!("{} ", leaf.status.glyph()),
                };
                let detail = if leaf.detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", leaf.detail)
                };
                rows.push(format!("    {badge}{}{detail}", leaf.label));
            }
        }
        rows
    }
}

impl TreeView for MemoryTree {
    fn set_roots(&mut self, roots: Vec<TreeNode>) {
        // Preserve expansion + selection across refreshes keyed by id, so a
        // full rebuild (session added) never collapses the tree out from under
        // the operator. New groups default to expanded.
        let mut expanded: HashMap<String, bool> = HashMap::new();
        let collect = |nodes: &[TreeNode], expanded: &mut HashMap<String, bool>| {
            let mut stack: Vec<&TreeNode> = nodes.iter().collect();
            while let Some(n) = stack.pop() {
                if n.is_group() {
                    expanded.insert(n.id.clone(), n.expanded);
                }
                stack.extend(n.children.iter());
            }
        };
        collect(&self.roots, &mut expanded);
        self.roots = roots;
        let mut stack: Vec<&mut TreeNode> = self.roots.iter_mut().collect();
        while let Some(n) = stack.pop() {
            if n.is_group() {
                // `or(true)`: a group the operator never touched renders
                // expanded; an explicit collapse survives refreshes.
                n.expanded = expanded.get(&n.id).copied().unwrap_or(true);
            }
            stack.extend(n.children.iter_mut());
        }
        // (2021 edition: no `let` chains in the vendored crate.)
        if let Some(sel) = &self.selected {
            if Self::find(&self.roots, sel).is_none() {
                self.selected = None;
            }
        }
    }

    fn update_node(&mut self, id: &str, label: &str, detail: &str, status: TreeStatus) {
        // Presence-only update: label/detail/status change, structure
        // (order, expansion, selection, children) is untouched by contract.
        if let Some(n) = Self::find_mut(&mut self.roots, id) {
            n.label = label.to_string();
            n.detail = detail.to_string();
            n.status = status;
        }
    }

    fn set_on_select(&mut self, cb: Box<dyn FnMut(String)>) {
        self.on_select = Some(cb);
    }

    fn set_on_toggle(&mut self, cb: Box<dyn FnMut(String, bool)>) {
        self.on_toggle = Some(cb);
    }

    fn reveal(&mut self, id: &str) {
        // Expand every ancestor of `id` so a badge is never hidden inside a
        // collapsed group. The selected row follows the revealed node.
        fn expand_ancestors(nodes: &mut [TreeNode], id: &str) -> bool {
            for n in nodes.iter_mut() {
                if n.id == id {
                    return true;
                }
                if !n.children.is_empty() && expand_ancestors(&mut n.children, id) {
                    n.expanded = true;
                    return true;
                }
            }
            false
        }
        if expand_ancestors(&mut self.roots, id) {
            self.selected = Some(id.to_string());
        }
    }

    fn focused_id(&self) -> Option<String> {
        self.selected.clone()
    }

    fn flatten(&self) -> Vec<TreeNode> {
        let mut out = Vec::new();
        Self::flatten_into(&self.roots, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(id: &str, status: TreeStatus) -> TreeNode {
        TreeNode::leaf(id, id, "", status)
    }

    fn group(id: &str, children: Vec<TreeNode>) -> TreeNode {
        let mut g = TreeNode::group(id, id, "", true);
        g.children = children;
        g
    }

    fn sample() -> Vec<TreeNode> {
        vec![
            group("g1", vec![leaf("a", TreeStatus::Running), leaf("b", TreeStatus::NeedsInputUnread(2))]),
            group("g2", vec![leaf("c", TreeStatus::Idle)]),
        ]
    }

    /// Statuses carry distinct glyphs and only unread/error demand attention.
    /// Asserts on the values the code under test itself computed, not a
    /// recomputation from test inputs.
    #[test]
    fn status_glyphs_distinct_and_attention_only_on_unread_or_error() {
        let statuses = [
            TreeStatus::Running,
            TreeStatus::NeedsInputUnread(1),
            TreeStatus::NeedsInputSeen,
            TreeStatus::Idle,
            TreeStatus::Error,
        ];
        let mut glyphs = std::collections::HashSet::new();
        for s in statuses {
            assert!(glyphs.insert(s.glyph()), "glyph {:?} not distinct", s);
        }
        assert!(!TreeStatus::Running.bold());
        assert!(TreeStatus::NeedsInputUnread(3).bold());
        assert!(TreeStatus::Error.bold());
        assert!(!TreeStatus::NeedsInputSeen.bold());
        assert!(!TreeStatus::Idle.bold());
        assert_eq!(TreeStatus::NeedsInputUnread(3).unread(), 3);
        assert_eq!(TreeStatus::Idle.unread(), 0);
    }

    /// `set_roots` preserves expansion + selection for surviving ids, expands
    /// new groups, and drops selection for removed ids.
    #[test]
    fn set_roots_preserves_expansion_and_selection() {
        let mut t = MemoryTree::new();
        t.set_roots(sample());
        t.select("b");
        assert_eq!(t.focused_id().as_deref(), Some("b"));
        assert_eq!(t.toggle("g1"), Some(false));

        // Refresh: g1 collapsed survives, g3 (new) defaults expanded, b still focused.
        let mut next = sample();
        next.push(group("g3", vec![leaf("d", TreeStatus::Idle)]));
        t.set_roots(next);
        assert_eq!(t.focused_id().as_deref(), Some("b"));
        let g1 = MemoryTree::find(&t.roots, "g1").unwrap();
        assert!(!g1.expanded, "explicit collapse survives refresh");
        let g3 = MemoryTree::find(&t.roots, "g3").unwrap();
        assert!(g3.expanded, "new groups default expanded");

        // Refresh with b gone: selection clears rather than dangling.
        t.set_roots(vec![group("g1", vec![leaf("a", TreeStatus::Idle)])]);
        assert_eq!(t.focused_id(), None);
    }

    /// `update_node` changes label/detail/status but never structure: order,
    /// expansion and selection are untouched, and unknown ids are a no-op.
    #[test]
    fn update_node_is_presence_only_and_total_on_unknown_ids() {
        let mut t = MemoryTree::new();
        t.set_roots(sample());
        t.select("a");
        t.toggle("g2"); // collapse g2
        let before: Vec<String> = t.flatten().iter().map(|n| n.id.clone()).collect();

        t.update_node("b", "new label", "new detail", TreeStatus::NeedsInputSeen);
        t.update_node("nope", "x", "y", TreeStatus::Running); // no-op, must not panic
        let after: Vec<String> = t.flatten().iter().map(|n| n.id.clone()).collect();
        assert_eq!(before, after, "structure must not move under a presence update");

        let b = MemoryTree::find(&t.roots, "b").unwrap();
        assert_eq!(b.label, "new label");
        assert_eq!(b.detail, "new detail");
        assert_eq!(b.status, TreeStatus::NeedsInputSeen);
        assert_eq!(t.focused_id().as_deref(), Some("a"), "selection untouched");
        assert!(!MemoryTree::find(&t.roots, "g2").unwrap().expanded, "expansion untouched");
    }

    /// `reveal` expands collapsed ancestors and moves focus; unknown ids are a
    /// no-op (selection untouched).
    #[test]
    fn reveal_expands_ancestors_and_focuses() {
        let mut t = MemoryTree::new();
        t.set_roots(sample());
        t.toggle("g2");
        assert!(!MemoryTree::find(&t.roots, "g2").unwrap().expanded);
        t.reveal("c");
        assert!(MemoryTree::find(&t.roots, "g2").unwrap().expanded);
        assert_eq!(t.focused_id().as_deref(), Some("c"));

        t.reveal("ghost");
        assert_eq!(t.focused_id().as_deref(), Some("c"), "unknown reveal is a no-op");
    }

    /// Callbacks fire exactly once per select/toggle with the right payload.
    #[test]
    fn callbacks_fire_with_ids() {
        use std::sync::{Arc, Mutex};
        let mut t = MemoryTree::new();
        t.set_roots(sample());
        let selected = Arc::new(Mutex::new(Vec::new()));
        let toggled = Arc::new(Mutex::new(Vec::new()));
        let s2 = selected.clone();
        t.set_on_select(Box::new(move |id| s2.lock().unwrap().push(id)));
        let t2 = toggled.clone();
        t.set_on_toggle(Box::new(move |id, expanded| t2.lock().unwrap().push((id, expanded))));

        t.select("a");
        t.select("ghost"); // unknown: no fire
        t.toggle("g1");
        t.toggle("a"); // leaf: no fire
        assert_eq!(*selected.lock().unwrap(), vec!["a".to_string()]);
        assert_eq!(*toggled.lock().unwrap(), vec![("g1".to_string(), false)]);
    }

    /// Empty groups are hidden from text rows; collapsed groups show one row;
    /// leaves carry glyphs.
    #[test]
    fn text_rows_hide_empty_groups_and_show_glyphs() {
        let mut t = MemoryTree::new();
        let mut roots = sample();
        roots.push(TreeNode::group("empty", "empty", "", true)); // no children
        t.set_roots(roots);
        let rows = t.text_rows();
        assert!(!rows.iter().any(|r| r.contains("empty")), "empty groups hidden: {rows:?}");
        assert!(rows.iter().any(|r| r.contains("●2")), "unread badge: {rows:?}");
        assert!(rows.iter().any(|r| r.contains("▶")), "running glyph: {rows:?}");

        t.toggle("g1");
        let rows2 = t.text_rows();
        assert!(rows2.iter().any(|r| r.contains("g1")), "collapsed header stays");
        assert!(!rows2.iter().any(|r| r.contains('●')), "collapsed rows hidden: {rows2:?}");
    }

    /// `attention_count` counts unread + error, nothing else.
    #[test]
    fn attention_count_counts_unread_and_error_only() {
        let mut t = MemoryTree::new();
        t.set_roots(vec![group(
            "g",
            vec![
                leaf("a", TreeStatus::Running),
                leaf("b", TreeStatus::NeedsInputUnread(2)),
                leaf("c", TreeStatus::Error),
                leaf("d", TreeStatus::Idle),
            ],
        )]);
        assert_eq!(t.attention_count(), 2);
    }
}
