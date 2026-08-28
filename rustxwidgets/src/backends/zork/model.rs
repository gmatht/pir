//! Pure, I/O-free model for the headless `zork` backend.
//!
//! Everything here is plain data + operations on it is no terminal
//! access, no `rustyline`, and no `thread_local` global: a [`ZorkState`] is
//! just a value you can own, clone, snapshot to JSON, and drive from either the
//! interactive REPL ([`crate::backends::zork::repl`]) or the typed test harness
//! ([`crate::backends::zork::harness`]).
//!
//! The free functions that the per-widget adapters call (e.g.
//! [`create_button`], [`set_label_text`]) operate on a thread-local singleton so
//! that the existing `backends_zork_adapter` shim keeps working unchanged.
//! That singleton is just a thin wrapper over a `ZorkState`; tests that want a
//! clean, isolated instance should use [`ZorkState`] / [`harness::Harness`]
//! directly instead.

use std::cell::RefCell;
use std::collections::HashMap;

pub type Callback = Box<dyn FnMut()>;

#[derive(Clone, Debug)]
pub struct MenuItemData {
    pub label: String,
    pub action: String,
    pub submenu: Option<Vec<MenuItemData>>,
}

#[derive(Clone, Debug)]
pub enum ZorkKind {
    Window { title: String },
    Button { label: String },
    Label { text: String },
    BoxWidget { horizontal: bool, spacing: i32 },
    Grid { cols: usize, rows: usize },
    Entry { buffer: String, cursor: usize },
    CheckButton { label: String, checked: bool },
    RadioButton { label: String, checked: bool, group_id: usize },
    Dialog { title: String },
    Menu,
    MenuBar,
    SimpleAction,
    DropDown { items: Vec<String>, selected: Option<usize> },
    TextView { text: String },
}

pub struct ZorkNode {
    pub id: usize,
    pub kind: ZorkKind,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub callbacks: Vec<Callback>,
}

