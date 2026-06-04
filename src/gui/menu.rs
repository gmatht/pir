use rustxwidgets::App;

pub fn build_menu_bar(app: &rustxwidgets::App, window: &rustxwidgets::Window) {
    // Menu system uses GAction-based approach. For now, wire keyboard
    // accelerators and provide menu item definitions consumed by mod.rs.
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
