#[cfg(feature = "pancurses")]
mod pancurses_adapter {
    use std::os::raw::c_void;
    use crate::core::{Error, Widget};

    // -- Window --

    pub struct Window {
        pub(crate) id: usize,
    }

    impl Widget for Window {
        fn raw_handle(&self) -> *mut c_void {
            &self.id as *const usize as *mut c_void
        }
    }

    impl AsRef<*mut c_void> for Window {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.id as *const usize as *const *mut c_void) }
        }
    }

    impl Clone for Window {
        fn clone(&self) -> Self { Window { id: self.id } }
    }

    impl Window {
        pub fn set_title(&self, title: &str) {
            crate::backends::pancurses::set_window_title(self.id, title);
        }

        pub fn set_child(&self, child: &impl AsRef<*mut c_void>) {
            let child_ptr = *child.as_ref();
            let child_id = child_ptr as usize;
            crate::backends::pancurses::set_child(self.id, child_id);
        }

        pub fn present(&self) {}

        pub fn insert_action_group(&self, _name: &str, _group_ptr: *mut c_void) {}

        pub fn set_default_size(&self, _width: i32, _height: i32) {}
    }

    // -- Button --

    pub struct Button {
        pub(crate) id: usize,
    }

    impl Widget for Button {
        fn raw_handle(&self) -> *mut c_void {
            &self.id as *const usize as *mut c_void
        }
    }

    impl AsRef<*mut c_void> for Button {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.id as *const usize as *const *mut c_void) }
        }
    }

    impl Clone for Button {
        fn clone(&self) -> Self { Button { id: self.id } }
    }

    impl Button {
        pub fn on_click(&self, f: impl FnMut() + 'static) -> Result<u64, Error> {
            crate::backends::pancurses::add_callback(self.id, Box::new(f));
            Ok(0)
        }

        pub fn emit_clicked(&self) -> Result<u64, Error> {
            // fire synchronously
            Ok(0)
        }
    }

    // -- Label --

    pub struct Label {
        pub(crate) id: usize,
    }

    impl AsRef<*mut c_void> for Label {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.id as *const usize as *const *mut c_void) }
        }
    }

    impl Clone for Label {
        fn clone(&self) -> Self { Label { id: self.id } }
    }

    impl Label {
        pub fn set_text(&self, text: &str) {
            crate::backends::pancurses::set_label_text(self.id, text);
        }

        pub fn get_text(&self) -> Option<String> {
            crate::backends::pancurses::get_label_text(self.id)
        }

        pub fn add_class(&self, _class_name: &str) {}
        pub fn remove_class(&self, _class_name: &str) {}
        pub fn set_markup(&self, _markup: &str) {}
        pub fn set_visible(&self, visible: bool) {
            crate::backends::pancurses::set_label_visible(self.id, visible);
        }
        pub fn set_xalign(&self, _x: f32) {}
    }

    // -- BoxWidget --

    pub struct BoxWidget {
        pub(crate) id: usize,
        pub(crate) orientation: Orientation,
        pub(crate) spacing: i32,
    }

    #[derive(Clone, Copy, PartialEq)]
    pub enum Orientation {
        Horizontal,
        Vertical,
    }

    impl AsRef<*mut c_void> for BoxWidget {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.id as *const usize as *const *mut c_void) }
        }
    }

    impl BoxWidget {
        pub fn append(&self, child: &impl AsRef<*mut c_void>) {
            let child_ptr = *child.as_ref();
            let child_id = child_ptr as usize;
            crate::backends::pancurses::append_child(self.id, child_id);
        }

        pub fn layout(&self, _x: i32, _y: i32, _w: i32, _h: i32) {
            crate::backends::pancurses::layout_box(self.id);
        }
    }

    // -- Grid --

    pub struct Grid {
        pub(crate) id: usize,
    }

    impl AsRef<*mut c_void> for Grid {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.id as *const usize as *const *mut c_void) }
        }
    }

    impl Grid {
        pub fn attach(&self, child: &impl AsRef<*mut c_void>, _left: i32, _top: i32, _width: i32, _height: i32) {
            let child_ptr = *child.as_ref();
            let child_id = child_ptr as usize;
            crate::backends::pancurses::append_child(self.id, child_id);
        }

        pub fn layout(&self) {
            crate::backends::pancurses::layout_grid(self.id);
        }
    }

    // -- Entry --

    pub struct Entry {
        pub(crate) id: usize,
    }

    impl AsRef<*mut c_void> for Entry {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.id as *const usize as *const *mut c_void) }
        }
    }

    impl Clone for Entry {
        fn clone(&self) -> Self { Entry { id: self.id } }
    }

    impl Entry {
        pub fn set_text(&self, text: &str) {
            crate::backends::pancurses::entry_set_text(self.id, text);
        }

        pub fn text(&self) -> Option<String> {
            crate::backends::pancurses::entry_text(self.id)
        }

        pub fn get_text(&self) -> Option<String> {
            self.text()
        }

        pub fn connect_changed(&self, f: impl FnMut() + 'static) -> Result<u64, Error> {
            crate::backends::pancurses::add_callback(self.id, Box::new(f));
            Ok(0)
        }
    }

    // -- Menu --

    pub struct Menu {
        pub(crate) id: usize,
        pub(crate) items: std::cell::RefCell<Vec<(String, String)>>,
        submenu_data: std::cell::RefCell<Vec<(String, Vec<(String, String)>)>>,
    }

    impl AsRef<*mut c_void> for Menu {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.id as *const usize as *const *mut c_void) }
        }
    }

    impl Menu {
        pub fn append(&self, label: &str, action_name: &str) {
            self.items.borrow_mut().push((label.to_string(), action_name.to_string()));
        }
        pub fn append_submenu(&self, label: &str, submenu: &Menu) {
            let sub_items = submenu.items.borrow().clone();
            self.submenu_data.borrow_mut().push((label.to_string(), sub_items));
        }
        pub fn append_item(&self, _label: &str, _action: &SimpleAction) {}
        pub fn append_section(&self, _label: &str) {}
    }

    pub(crate) fn collect_menu_items(menu: &Menu) -> (Vec<String>, Vec<Vec<(String, String)>>) {
        let submenus = menu.submenu_data.borrow();
        let mut labels = Vec::new();
        let mut items_list = Vec::new();
        for (label, items) in submenus.iter() {
            labels.push(label.clone());
            items_list.push(items.clone());
        }
        (labels, items_list)
    }

    // -- MenuBar --

    pub struct MenuBar {
        pub(crate) id: usize,
    }

    impl Widget for MenuBar {
        fn raw_handle(&self) -> *mut c_void {
            &self.id as *const usize as *mut c_void
        }
    }

    impl AsRef<*mut c_void> for MenuBar {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.id as *const usize as *const *mut c_void) }
        }
    }

    // -- SimpleAction --

    pub struct SimpleAction {
        pub(crate) id: usize,
    }

    impl AsRef<*mut c_void> for SimpleAction {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.id as *const usize as *const *mut c_void) }
        }
    }

    impl SimpleAction {
        pub fn on_activate(&self, f: impl FnMut() + 'static) -> Result<u64, Error> {
            crate::backends::pancurses::add_callback(self.id, Box::new(f));
            Ok(0)
        }
    }

    // -- Dialog --

    pub struct Dialog {
        pub(crate) id: usize,
    }

    impl Widget for Dialog {
        fn raw_handle(&self) -> *mut c_void {
            &self.id as *const usize as *mut c_void
        }
    }

    impl AsRef<*mut c_void> for Dialog {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.id as *const usize as *const *mut c_void) }
        }
    }

    impl Dialog {
        pub fn set_title(&self, title: &str) {
            crate::backends::pancurses::set_window_title(self.id, title);
        }
        pub fn set_default_size(&self, _w: i32, _h: i32) {}
        pub fn append_content_area(&self, child: &impl AsRef<*mut c_void>) {
            let child_ptr = *child.as_ref();
            let child_id = child_ptr as usize;
            crate::backends::pancurses::set_child(self.id, child_id);
        }
        pub fn add_button(&self, _label: &str, _response_id: i32) {}
        pub fn connect_response(&self, _f: impl FnMut(i32) + 'static) -> Result<u64, Error> { Ok(0) }
        pub fn present(&self) {}
    }

    // -- DropDown --

    pub struct DropDown {
        pub(crate) id: usize,
    }

    impl Widget for DropDown {
        fn raw_handle(&self) -> *mut c_void {
            &self.id as *const usize as *mut c_void
        }
    }

    impl AsRef<*mut c_void> for DropDown {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.id as *const usize as *const *mut c_void) }
        }
    }

    impl Clone for DropDown {
        fn clone(&self) -> Self { DropDown { id: self.id } }
    }

    impl DropDown {
        pub fn set_items(&self, items: &[&str]) {
            crate::backends::pancurses::set_dropdown_items(self.id, items);
        }
        pub fn set_active(&self, idx: i32) {
            crate::backends::pancurses::set_dropdown_selected(self.id, idx);
        }
        pub fn get_active(&self) -> i32 {
            crate::backends::pancurses::get_dropdown_selected(self.id)
        }
        pub fn connect_changed(&self, f: impl FnMut() + 'static) -> Result<u64, Error> {
            crate::backends::pancurses::add_callback(self.id, Box::new(f));
            Ok(0)
        }
    }

    // -- CheckButton --

    pub struct CheckButton {
        pub(crate) id: usize,
    }

    impl Widget for CheckButton {
        fn raw_handle(&self) -> *mut c_void {
            &self.id as *const usize as *mut c_void
        }
    }

    impl AsRef<*mut c_void> for CheckButton {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.id as *const usize as *const *mut c_void) }
        }
    }

    impl Clone for CheckButton {
        fn clone(&self) -> Self { CheckButton { id: self.id } }
    }

    impl CheckButton {
        pub fn set_active(&self, active: bool) {
            crate::backends::pancurses::set_checkbutton_checked(self.id, active);
        }

        pub fn is_active(&self) -> bool {
            crate::backends::pancurses::get_checkbutton_checked(self.id)
        }

        pub fn on_toggle(&self, f: impl FnMut() + 'static) -> Result<u64, Error> {
            crate::backends::pancurses::add_callback(self.id, Box::new(f));
            Ok(0)
        }

        pub fn connect_toggled(&self, f: impl FnMut() + 'static) -> Result<u64, Error> {
            self.on_toggle(f)
        }
    }

    // -- RadioButton --

    pub struct RadioButton {
        pub(crate) id: usize,
    }

    impl Widget for RadioButton {
        fn raw_handle(&self) -> *mut c_void {
            &self.id as *const usize as *mut c_void
        }
    }

    impl AsRef<*mut c_void> for RadioButton {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.id as *const usize as *const *mut c_void) }
        }
    }

    impl Clone for RadioButton {
        fn clone(&self) -> Self { RadioButton { id: self.id } }
    }

    impl RadioButton {
        pub fn set_active(&self, active: bool) {
            crate::backends::pancurses::set_radiobutton_checked(self.id, active);
        }

        pub fn is_active(&self) -> bool {
            crate::backends::pancurses::get_radiobutton_checked(self.id)
        }

        pub fn on_toggle(&self, f: impl FnMut() + 'static) -> Result<u64, Error> {
            crate::backends::pancurses::add_callback(self.id, Box::new(f));
            Ok(0)
        }

        pub fn connect_toggled(&self, f: impl FnMut() + 'static) -> Result<u64, Error> {
            self.on_toggle(f)
        }
    }

    // -- TextView --

    pub struct TextView {
        pub(crate) id: usize,
    }

    impl AsRef<*mut c_void> for TextView {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.id as *const usize as *const *mut c_void) }
        }
    }

    impl Widget for TextView {
        fn raw_handle(&self) -> *mut c_void {
            &self.id as *const usize as *mut c_void
        }
    }

    impl TextView {
        pub fn set_text(&self, text: &str) {
            crate::backends::pancurses::set_textview_text(self.id, text);
        }
        pub fn get_text(&self) -> Option<String> {
            crate::backends::pancurses::get_textview_text(self.id)
        }
        pub fn set_wrap_mode(&self, _mode: i32) {}
        pub fn set_size_request(&self, _w: i32, _h: i32) {}
    }

    // -- Spreadsheet --

    pub struct Spreadsheet {
        pub(crate) id: usize,
    }

    impl Clone for Spreadsheet {
        fn clone(&self) -> Self { Spreadsheet { id: self.id } }
    }

    impl AsRef<*mut c_void> for Spreadsheet {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.id as *const usize as *const *mut c_void) }
        }
    }

    impl Widget for Spreadsheet {
        fn raw_handle(&self) -> *mut c_void {
            &self.id as *const usize as *mut c_void
        }
    }

    impl Spreadsheet {
        pub fn id(&self) -> usize { self.id }
        pub fn set_cell(&self, row: u32, col: u32, text: &str) {
            crate::backends::pancurses::spreadsheet_set_cell(self.id, row, col, text);
        }
        pub fn get_cell(&self, row: u32, col: u32) -> Option<String> {
            crate::backends::pancurses::spreadsheet_get_cell(self.id, row, col)
        }
        pub fn set_raw_cell(&self, row: u32, col: u32, text: &str) {
            crate::backends::pancurses::spreadsheet_set_raw_cell(self.id, row, col, text);
        }
        pub fn set_cell_style(&self, row: u32, col: u32, style: u8) {
            crate::backends::pancurses::spreadsheet_set_cell_style(self.id, row, col, style);
        }
        pub fn cursor_position(&self) -> Option<(u32, u32)> {
            crate::backends::pancurses::spreadsheet_cursor_position(self.id)
        }
        pub fn set_cursor(&self, row: u32, col: u32) {
            crate::backends::pancurses::spreadsheet_set_cursor(self.id, row, col);
        }
        pub fn set_editing(&self, editing: bool, edit_buf: &str, edit_pos: usize) {
            crate::backends::pancurses::spreadsheet_set_edit_state(self.id, editing, edit_buf, edit_pos);
        }
        pub fn set_grid_config(&self, margin_cols: u32, main_cols: u32) {
            crate::backends::pancurses::spreadsheet_set_grid_config(self.id, margin_cols, main_cols);
        }
        pub fn set_row_counts(&self, header_rows: u32, main_rows: u32) {
            crate::backends::pancurses::spreadsheet_set_row_counts(self.id, header_rows, main_rows);
        }
        pub fn set_column_layout(&self, layout: Vec<(u32, u32, String)>) {
            crate::backends::pancurses::spreadsheet_set_column_layout(self.id, layout);
        }
        pub fn set_row_labels(&self, labels: Vec<(u32, String)>) {
            crate::backends::pancurses::spreadsheet_set_row_labels(self.id, labels);
        }
        pub fn set_menu_text(&self, text: &str) {
            crate::backends::pancurses::spreadsheet_set_menu_text(self.id, text);
        }
        pub fn set_border_title(&self, text: &str) {
            crate::backends::pancurses::spreadsheet_set_border_title(self.id, text);
        }
        pub fn set_status_text(&self, text: &str) {
            crate::backends::pancurses::spreadsheet_set_status_text(self.id, text);
        }
        pub fn set_formula_bar_trailing(&self, text: &str) {
            crate::backends::pancurses::spreadsheet_set_formula_bar_trailing(self.id, text);
        }
        pub fn set_tab_data(&self, titles: &[String], active: usize) {
            crate::backends::pancurses::spreadsheet_set_tab_data(self.id, titles, active);
        }
        pub fn set_formula_bar(&self, address_label: &Label, entry: &Entry) {
            crate::backends::pancurses::spreadsheet_set_formula_bar(
                self.id, address_label.id, entry.id,
            );
        }
        pub fn commit_formula_bar(&self) {
            crate::backends::pancurses::spreadsheet_commit_formula_bar(self.id);
        }
    }

    // -- Factory functions --

    pub fn create_window() -> Result<Window, Error> {
        crate::backends::pancurses::create_window()
            .map(|id| Window { id })
            .map_err(|e| Error::Backend(format!("{}", e)))
    }

    pub fn create_button(label: &str) -> Result<Button, Error> {
        crate::backends::pancurses::create_button(label)
            .map(|id| Button { id })
            .map_err(|e| Error::Backend(format!("{}", e)))
    }

    pub fn create_label(text: &str) -> Result<Label, Error> {
        crate::backends::pancurses::create_label(text)
            .map(|id| Label { id })
            .map_err(|e| Error::Backend(format!("{}", e)))
    }

    pub fn create_box(orientation: Orientation, spacing: i32) -> Result<BoxWidget, Error> {
        let horizontal = orientation == Orientation::Horizontal;
        crate::backends::pancurses::create_box(horizontal, spacing)
            .map(|id| BoxWidget { id, orientation, spacing })
            .map_err(|e| Error::Backend(format!("{}", e)))
    }

    pub fn create_grid() -> Result<Grid, Error> {
        crate::backends::pancurses::create_grid()
            .map(|id| Grid { id })
            .map_err(|e| Error::Backend(format!("{}", e)))
    }

    pub fn create_entry() -> Result<Entry, Error> {
        crate::backends::pancurses::create_entry()
            .map(|id| Entry { id })
            .map_err(|e| Error::Backend(format!("{}", e)))
    }

    pub fn create_menu() -> Result<Menu, Error> {
        crate::backends::pancurses::create_menu()
            .map(|id| Menu { id, items: std::cell::RefCell::new(vec![]), submenu_data: std::cell::RefCell::new(vec![]) })
            .map_err(|e| Error::Backend(format!("{}", e)))
    }

    pub fn create_menubar(model: &Menu, _action_group: *mut c_void) -> Result<MenuBar, Error> {
        let (labels, itemss) = collect_menu_items(model);
        let submenu_items: Vec<(String, Vec<(String, String)>)> = labels.into_iter().zip(itemss.into_iter()).collect();
        let id = unsafe { crate::backends::pancurses::create_menubar(submenu_items, _action_group) }
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(MenuBar { id })
    }

    pub fn create_simple_action(name: &str) -> Result<SimpleAction, Error> {
        crate::backends::pancurses::create_simple_action(name)
            .map(|id| SimpleAction { id })
            .map_err(|e| Error::Backend(format!("{}", e)))
    }

    pub fn create_dialog() -> Result<Dialog, Error> {
        crate::backends::pancurses::create_dialog()
            .map(|id| Dialog { id })
            .map_err(|e| Error::Backend(format!("{}", e)))
    }

    pub fn create_dropdown(items: &[&str]) -> Result<DropDown, Error> {
        crate::backends::pancurses::create_dropdown(items)
            .map(|id| DropDown { id })
            .map_err(|e| Error::Backend(format!("{}", e)))
    }

    pub fn create_checkbutton(label: &str) -> Result<CheckButton, Error> {
        crate::backends::pancurses::create_checkbutton(label)
            .map(|id| CheckButton { id })
            .map_err(|e| Error::Backend(format!("{}", e)))
    }

    pub fn create_radiobutton(group: Option<&RadioButton>, label: &str) -> Result<RadioButton, Error> {
        let gid = group.map(|r| r.id);
        crate::backends::pancurses::create_radiobutton(gid, label)
            .map(|id| RadioButton { id })
            .map_err(|e| Error::Backend(format!("{}", e)))
    }

    pub fn create_textview() -> Result<TextView, Error> {
        crate::backends::pancurses::create_textview()
            .map(|id| TextView { id })
            .map_err(|e| Error::Backend(format!("{}", e)))
    }

    pub fn create_spreadsheet(rows: u32, cols: u32) -> Result<Spreadsheet, Error> {
        crate::backends::pancurses::create_spreadsheet(rows, cols)
            .map(|id| Spreadsheet { id })
            .map_err(|e| Error::Backend(format!("{}", e)))
    }

    pub fn add_cursor_move_callback<F: FnMut(u32, u32) + 'static>(f: F) {
        crate::backends::pancurses::spreadsheet_add_cursor_move_callback(f);
    }
    pub fn add_commit_edit_callback<F: FnMut(u32, u32, String) + 'static>(f: F) {
        crate::backends::pancurses::spreadsheet_add_commit_edit_callback(f);
    }
    pub fn spreadsheet_set_cell(id: usize, r: u32, c: u32, text: &str) {
        crate::backends::pancurses::spreadsheet_set_cell(id, r, c, text);
    }
    pub fn spreadsheet_set_cell_style(id: usize, r: u32, c: u32, style: u8) {
        crate::backends::pancurses::spreadsheet_set_cell_style(id, r, c, style);
    }
    pub fn spreadsheet_set_column_layout(id: usize, layout: Vec<(u32, u32, String)>) {
        crate::backends::pancurses::spreadsheet_set_column_layout(id, layout);
    }
    pub fn spreadsheet_set_border_title(id: usize, text: &str) {
        crate::backends::pancurses::spreadsheet_set_border_title(id, text);
    }
    pub fn spreadsheet_set_row_labels(id: usize, labels: Vec<(u32, String)>) {
        crate::backends::pancurses::spreadsheet_set_row_labels(id, labels);
    }
    pub fn spreadsheet_set_grid_config(id: usize, margin_cols: u32, main_cols: u32) {
        crate::backends::pancurses::spreadsheet_set_grid_config(id, margin_cols, main_cols);
    }
    pub fn spreadsheet_set_edit_state(id: usize, editing: bool, edit_buf: &str, edit_pos: usize) {
        crate::backends::pancurses::spreadsheet_set_edit_state(id, editing, edit_buf, edit_pos);
    }
}

pub use pancurses_adapter::*;
