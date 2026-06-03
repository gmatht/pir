// Minimal editor + formula bar glue for demo. We'll export a simple function to build UI controls
use crate::widget::sheet::{Sheet, Row, Col};
use gtk_compat::{Loader, BoxWidget, Orientation, Entry, Button, Grid, Label};
use std::sync::Arc;
use std::rc::Rc;

pub struct EditorUi {
    pub col_entry: Entry,
    pub row_entry: Entry,
    pub go_btn: Button,
}

impl EditorUi {
    pub fn new(loader: Arc<Loader>) -> Result<Self, Box<dyn std::error::Error>> {
        let col_entry = Entry::new(loader.clone())?;
        let row_entry = Entry::new(loader.clone())?;
        let go_btn = Button::with_label(loader.clone(), "Go")?;
        Ok(EditorUi { col_entry, row_entry, go_btn })
    }
}
