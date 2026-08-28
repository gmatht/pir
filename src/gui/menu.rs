use crate::gui::dialogs;
use rustxwidgets::{Menu, SimpleAction};

pub struct MenuAction {
    pub label: &'static str,
    pub shortcut: &'static str,
    pub action: MenuActionKind,
}

#[derive(Clone, Copy)]
pub enum MenuActionKind {
    Open,
    Save,
    SaveAs,
    Quit,
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    Find,
    Replace,
    DeleteCell,
    SelectAll,
    ToggleHeaders,
    ToggleMargins,
    NewSheet,
    RenameSheet,
    DeleteSheet,
    SortAsc,
    SortDesc,
    BalanceBooks,
    ExportTsv,
    ExportCsv,
    ExportOds,
    ExportAscii,
    About,
    HelpKeybinds,
}

pub fn action_kind_to_name(kind: MenuActionKind) -> &'static str {
    match kind {
        MenuActionKind::Open => "open",
        MenuActionKind::Save => "save",
        MenuActionKind::SaveAs => "save_as",
        MenuActionKind::Quit => "quit",
        MenuActionKind::Undo => "undo",
        MenuActionKind::Redo => "redo",
        MenuActionKind::Cut => "cut",
        MenuActionKind::Copy => "copy",
        MenuActionKind::Paste => "paste",
        MenuActionKind::Find => "find",
        MenuActionKind::Replace => "replace",
        MenuActionKind::DeleteCell => "delete_cell",
        MenuActionKind::SelectAll => "select_all",
        MenuActionKind::ToggleHeaders => "toggle_headers",
        MenuActionKind::ToggleMargins => "toggle_margins",
        MenuActionKind::NewSheet => "new_sheet",
        MenuActionKind::RenameSheet => "rename_sheet",
        MenuActionKind::DeleteSheet => "delete_sheet",
        MenuActionKind::SortAsc => "sort_asc",
        MenuActionKind::SortDesc => "sort_desc",
        MenuActionKind::BalanceBooks => "balance_books",
        MenuActionKind::ExportTsv => "export_tsv",
        MenuActionKind::ExportCsv => "export_csv",
        MenuActionKind::ExportOds => "export_ods",
        MenuActionKind::ExportAscii => "export_ascii",
        MenuActionKind::About => "about",
        MenuActionKind::HelpKeybinds => "help_keybinds",
    }
}

/// Build a submenu model from action descriptors.
pub fn build_submenu(rxapp: &rustxwidgets::App, items: &[MenuAction], prefix: &str) -> Result<Menu, Box<dyn std::error::Error>> {
    let mut menu = rxapp.create_menu()?;
    for item in items {
        let name = action_kind_to_name(item.action);
        menu.append(item.label, &format!("{}.{}", prefix, name));
    }
    Ok(menu)
}

/// Create a SimpleAction, connect its callback, and register it.
pub fn register_action<F: FnMut() + 'static>(
    rxapp: &rustxwidgets::App,
    name: &str,
    mut f: F,
) -> Result<SimpleAction, Box<dyn std::error::Error>> {
    let action = rxapp.create_simple_action(name)?;
    #[cfg(feature = "pancurses")]
    action.on_activate(move || f());
    #[cfg(not(feature = "pancurses"))]
    action.connect_activate(move |_| f())?;
    #[cfg(not(feature = "pancurses"))]
    rxapp.register_action(&action)?;
    Ok(action)
}

