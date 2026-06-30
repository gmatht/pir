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
    // Insert menu
    InsertRows,
    InsertMitosisRow,
    InsertMitosisCol,
    InsertCols,
    InsertSpecialChars,
    InsertDate,
    InsertTime,
    InsertHyperlink,
    // Format menu
    FormatApplyAll,
    FormatApplyFullColumn,
    FormatApplyData,
    FormatApplySpecial,
    FormatApplyCell,
    FormatApplySelection,
    FormatDecimalGeneric,
    FormatCurrency,
    FormatRational,
    FormatFixed0,
    FormatFixed1,
    FormatFixed2,
    FormatFixedCustom,
    FormatAlignLeft,
    FormatAlignCenter,
    FormatAlignRight,
    FormatAlignDefault,
    FormatReset,
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
        MenuActionKind::InsertRows => "insert_rows",
        MenuActionKind::InsertMitosisRow => "insert_mitosis_row",
        MenuActionKind::InsertMitosisCol => "insert_mitosis_col",
        MenuActionKind::InsertCols => "insert_cols",
        MenuActionKind::InsertSpecialChars => "insert_special_chars",
        MenuActionKind::InsertDate => "insert_date",
        MenuActionKind::InsertTime => "insert_time",
        MenuActionKind::InsertHyperlink => "insert_hyperlink",
        MenuActionKind::FormatApplyAll => "format_apply_all",
        MenuActionKind::FormatApplyFullColumn => "format_apply_full_column",
        MenuActionKind::FormatApplyData => "format_apply_data",
        MenuActionKind::FormatApplySpecial => "format_apply_special",
        MenuActionKind::FormatApplyCell => "format_apply_cell",
        MenuActionKind::FormatApplySelection => "format_apply_selection",
        MenuActionKind::FormatDecimalGeneric => "format_decimal_generic",
        MenuActionKind::FormatCurrency => "format_currency",
        MenuActionKind::FormatRational => "format_rational",
        MenuActionKind::FormatFixed0 => "format_fixed_0",
        MenuActionKind::FormatFixed1 => "format_fixed_1",
        MenuActionKind::FormatFixed2 => "format_fixed_2",
        MenuActionKind::FormatFixedCustom => "format_fixed_custom",
        MenuActionKind::FormatAlignLeft => "format_align_left",
        MenuActionKind::FormatAlignCenter => "format_align_center",
        MenuActionKind::FormatAlignRight => "format_align_right",
        MenuActionKind::FormatAlignDefault => "format_align_default",
        MenuActionKind::FormatReset => "format_reset",
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

pub const INSERT_MENU: &[MenuAction] = &[
    MenuAction { label: "Rows",          shortcut: "", action: MenuActionKind::InsertRows },
    MenuAction { label: "Mitosis (Row)", shortcut: "", action: MenuActionKind::InsertMitosisRow },
    MenuAction { label: "Mitosis (Col)", shortcut: "", action: MenuActionKind::InsertMitosisCol },
    MenuAction { label: "Cols",          shortcut: "", action: MenuActionKind::InsertCols },
    MenuAction { label: "Special Char",  shortcut: "", action: MenuActionKind::InsertSpecialChars },
    MenuAction { label: "Date",          shortcut: "", action: MenuActionKind::InsertDate },
    MenuAction { label: "Time",          shortcut: "", action: MenuActionKind::InsertTime },
    MenuAction { label: "Hyperlink",     shortcut: "", action: MenuActionKind::InsertHyperlink },
];

pub const FORMAT_MENU: &[MenuAction] = &[
    MenuAction { label: "Scope: All",        shortcut: "", action: MenuActionKind::FormatApplyAll },
    MenuAction { label: "Scope: Full Col",   shortcut: "", action: MenuActionKind::FormatApplyFullColumn },
    MenuAction { label: "Scope: Data",       shortcut: "", action: MenuActionKind::FormatApplyData },
    MenuAction { label: "Scope: Special",    shortcut: "", action: MenuActionKind::FormatApplySpecial },
    MenuAction { label: "Scope: Cell",       shortcut: "", action: MenuActionKind::FormatApplyCell },
    MenuAction { label: "Scope: Selection",  shortcut: "", action: MenuActionKind::FormatApplySelection },
    MenuAction { label: "Decimal (generic)", shortcut: "", action: MenuActionKind::FormatDecimalGeneric },
    MenuAction { label: "Currency ($)",      shortcut: "", action: MenuActionKind::FormatCurrency },
    MenuAction { label: "Rational",          shortcut: "", action: MenuActionKind::FormatRational },
    MenuAction { label: "Fixed 0",           shortcut: "", action: MenuActionKind::FormatFixed0 },
    MenuAction { label: "Fixed 1",           shortcut: "", action: MenuActionKind::FormatFixed1 },
    MenuAction { label: "Fixed 2",           shortcut: "", action: MenuActionKind::FormatFixed2 },
    MenuAction { label: "Fixed n",           shortcut: "", action: MenuActionKind::FormatFixedCustom },
    MenuAction { label: "Align Left",        shortcut: "", action: MenuActionKind::FormatAlignLeft },
    MenuAction { label: "Align Center",      shortcut: "", action: MenuActionKind::FormatAlignCenter },
    MenuAction { label: "Align Right",       shortcut: "", action: MenuActionKind::FormatAlignRight },
    MenuAction { label: "Align Default",     shortcut: "", action: MenuActionKind::FormatAlignDefault },
    MenuAction { label: "Reset",             shortcut: "", action: MenuActionKind::FormatReset },
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
