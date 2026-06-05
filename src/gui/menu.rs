#[cfg(feature = "gui")]
use crate::gui::dialogs;
#[cfg(feature = "gui")]
use rustxwidgets::backends_gtk_adapter::{
    self, Application, Menu, MenuBar, Window,
};

/// Build the GTK menu bar, create and register actions, and return the MenuBar widget.
/// The caller should pack the returned MenuBar into the window's layout before other content.
#[cfg(feature = "gui")]
pub fn build_menu_bar(
    app: &Application,
    window: &Window,
) -> Result<MenuBar, Box<dyn std::error::Error>> {
    // ── Build submenu models ────────────────────────────────────────────
    let file_menu = build_submenu(FILE_MENU, "app")?;
    let edit_menu = build_submenu(EDIT_MENU, "app")?;
    let view_menu = build_submenu(VIEW_MENU, "app")?;
    let sheet_menu = build_submenu(SHEET_MENU, "app")?;
    let data_menu = build_submenu(DATA_MENU, "app")?;
    let help_menu = build_submenu(HELP_MENU, "app")?;

    // ── Create and register actions ─────────────────────────────────────
    register_action(app, "open", || {
        if let Some(path) = dialogs::file_open_dialog() {
            eprintln!("Open file: {:?}", path);
        }
    })?;
    register_action(app, "save", || {
        if let Some(path) = dialogs::file_save_dialog() {
            eprintln!("Save file: {:?}", path);
        }
    })?;
    register_action(app, "save_as", || {
        if let Some(path) = dialogs::file_save_dialog() {
            eprintln!("Save file as: {:?}", path);
        }
    })?;
    register_action(app, "quit", || {
        let _ = rustxwidgets::backends_gtk_adapter::quit_main_loop();
    })?;
    register_action(app, "undo", || eprintln!("Undo"))?;
    register_action(app, "redo", || eprintln!("Redo"))?;
    register_action(app, "cut", || eprintln!("Cut"))?;
    register_action(app, "copy", || eprintln!("Copy"))?;
    register_action(app, "paste", || eprintln!("Paste"))?;
    register_action(app, "delete_cell", || eprintln!("Delete cell"))?;
    register_action(app, "select_all", || eprintln!("Select all"))?;
    register_action(app, "find", || {
        dialogs::find_dialog(|result| {
            if let Some(text) = result {
                eprintln!("Find: {}", text);
            }
        });
    })?;
    register_action(app, "replace", || {
        dialogs::replace_dialog(|result| {
            if let Some((find, replace)) = result {
                eprintln!("Replace: {} with {}", find, replace);
            }
        });
    })?;
    register_action(app, "toggle_headers", || eprintln!("Toggle headers"))?;
    register_action(app, "toggle_margins", || eprintln!("Toggle margins"))?;
    register_action(app, "new_sheet", || eprintln!("New sheet"))?;
    register_action(app, "rename_sheet", || {
        dialogs::find_dialog(|result| {
            if let Some(name) = result {
                eprintln!("Rename sheet to: {}", name);
            }
        });
    })?;
    register_action(app, "delete_sheet", || eprintln!("Delete sheet"))?;
    register_action(app, "sort_asc", || {
        let wb = crate::ops::WorkbookState::default();
        dialogs::sort_dialog(&wb, |result| {
            if let Some((col, asc)) = result {
                eprintln!("Sort column {} ascending: {}", col, asc);
            }
        });
    })?;
    register_action(app, "sort_desc", || {
        let wb = crate::ops::WorkbookState::default();
        dialogs::sort_dialog(&wb, |result| {
            if let Some((col, asc)) = result {
                eprintln!("Sort column {} descending: {}", col, !asc);
            }
        });
    })?;
    register_action(app, "balance_books", || {
        dialogs::balance_dialog(|result| {
            if let Some(col) = result {
                eprintln!("Balance column: {}", col);
            }
        });
    })?;
    register_action(app, "export_tsv", || eprintln!("Export TSV"))?;
    register_action(app, "export_csv", || eprintln!("Export CSV"))?;
    register_action(app, "export_ods", || eprintln!("Export ODS"))?;
    register_action(app, "export_ascii", || eprintln!("Export ASCII"))?;
    register_action(app, "about", || {
        dialogs::show_about_dialog();
    })?;
    register_action(app, "help_keybinds", || {
        dialogs::show_keybinds_help();
    })?;

    // ── Assemble menubar model ──────────────────────────────────────────
    let mut menubar_model = backends_gtk_adapter::create_menu()?;
    menubar_model.append_submenu("File", &file_menu);
    menubar_model.append_submenu("Edit", &edit_menu);
    menubar_model.append_submenu("View", &view_menu);
    menubar_model.append_submenu("Sheet", &sheet_menu);
    menubar_model.append_submenu("Data", &data_menu);
    menubar_model.append_submenu("Help", &help_menu);

    // ── Insert action group into window and create MenuBar ──────────────
    unsafe {
        window.insert_action_group("app", app.as_ptr());
        let menubar = backends_gtk_adapter::create_menubar(&menubar_model, app.as_ptr())?;
        Ok(menubar)
    }
}

/// Create a `Menu` from a slice of `MenuAction` items.
#[cfg(feature = "gui")]
fn build_submenu(items: &[MenuAction], prefix: &str) -> Result<Menu, Box<dyn std::error::Error>> {
    let mut menu = backends_gtk_adapter::create_menu()?;
    for item in items {
        let action_name = action_kind_to_name(item.action);
        menu.append(item.label, &format!("{}.{}", prefix, action_name));
    }
    Ok(menu)
}

/// Create a `SimpleAction`, connect its activate closure, and register it with the application.
#[cfg(feature = "gui")]
fn register_action<F: FnMut() + 'static>(
    app: &Application,
    name: &str,
    mut f: F,
) -> Result<(), Box<dyn std::error::Error>> {
    let action = backends_gtk_adapter::create_simple_action(name)?;
    action.connect_activate(move |_param| f())?;
    app.add_action(&action)?;
    Ok(())
}

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

fn action_kind_to_name(kind: MenuActionKind) -> &'static str {
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
