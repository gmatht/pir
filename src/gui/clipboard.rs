use std::collections::HashMap;

pub struct Clipboard {
    pub text: Option<String>,
    pub cells: Option<HashMap<(u32, u32), String>>,
}

impl Clipboard {
    pub fn new() -> Self {
        Clipboard { text: None, cells: None }
    }

    pub fn set_text(&mut self, text: &str) {
        self.text = Some(text.to_string());
    }

    pub fn get_text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    pub fn copy_cells(&mut self, cells: HashMap<(u32, u32), String>, selection_text: &str) {
        self.cells = Some(cells);
        self.text = Some(selection_text.to_string());
    }

    pub fn get_cells(&self) -> Option<&HashMap<(u32, u32), String>> {
        self.cells.as_ref()
    }
}
