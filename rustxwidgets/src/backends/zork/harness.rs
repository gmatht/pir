//! Typed, synchronous in-process test harness over the
//! [`crate::backends::zork::model`].
//!
//! This is the recommended replacement for a JSON test backend. It drives the
//! exact same model the REPL does, but in plain Rust: widgets are returned as
//! typed, `Clone`-able handles, actions (`click`, `type_into`, `toggle`,
//! `select`) are synchronous and fire callbacks immediately, and state is
//! asserted via getters or [`ZorkState::snapshot`].
//!
//! Each widget handle owns a shared `Rc<RefCell<ZorkState>>`, so widget methods
//! (e.g. [`Label::set_text`]) do not require a `&Harness` reference and closures
//! can capture handles without borrowing the harness.
//!
//! ```
//! use std::rc::Rc;
//! use std::cell::Cell;
//! use rustxwidgets::backends::zork::harness::Harness;
//!
//! let h = Harness::new();
//! let label = h.create_label("count: 0");
//! let btn = h.create_button("inc");
//! let counter = Rc::new(Cell::new(0));
//! {
//!     let c = counter.clone();
//!     let l = label.clone();
//!     btn.on_click(move || {
//!         let n = c.get() + 1;
//!         c.set(n);
//!         l.set_text(&format!("count: {}", n));
//!     });
//! }
//! h.click(&btn);
//! h.click(&btn);
//! assert_eq!(h.label_text(&label), Some("count: 2".to_string()));
//! ```

use std::cell::RefCell;
use std::rc::Rc;

use crate::backends::zork::model::{MenuItemData, ZorkState};

/// Opaque, cloneable handle to a node in the harness model.
///
/// Cloning a handle is cheap: it shares the same underlying node id and the same
/// backing [`ZorkState`].
#[derive(Clone)]
pub struct Widget {
    pub(crate) id: usize,
    pub(crate) state: Rc<RefCell<ZorkState>>,
}

impl Widget {
    pub(crate) fn new(id: usize, state: Rc<RefCell<ZorkState>>) -> Self {
        Widget { id, state }
    }
}

/// Trait implemented by all typed widget handles so the harness can accept any
/// widget in action/getter methods.
pub trait AsId {
    fn id(&self) -> usize;
}

/// Typed test harness. Owns its own model (shared via [`Rc`] with the widget
/// handles it produces) so tests are isolated from the thread-local singleton and
/// from each other.
pub struct Harness {
    state: Rc<RefCell<ZorkState>>,
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

impl Harness {
    pub fn new() -> Self {
        Harness {
            state: Rc::new(RefCell::new(ZorkState::new())),
        }
    }

    /// Borrow the underlying model for direct assertions (e.g. via
    /// [`ZorkState::snapshot`]).
    pub fn with_state<R>(&self, f: impl FnOnce(&ZorkState) -> R) -> R {
        f(&self.state.borrow())
    }

    // -- creation --

    pub fn create_window(&self) -> Window {
        let id = self.state.borrow_mut().create_window();
        Window(Widget::new(id, self.state.clone()))
    }
    pub fn create_dialog(&self) -> Dialog {
        let id = self.state.borrow_mut().create_dialog();
        Dialog(Widget::new(id, self.state.clone()))
    }
    pub fn create_button(&self, label: &str) -> Button {
        let id = self.state.borrow_mut().create_button(label);
        Button(Widget::new(id, self.state.clone()))
    }
    pub fn create_label(&self, text: &str) -> Label {
        let id = self.state.borrow_mut().create_label(text);
        Label(Widget::new(id, self.state.clone()))
    }
    pub fn create_box(&self, horizontal: bool, spacing: i32) -> BoxWidget {
        let id = self.state.borrow_mut().create_box(horizontal, spacing);
        BoxWidget(Widget::new(id, self.state.clone()))
    }
    pub fn create_grid(&self) -> Grid {
        let id = self.state.borrow_mut().create_grid();
        Grid(Widget::new(id, self.state.clone()))
    }
    pub fn create_entry(&self) -> Entry {
        let id = self.state.borrow_mut().create_entry();
        Entry(Widget::new(id, self.state.clone()))
    }
    pub fn create_textview(&self) -> TextView {
        let id = self.state.borrow_mut().create_textview();
        TextView(Widget::new(id, self.state.clone()))
    }
    pub fn create_dropdown(&self, items: &[&str]) -> DropDown {
        let id = self.state.borrow_mut().create_dropdown(items);
        DropDown(Widget::new(id, self.state.clone()))
    }
    pub fn create_checkbutton(&self, label: &str) -> CheckButton {
        let id = self.state.borrow_mut().create_checkbutton(label);
        CheckButton(Widget::new(id, self.state.clone()))
    }
    pub fn create_radiobutton(&self, group: Option<&RadioButton>, label: &str) -> RadioButton {
        let gid = group.map(|r| r.0.id);
        let id = self.state.borrow_mut().create_radiobutton(gid, label);
        RadioButton(Widget::new(id, self.state.clone()))
    }
    pub fn create_menu(&self) -> Menu {
        let id = self.state.borrow_mut().create_menu();
        Menu(Widget::new(id, self.state.clone()))
    }

