//! Backwards-compatible free-function facade over [`super::model`].
//!
//! The per-widget adapter ([`crate::backends_zork_adapter`]) was written against
//! the old `backends::zork::*` free functions, which mutably operated on a
//! thread-local model. This module re-exposes those names (each delegating to
//! [`super::model::with_state`]) so the adapter is unchanged. New code should use
//! the typed [`super::harness`] instead.

use std::os::raw::c_void;

use super::model::{with_state, MenuItemData};

pub type Callback = Box<dyn FnMut()>;

pub fn create_window() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.create_window()))
}
pub fn create_button(label: &str) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.create_button(label)))
}
pub fn create_label(text: &str) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.create_label(text)))
}
pub fn create_box(horizontal: bool, spacing: i32) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.create_box(horizontal, spacing)))
}
pub fn create_grid() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.create_grid()))
}
pub fn create_entry() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.create_entry()))
}
pub fn create_menu() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.create_menu()))
}
pub fn create_simple_action(_name: &str) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.create_simple_action(_name)))
}
pub fn create_menubar(model_id: usize, action_group: *mut c_void) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.create_menubar(model_id, action_group)))
}
pub fn create_dialog() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.create_dialog()))
}
pub fn create_dropdown(items: &[&str]) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.create_dropdown(items)))
}
pub fn create_checkbutton(label: &str) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.create_checkbutton(label)))
}
pub fn create_radiobutton(group_id: Option<usize>, label: &str) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.create_radiobutton(group_id, label)))
}
pub fn create_textview() -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    Ok(with_state(|s| s.create_textview()))
}

pub fn menu_append(menu_id: usize, label: &str, action: &str) {
    with_state(|s| s.menu_append(menu_id, label, action));
}
pub fn menu_append_submenu(menu_id: usize, label: &str, submenu_id: usize) {
    with_state(|s| s.menu_append_submenu(menu_id, label, submenu_id));
}

pub fn set_window_title(id: usize, title: &str) {
    with_state(|s| s.set_window_title(id, title));
}
pub fn set_label_text(id: usize, text: &str) {
    with_state(|s| s.set_label_text(id, text));
}
pub fn get_label_text(id: usize) -> Option<String> {
    with_state(|s| s.get_label_text(id))
}
pub fn add_callback(id: usize, cb: Callback) {
    with_state(|s| s.add_callback(id, cb));
}
pub fn set_entry_text(id: usize, text: &str) {
    with_state(|s| s.set_entry_text(id, text));
}
pub fn get_entry_text(id: usize) -> Option<String> {
    with_state(|s| s.get_entry_text(id))
}
pub fn set_textview_text(id: usize, text: &str) {
    with_state(|s| s.set_textview_text(id, text));
}
pub fn get_textview_text(id: usize) -> Option<String> {
    with_state(|s| s.get_textview_text(id))
}
pub fn entry_set_text(id: usize, text: &str) {
    set_entry_text(id, text);
}
pub fn entry_text(id: usize) -> Option<String> {
    get_entry_text(id)
}
pub fn set_dropdown_items(id: usize, items: &[&str]) {
    with_state(|s| s.set_dropdown_items(id, items));
}
pub fn set_dropdown_selected(id: usize, idx: i32) {
    with_state(|s| s.set_dropdown_selected(id, idx));
}
pub fn get_dropdown_selected(id: usize) -> i32 {
    with_state(|s| s.get_dropdown_selected(id))
}
pub fn get_checkbutton_checked(id: usize) -> bool {
    with_state(|s| s.get_checkbutton_checked(id))
}
pub fn get_radiobutton_checked(id: usize) -> bool {
    with_state(|s| s.get_radiobutton_checked(id))
}
pub fn set_checkbutton_checked(id: usize, checked: bool) {
    with_state(|s| s.set_checkbutton_checked(id, checked));
}
pub fn set_radiobutton_checked(id: usize, checked: bool) {
    with_state(|s| s.set_radiobutton_checked(id, checked));
}
pub fn set_child(parent_id: usize, child_id: usize) {
    with_state(|s| s.set_child(parent_id, child_id));
}
pub fn append_child(parent_id: usize, child_id: usize) {
    with_state(|s| s.append_child(parent_id, child_id));
}
pub fn layout_box(_id: usize) {}
pub fn layout_grid(_id: usize) {}
pub fn set_focus(_id: usize) {}
pub fn quit() {
    with_state(|s| s.quit());
}

/// Convenience re-export so adapter code referencing `MenuItemData` keeps working.
pub use MenuItemData as ZorkMenuItemData;
