pub struct SharedState {
    pub cursor_row: std::cell::Cell<u32>,
    pub cursor_col: std::cell::Cell<u32>,
    pub scroll_row: std::cell::Cell<u32>,
    pub scroll_col: std::cell::Cell<u32>,
    pub edit_buf: std::cell::RefCell<String>,
    pub editing: std::cell::Cell<bool>,
    pub data: std::cell::RefCell<std::collections::HashMap<(u32, u32), String>>,
}
