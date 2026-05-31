// Re-export the adapter functions implemented in gtk_adapter.rs
// Re-export the adapter implemented in src/backends/gtk_adapter.rs
pub use crate::backends_gtk_adapter_impl::*;
pub use crate::backends_gtk_adapter_impl::{create_window, create_button, create_label, create_box, create_grid, create_entry, create_menu, create_menubar, create_simple_action, Window, Button, Label, BoxWidget, Grid, Entry, Menu, MenuBar, SimpleAction};