impl ZorkNode {
    /// Run all callbacks currently stored on this node. Callers must invoke this
    /// *outside* of any borrow of the owning [`ZorkState`], because a callback
    /// may re-enter the model.
    pub fn fire_callbacks(&mut self) {
        // Take the callbacks out so the borrow on `self` is released before each
        // (potentially re-entrant) call.
        let mut cbs = std::mem::take(&mut self.callbacks);
        for cb in cbs.iter_mut() {
            cb();
        }
        self.callbacks = cbs;
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SnapshotNode {
    pub id: usize,
    pub kind: String,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub title: Option<String>,
    pub text: Option<String>,
    pub label: Option<String>,
    pub checked: Option<bool>,
    pub selected: Option<usize>,
    pub items: Option<Vec<String>>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Snapshot {
    pub nodes: Vec<SnapshotNode>,
    pub menu_items: HashMap<usize, Vec<MenuItemData>>,
    pub current_id: usize,
    pub running: bool,
}

pub struct ZorkState {
    pub nodes: Vec<ZorkNode>,
    pub next_id: usize,
    pub running: bool,
    pub current_id: usize,
    pub prev_location: Option<usize>,
    /// Menu model items keyed by menu node id.
    pub menu_items: HashMap<usize, Vec<MenuItemData>>,
}

impl Default for ZorkState {
    fn default() -> Self {
        Self::new()
    }
}

impl ZorkState {
    pub fn new() -> Self {
        ZorkState {
            nodes: Vec::new(),
            next_id: 1,
            running: true,
            current_id: 0,
            prev_location: None,
            menu_items: HashMap::new(),
        }
    }

    pub fn alloc_id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn add_node(&mut self, kind: ZorkKind, parent: Option<usize>) -> usize {
        let id = self.alloc_id();
        // If this is the first node (Window), set current_id
        if self.nodes.is_empty() {
            self.current_id = id;
        }
        self.nodes.push(ZorkNode {
            id,
            kind,
            parent,
            children: Vec::new(),
            callbacks: Vec::new(),
        });
        if let Some(pid) = parent {
            if let Some(p) = self.nodes.iter_mut().find(|n| n.id == pid) {
                p.children.push(id);
            }
        }
        id
    }

    pub fn node_mut(&mut self, id: usize) -> Option<&mut ZorkNode> {
        self.nodes.iter_mut().find(|n| n.id == id)
    }

    pub fn node(&self, id: usize) -> Option<&ZorkNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn find_window_id(&self) -> Option<usize> {
        self.nodes
            .iter()
            .find(|n| matches!(n.kind, ZorkKind::Window { .. } | ZorkKind::Dialog { .. }))
            .map(|n| n.id)
    }

    // -- Factory functions (operate on this state) --

    pub fn create_window(&mut self) -> usize {
        self.add_node(ZorkKind::Window { title: String::new() }, None)
    }

    pub fn create_button(&mut self, label: &str) -> usize {
        self.add_node(ZorkKind::Button { label: label.to_string() }, self.find_window_id())
    }

    pub fn create_label(&mut self, text: &str) -> usize {
        self.add_node(ZorkKind::Label { text: text.to_string() }, self.find_window_id())
    }

    pub fn create_box(&mut self, horizontal: bool, spacing: i32) -> usize {
        self.add_node(ZorkKind::BoxWidget { horizontal, spacing }, self.find_window_id())
    }

    pub fn create_grid(&mut self) -> usize {
        self.add_node(ZorkKind::Grid { cols: 0, rows: 0 }, self.find_window_id())
    }

    pub fn create_entry(&mut self) -> usize {
        self.add_node(ZorkKind::Entry { buffer: String::new(), cursor: 0 }, self.find_window_id())
    }

    pub fn create_menu(&mut self) -> usize {
        let id = self.add_node(ZorkKind::Menu, self.find_window_id());
        self.menu_items.insert(id, Vec::new());
        id
    }

    pub fn create_simple_action(&mut self, _name: &str) -> usize {
        self.add_node(ZorkKind::SimpleAction, self.find_window_id())
    }

    pub fn create_menubar(&mut self, model_id: usize, _action_group: *mut std::os::raw::c_void) -> usize {
        let bar_id = self.add_node(ZorkKind::MenuBar, self.find_window_id());
        let items = self.menu_items.get(&model_id).cloned().unwrap_or_default();
        self.menu_items.insert(bar_id, items.clone());
        for item in &items {
            if let Some(sub) = &item.submenu {
                let sub_id = self.add_node(ZorkKind::Menu, Some(bar_id));
                self.menu_items.insert(sub_id, sub.clone());
            }
        }
        bar_id
    }

    pub fn create_dialog(&mut self) -> usize {
        self.add_node(ZorkKind::Dialog { title: String::new() }, self.find_window_id())
    }

    pub fn create_dropdown(&mut self, items: &[&str]) -> usize {
        let items_str: Vec<String> = items.iter().map(|s| s.to_string()).collect();
        self.add_node(ZorkKind::DropDown { items: items_str, selected: None }, self.find_window_id())
    }

    pub fn create_checkbutton(&mut self, label: &str) -> usize {
        self.add_node(ZorkKind::CheckButton { label: label.to_string(), checked: false }, self.find_window_id())
    }

    pub fn create_radiobutton(&mut self, group_id: Option<usize>, label: &str) -> usize {
        let gid = group_id.unwrap_or(0);
        self.add_node(ZorkKind::RadioButton { label: label.to_string(), checked: false, group_id: gid }, self.find_window_id())
    }

    pub fn create_textview(&mut self) -> usize {
        self.add_node(ZorkKind::TextView { text: String::new() }, self.find_window_id())
    }

    // -- Menu model --

    pub fn menu_append(&mut self, menu_id: usize, label: &str, action: &str) {
        if let Some(items) = self.menu_items.get_mut(&menu_id) {
            items.push(MenuItemData {
                label: label.to_string(),
                action: action.to_string(),
                submenu: None,
            });
        }
    }

    pub fn menu_append_submenu(&mut self, menu_id: usize, label: &str, submenu_id: usize) {
        let sub_items = self.menu_items.get(&submenu_id).cloned().unwrap_or_default();
        if let Some(items) = self.menu_items.get_mut(&menu_id) {
            items.push(MenuItemData {
                label: label.to_string(),
                action: String::new(),
                submenu: Some(sub_items),
            });
        }
    }

    // -- Setters / getters --

    pub fn set_window_title(&mut self, id: usize, title: &str) {
        if let Some(n) = self.node_mut(id) {
            if let ZorkKind::Window { title: ref mut t } = n.kind {
                *t = title.to_string();
            }
        }
    }

    pub fn set_label_text(&mut self, id: usize, text: &str) {
        if let Some(n) = self.node_mut(id) {
            if let ZorkKind::Label { text: ref mut t } = n.kind {
                *t = text.to_string();
            }
        }
    }

    pub fn get_label_text(&self, id: usize) -> Option<String> {
        self.node(id).and_then(|n| {
            if let ZorkKind::Label { ref text } = n.kind {
                Some(text.clone())
            } else {
                None
            }
        })
    }

    pub fn add_callback(&mut self, id: usize, cb: Callback) {
        if let Some(n) = self.node_mut(id) {
            n.callbacks.push(cb);
        }
    }

    pub fn set_entry_text(&mut self, id: usize, text: &str) {
        if let Some(n) = self.node_mut(id) {
            if let ZorkKind::Entry { ref mut buffer, ref mut cursor } = n.kind {
                *buffer = text.to_string();
                *cursor = buffer.len();
            }
        }
    }

    pub fn get_entry_text(&self, id: usize) -> Option<String> {
        self.node(id).and_then(|n| {
            if let ZorkKind::Entry { ref buffer, .. } = n.kind {
                Some(buffer.clone())
            } else {
                None
            }
        })
    }

    pub fn set_textview_text(&mut self, id: usize, text: &str) {
        if let Some(n) = self.node_mut(id) {
            if let ZorkKind::TextView { text: ref mut t } = n.kind {
                *t = text.to_string();
            }
        }
    }

    pub fn get_textview_text(&self, id: usize) -> Option<String> {
        self.node(id).and_then(|n| {
            if let ZorkKind::TextView { ref text } = n.kind {
                Some(text.clone())
            } else {
                None
            }
        })
    }

    pub fn set_dropdown_items(&mut self, id: usize, items: &[&str]) {
        if let Some(n) = self.node_mut(id) {
            if let ZorkKind::DropDown { items: ref mut items_vec, .. } = n.kind {
                *items_vec = items.iter().map(|s| s.to_string()).collect();
            }
        }
    }

    pub fn set_dropdown_selected(&mut self, id: usize, idx: i32) {
        if let Some(n) = self.node_mut(id) {
            if let ZorkKind::DropDown { ref mut selected, ref items } = n.kind {
                *selected = if idx >= 0 && (idx as usize) < items.len() {
                    Some(idx as usize)
                } else {
                    None
                };
            }
        }
    }

    pub fn get_dropdown_selected(&self, id: usize) -> i32 {
        self.node(id)
            .and_then(|n| {
                if let ZorkKind::DropDown { ref selected, .. } = n.kind {
                    selected.map(|s| s as i32)
                } else {
                    None
                }
            })
            .unwrap_or(-1)
    }

    pub fn get_checkbutton_checked(&self, id: usize) -> bool {
        self.node(id)
            .map(|n| {
                if let ZorkKind::CheckButton { ref checked, .. } = n.kind {
                    *checked
                } else {
                    false
                }
            })
            .unwrap_or(false)
    }

    pub fn get_radiobutton_checked(&self, id: usize) -> bool {
        self.node(id)
            .map(|n| {
                if let ZorkKind::RadioButton { ref checked, .. } = n.kind {
                    *checked
                } else {
                    false
                }
            })
            .unwrap_or(false)
    }

    pub fn set_checkbutton_checked(&mut self, id: usize, checked: bool) {
        if let Some(n) = self.node_mut(id) {
            if let ZorkKind::CheckButton { checked: ref mut c, .. } = n.kind {
                *c = checked;
            }
        }
    }

    pub fn set_radiobutton_checked(&mut self, id: usize, checked: bool) {
        if let Some(n) = self.node_mut(id) {
            if let ZorkKind::RadioButton { checked: ref mut c, .. } = n.kind {
                *c = checked;
            }
        }
    }

    pub fn set_child(&mut self, parent_id: usize, child_id: usize) {
        self.nodes.iter_mut().for_each(|n| n.children.retain(|c| *c != child_id));
        if let Some(parent) = self.node_mut(parent_id) {
            parent.children.push(child_id);
        }
        if let Some(child) = self.node_mut(child_id) {
            child.parent = Some(parent_id);
        }
    }

    pub fn append_child(&mut self, parent_id: usize, child_id: usize) {
        self.set_child(parent_id, child_id);
    }

    pub fn set_focus(&mut self, _id: usize) {}

    /// Fire the callbacks registered on a node. Panics if `id` does not exist.
    ///
    /// This must be called outside any borrow of `self` that the callback might
    /// re-take; the implementation releases its borrow before invoking each
    /// callback.
    pub fn click(&mut self, id: usize) {
        let mut cbs = match self.node_mut(id) {
            Some(n) => std::mem::take(&mut n.callbacks),
            None => return,
        };
        for cb in cbs.iter_mut() {
            cb();
        }
        if let Some(n) = self.node_mut(id) {
            n.callbacks = cbs;
        }
    }

    /// Convenience: type into an entry and fire `changed` callbacks.
    pub fn type_into(&mut self, id: usize, text: &str) {
        self.set_entry_text(id, text);
        self.click(id);
    }

    /// Convenience: toggle a check/radio button and fire its callbacks.
    pub fn toggle(&mut self, id: usize) {
        let became = match self.node_mut(id) {
            Some(n) => match &mut n.kind {
                ZorkKind::CheckButton { ref mut checked, .. } => {
                    *checked = !*checked;
                    Some(*checked)
                }
                ZorkKind::RadioButton { ref mut checked, .. } => {
                    *checked = !*checked;
                    Some(*checked)
                }
                _ => None,
            },
            None,
        };
        if became.is_some() {
            self.click(id);
        }
    }

    pub fn quit(&mut self) {
        self.running = false;
    }

    /// Produce a serialization-friendly snapshot of the current model.
    ///
    /// This is the *only* sanctioned JSON surface: it is used for snapshot
    /// regression tests and for external (non-Rust) drivers that want to inspect
    /// the model without parsing the REPL's prose. It deliberately does not
    /// include callbacks.
    pub fn snapshot(&self) -> Snapshot {
        let nodes = self
            .nodes
            .iter()
            .map(|n| {
                let mut s = SnapshotNode {
                    id: n.id,
                    kind: match n.kind {
                        ZorkKind::Window { .. } => "Window",
                        ZorkKind::Button { .. } => "Button",
                        ZorkKind::Label { .. } => "Label",
                        ZorkKind::BoxWidget { .. } => "BoxWidget",
                        ZorkKind::Grid { .. } => "Grid",
                        ZorkKind::Entry { .. } => "Entry",
                        ZorkKind::CheckButton { .. } => "CheckButton",
                        ZorkKind::RadioButton { .. } => "RadioButton",
                        ZorkKind::Dialog { .. } => "Dialog",
                        ZorkKind::Menu => "Menu",
                        ZorkKind::MenuBar => "MenuBar",
                        ZorkKind::SimpleAction => "SimpleAction",
                        ZorkKind::DropDown { .. } => "DropDown",
                        ZorkKind::TextView { .. } => "TextView",
                    }
                    .to_string(),
                    parent: n.parent,
                    children: n.children.clone(),
                    title: None,
                    text: None,
                    label: None,
                    checked: None,
                    selected: None,
                    items: None,
                };
                match &n.kind {
                    ZorkKind::Window { title }
                    | ZorkKind::Dialog { title } => s.title = Some(title.clone()),
                    ZorkKind::Label { text } | ZorkKind::TextView { text } => s.text = Some(text.clone()),
                    ZorkKind::Button { label }
                    | ZorkKind::CheckButton { label, .. }
                    | ZorkKind::RadioButton { label, .. } => s.label = Some(label.clone()),
                    ZorkKind::CheckButton { checked, .. }
                    | ZorkKind::RadioButton { checked, .. } => s.checked = Some(*checked),
                    ZorkKind::DropDown { items, selected } => {
                        s.items = Some(items.clone());
                        s.selected = *selected;
                    }
                    _ => {}
                }
                s
            })
            .collect();
        Snapshot {
            nodes,
            menu_items: self.menu_items.clone(),
            current_id: self.current_id,
            running: self.running,
        }
    }
}

// -- Thread-local singleton used by the adapter shim --

thread_local! {
    static ZORK_STATE: RefCell<ZorkState> = RefCell::new(ZorkState::new());
}

pub(crate) fn with_state<F, R>(f: F) -> R
where
    F: FnOnce(&mut ZorkState) -> R,
{
    ZORK_STATE.with(|s| f(&mut s.borrow_mut()))
}
