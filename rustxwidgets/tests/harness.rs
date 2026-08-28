//! Tests for the typed headless [`Harness`] over the `zork` model.
//!
//! These exercise the same in-memory model the REPL drives, but without a string
//! protocol: clicks/toggles fire callbacks synchronously, and state is asserted
//! through typed getters or a JSON snapshot. This is the recommended replacement
//! for a JSON backend.

#![cfg(feature = "zork")]

use std::cell::RefCell;
use std::rc::Rc;

use rustxwidgets::backends::zork::harness::Harness;

#[test]
fn button_click_fires_callback_and_updates_label() {
    let h = Harness::new();
    let label = h.create_label("count: 0");
    let btn = h.create_button("inc");

    let counter = Rc::new(RefCell::new(0));
    {
        let c = counter.clone();
        let l = label.clone();
        btn.on_click(move || {
            let n = *c.borrow() + 1;
            *c.borrow_mut() = n;
            l.set_text(&format!("count: {}", n));
        });
    }

    assert_eq!(h.label_text(&label), Some("count: 0".to_string()));
    h.click(&btn);
    h.click(&btn);
    h.click(&btn);
    assert_eq!(h.label_text(&label), Some("count: 3".to_string()));
    assert_eq!(*counter.borrow(), 3);
}

#[test]
fn entry_type_into_fires_changed_and_sets_text() {
    let h = Harness::new();
    let entry = h.create_entry();
    let seen = Rc::new(RefCell::new(String::new()));
    {
        let s = seen.clone();
        let e = entry.clone();
        entry.connect_changed(move || {
            *s.borrow_mut() = e.id().to_string();
        });
    }
    h.type_into(&entry, "hello world");
    assert_eq!(h.entry_text(&entry), Some("hello world".to_string()));
    assert!(!seen.borrow().is_empty());
}

#[test]
fn checkbutton_toggle_updates_state() {
    let h = Harness::new();
    let cb = h.create_checkbutton("enable");
    assert!(!h.checkbutton_checked(&cb));
    h.toggle(&cb);
    assert!(h.checkbutton_checked(&cb));
    h.toggle(&cb);
    assert!(!h.checkbutton_checked(&cb));
}

#[test]
fn dropdown_selection_and_getters() {
    let h = Harness::new();
    let dd = h.create_dropdown(&["a", "b", "c"]);
    dd.set_active(1);
    assert_eq!(h.dropdown_selected(&dd), 1);
    dd.set_items(&["x", "y"]);
    dd.set_active(1);
    assert_eq!(h.dropdown_selected(&dd), 1);
}

#[test]
fn menu_items_and_select() {
    let h = Harness::new();
    let menu = h.create_menu();
    menu.append("Open", "app.open");
    menu.append("Save", "app.save");
    assert_eq!(h.menu_items(&menu).len(), 2);

    let fired = Rc::new(RefCell::new(false));
    {
        let f = fired.clone();
        menu.on_click(move || *f.borrow_mut() = true);
    }
    h.select(&menu, 1);
    assert!(*fired.borrow());

    // Out-of-range select must be ignored (no panic, no fire).
    *fired.borrow_mut() = false;
    h.select(&menu, 99);
    assert!(!*fired.borrow());
}

#[test]
fn snapshot_is_serializable_json() {
    let h = Harness::new();
    let win = h.create_window();
    win.set_title("Demo");
    let lbl = h.create_label("hello");
    h.set_child(&win, &lbl);

    let json = h.snapshot_json();
    // The window title and label text must be present in the snapshot.
    assert!(json.contains("Demo"), "snapshot missing window title: {json}");
    assert!(json.contains("hello"), "snapshot missing label text: {json}");

    // Valid JSON round-trips.
    let value: serde_json::Value = serde_json::from_str(&json).expect("snapshot is valid JSON");
    assert!(value.get("nodes").is_some());
    assert!(value.get("current_id").is_some());
}