/// Execute a menu action by name, wiring it to the appropriate dialog or stub.
/// This is called when a menu item is activated.
pub fn handle_action(name: &str) {
    match name {
        "open" => {
            if let Some(path) = dialogs::file_open_dialog() {
                eprintln!("Open file: {:?}", path);
            }
        }
        "save" => {
            if let Some(path) = dialogs::file_save_dialog() {
                eprintln!("Save file: {:?}", path);
            }
        }
        "save_as" => {
            if let Some(path) = dialogs::file_save_dialog() {
                eprintln!("Save file as: {:?}", path);
            }
        }
        "quit" => {
            #[cfg(all(unix, not(feature = "pancurses")))]
            let _ = rustxwidgets::backends_gtk_adapter::quit_main_loop();
            #[cfg(all(windows, not(feature = "pancurses")))]
            rustxwidgets::backends_nwg_adapter::quit_main_loop();
        }
        "find" => dialogs::find_dialog(|result| {
            if let Some(text) = result {
                eprintln!("Find: {}", text);
            }
        }),
        "replace" => dialogs::replace_dialog(|result| {
            if let Some((find, replace)) = result {
                eprintln!("Replace: '{}' with '{}'", find, replace);
            }
        }),
        "sort_asc" => {
            let wb = crate::ops::WorkbookState::default();
            dialogs::sort_dialog(&wb, |result| {
                if let Some((col, asc)) = result {
                    eprintln!("Sort col {} asc: {}", col, asc);
                }
            });
        }
        "sort_desc" => {
            let wb = crate::ops::WorkbookState::default();
            dialogs::sort_dialog(&wb, |result| {
                if let Some((col, asc)) = result {
                    eprintln!("Sort col {} desc: {}", col, !asc);
                }
            });
        }
        "balance_books" => dialogs::balance_dialog(|result| {
            if let Some(col) = result {
                eprintln!("Balance col: {}", col);
            }
        }),
        "about" => dialogs::show_about_dialog(),
        "help_keybinds" => dialogs::show_keybinds_help(),
        "rename_sheet" => dialogs::find_dialog(|result| {
            if let Some(name) = result {
                eprintln!("Rename sheet to: {}", name);
            }
        }),
        _ => eprintln!("Menu action: {name}"),
    }
}

pub const FILE_MENU: &[MenuAction] = &[
    MenuAction { label: "Open",     shortcut: "Ctrl+O", action: MenuActionKind::Open },
    MenuAction { label: "Save",     shortcut: "Ctrl+S", action: MenuActionKind::Save },
    MenuAction { label: "Save As",  shortcut: "Ctrl+Shift+S", action: MenuActionKind::SaveAs },
    MenuAction { label: "Export TSV",  shortcut: "", action: MenuActionKind::ExportTsv },
    MenuAction { label: "Export CSV",  shortcut: "", action: MenuActionKind::ExportCsv },
    MenuAction { label: "Export ODS",  shortcut: "", action: MenuActionKind::ExportOds },
    MenuAction { label: "Export ASCII", shortcut: "", action: MenuActionKind::ExportAscii },
    MenuAction { label: "Quit",     shortcut: "Ctrl+Q", action: MenuActionKind::Quit },
];

pub const EDIT_MENU: &[MenuAction] = &[
    MenuAction { label: "Undo",         shortcut: "Ctrl+Z", action: MenuActionKind::Undo },
    MenuAction { label: "Redo",         shortcut: "Ctrl+Y", action: MenuActionKind::Redo },
    MenuAction { label: "Cut",          shortcut: "Ctrl+X", action: MenuActionKind::Cut },
    MenuAction { label: "Copy",         shortcut: "Ctrl+C", action: MenuActionKind::Copy },
    MenuAction { label: "Paste",        shortcut: "Ctrl+V", action: MenuActionKind::Paste },
    MenuAction { label: "Delete",       shortcut: "Del",    action: MenuActionKind::DeleteCell },
    MenuAction { label: "Select All",   shortcut: "Ctrl+A", action: MenuActionKind::SelectAll },
    MenuAction { label: "Find",         shortcut: "Ctrl+F", action: MenuActionKind::Find },
    MenuAction { label: "Replace",      shortcut: "Ctrl+H", action: MenuActionKind::Replace },
];

pub const VIEW_MENU: &[MenuAction] = &[
    MenuAction { label: "Toggle Headers", shortcut: "", action: MenuActionKind::ToggleHeaders },
    MenuAction { label: "Toggle Margins", shortcut: "", action: MenuActionKind::ToggleMargins },
];

pub const SHEET_MENU: &[MenuAction] = &[
    MenuAction { label: "New Sheet",   shortcut: "", action: MenuActionKind::NewSheet },
    MenuAction { label: "Rename Sheet",shortcut: "", action: MenuActionKind::RenameSheet },
    MenuAction { label: "Delete Sheet",shortcut: "", action: MenuActionKind::DeleteSheet },
];

pub const DATA_MENU: &[MenuAction] = &[
    MenuAction { label: "Sort Ascending",  shortcut: "", action: MenuActionKind::SortAsc },
    MenuAction { label: "Sort Descending", shortcut: "", action: MenuActionKind::SortDesc },
    MenuAction { label: "Balance Books",   shortcut: "", action: MenuActionKind::BalanceBooks },
];

pub const HELP_MENU: &[MenuAction] = &[
    MenuAction { label: "Keybindings", shortcut: "F1", action: MenuActionKind::HelpKeybinds },
    MenuAction { label: "About",       shortcut: "",   action: MenuActionKind::About },
];