    // -- actions (operate on the shared state) --

    /// Fire the callbacks registered on a widget. For `Button` this is a click;
    /// for `CheckButton`/`RadioButton` it re-fires without toggling (use
    /// [`Self::toggle`] to toggle).
    ///
    /// Callbacks are taken out of the shared state and fired *after* releasing the
    /// borrow, so a callback may freely mutate the model (re-entrancy safe).
    pub fn click(&self, w: &impl AsId) {
        let id = w.id();
        let mut cbs = {
            let mut state = self.state.borrow_mut();
            match state.node_mut(id) {
                Some(n) => std::mem::take(&mut n.callbacks),
                None => return,
            }
        };
        for cb in cbs.iter_mut() {
            cb();
        }
        if let Some(n) = self.state.borrow_mut().node_mut(id) {
            n.callbacks = cbs;
        }
    }

    /// Take the callbacks off a node and fire them outside any borrow of the
    /// shared state (re-entrancy safe).
    fn fire_callbacks(&self, id: usize) {
        let mut cbs = {
            let mut state = self.state.borrow_mut();
            match state.node_mut(id) {
                Some(n) => std::mem::take(&mut n.callbacks),
                None => return,
            }
        };
        for cb in cbs.iter_mut() {
            cb();
        }
        if let Some(n) = self.state.borrow_mut().node_mut(id) {
            n.callbacks = cbs;
        }
    }

    /// Type text into an `Entry` and fire its `changed` callbacks.
    pub fn type_into(&self, w: &impl AsId, text: &str) {
        let id = w.id();
        self.state.borrow_mut().set_entry_text(id, text);
        self.fire_callbacks(id);
    }

    /// Toggle a `CheckButton` or `RadioButton` and fire its callbacks.
    pub fn toggle(&self, w: &impl AsId) {
        let id = w.id();
        self.state.borrow_mut().toggle(id);
        self.fire_callbacks(id);
    }

    /// Select a menu item (by 1-based index) on a `Menu`/`MenuBar` and fire its
    /// callbacks. Out-of-range indices are ignored.
    pub fn select(&self, w: &impl AsId, index: usize) {
        let id = w.id();
        let len = self.state.borrow().menu_items.get(&id).map(|i| i.len()).unwrap_or(0);
        if index == 0 || index > len {
            return;
        }
        self.fire_callbacks(id);
    }

    /// Set a child of a container widget (Window/Box/Grid/Dialog), replacing any
    /// existing parent.
    pub fn set_child(&self, parent: &impl AsId, child: &impl AsId) {
        let pid = parent.id();
        let cid = child.id();
        self.state.borrow_mut().set_child(pid, cid);
    }

    /// Append a child to a box/grid container.
    pub fn append(&self, parent: &impl AsId, child: &impl AsId) {
        let pid = parent.id();
        let cid = child.id();
        self.state.borrow_mut().append_child(pid, cid);
    }

    // -- getters --

