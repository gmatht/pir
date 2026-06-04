use crate::ops::WorkbookState;
use std::path::PathBuf;

pub fn file_open_dialog() -> Option<PathBuf> {
    #[cfg(all(feature = "gui", not(feature = "pancurses")))]
    return rustxwidgets::App::init().ok().and_then(|app| {
        app.open_file("Open Spreadsheet").ok().flatten().map(PathBuf::from)
    });
    #[allow(unreachable_code)]
    None
}

pub fn file_save_dialog() -> Option<PathBuf> {
    #[cfg(all(feature = "gui", not(feature = "pancurses")))]
    return rustxwidgets::App::init().ok().and_then(|app| {
        app.save_file("Save Spreadsheet").ok().flatten().map(PathBuf::from)
    });
    #[allow(unreachable_code)]
    None
}

pub fn show_about_dialog() {
    eprintln!("corro {} - append-only collaborative spreadsheet", env!("CARGO_PKG_VERSION"));
}

pub fn show_keybinds_help() {
    eprintln!("Keybindings: arrows=navigate, Enter=edit, Esc=cancel, F1=help, Ctrl+Q=quit");
}

pub fn find_dialog() -> Option<String> {
    None
}

pub fn replace_dialog() -> Option<(String, String)> {
    None
}

pub fn sort_dialog(_workbook: &WorkbookState) -> Option<(usize, bool)> {
    None
}

pub fn balance_dialog() -> Option<String> {
    None
}