    pub fn label_text(&self, w: &impl AsId) -> Option<String> {
        self.state.borrow().get_label_text(w.id())
    }
    pub fn entry_text(&self, w: &impl AsId) -> Option<String> {
        self.state.borrow().get_entry_text(w.id())
    }
    pub fn textview_text(&self, w: &impl AsId) -> Option<String> {
        self.state.borrow().get_textview_text(w.id())
    }
    pub fn checkbutton_checked(&self, w: &impl AsId) -> bool {
        self.state.borrow().get_checkbutton_checked(w.id())
    }
    pub fn radiobutton_checked(&self, w: &impl AsId) -> bool {
        self.state.borrow().get_radiobutton_checked(w.id())
    }
    pub fn dropdown_selected(&self, w: &impl AsId) -> i32 {
        self.state.borrow().get_dropdown_selected(w.id())
    }
    pub fn menu_items(&self, w: &impl AsId) -> Vec<MenuItemData> {
        self.state.borrow().menu_items.get(&w.id()).cloned().unwrap_or_default()
    }

    /// Serialize the current model to a JSON snapshot string.
    pub fn snapshot_json(&self) -> String {
        serde_json::to_string(&self.state.borrow().snapshot()).expect("snapshot serializes")
    }
}

macro_rules! widget_handle {
    ($name:ident) => {
        #[derive(Clone)]
        pub struct $name(Widget);
        impl AsId for $name {
            fn id(&self) -> usize {
                self.0.id
            }
        }
        impl $name {
            /// The internal node id (useful for diagnostics / snapshot lookups).
            pub fn id(&self) -> usize {
                self.0.id
            }
            /// Register a callback fired by [`Harness::click`].
            pub fn on_click(&self, f: impl FnMut() + 'static) {
                self.0.state.borrow_mut().add_callback(self.0.id, Box::new(f));
            }
        }
    };
}

widget_handle!(Window);
widget_handle!(Dialog);
widget_handle!(Button);
widget_handle!(Label);
widget_handle!(BoxWidget);
widget_handle!(Grid);
widget_handle!(Entry);
widget_handle!(TextView);
widget_handle!(DropDown);
widget_handle!(CheckButton);
widget_handle!(RadioButton);
widget_handle!(Menu);

impl Window {
    pub fn set_title(&self, title: &str) {
        self.0.state.borrow_mut().set_window_title(self.0.id, title);
    }
}

impl Label {
    pub fn set_text(&self, text: &str) {
        self.0.state.borrow_mut().set_label_text(self.0.id, text);
    }
}

impl Entry {
    pub fn set_text(&self, text: &str) {
        self.0.state.borrow_mut().set_entry_text(self.0.id, text);
    }
    pub fn connect_changed(&self, f: impl FnMut() + 'static) {
        self.0.state.borrow_mut().add_callback(self.0.id, Box::new(f));
    }
}

impl TextView {
    pub fn set_text(&self, text: &str) {
        self.0.state.borrow_mut().set_textview_text(self.0.id, text);
    }
}

impl CheckButton {
    pub fn set_checked(&self, checked: bool) {
        self.0.state.borrow_mut().set_checkbutton_checked(self.0.id, checked);
    }
    pub fn on_toggle(&self, f: impl FnMut() + 'static) {
        self.0.state.borrow_mut().add_callback(self.0.id, Box::new(f));
    }
}

impl RadioButton {
    pub fn set_checked(&self, checked: bool) {
        self.0.state.borrow_mut().set_radiobutton_checked(self.0.id, checked);
    }
    pub fn on_toggle(&self, f: impl FnMut() + 'static) {
        self.0.state.borrow_mut().add_callback(self.0.id, Box::new(f));
    }
}

impl DropDown {
    pub fn set_items(&self, items: &[&str]) {
        self.0.state.borrow_mut().set_dropdown_items(self.0.id, items);
    }
    pub fn set_active(&self, idx: i32) {
        self.0.state.borrow_mut().set_dropdown_selected(self.0.id, idx);
    }
    pub fn connect_changed(&self, f: impl FnMut() + 'static) {
        self.0.state.borrow_mut().add_callback(self.0.id, Box::new(f));
    }
}

impl Menu {
    pub fn append(&self, label: &str, action: &str) {
        self.0.state.borrow_mut().menu_append(self.0.id, label, action);
    }
}
