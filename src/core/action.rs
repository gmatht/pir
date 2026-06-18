//! Actions represent user-intent operations decoupled from any specific
//! input mechanism (keyboard, mouse, script). The UI maps input events to
//! Actions, and the core dispatches them.

use crate::grid::{CellAddr, CellFormat, FormatScope, MainRange};
use std::path::PathBuf;

/// Every user-triggerable operation in the application.
#[derive(Clone, Debug)]
pub enum Action {
    // ── Navigation ──────────────────────────────────────────────────
    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,
    MoveHome,
    MoveEnd,
    MovePageUp,
    MovePageDown,
    MoveToCell(CellAddr),

    // ── Selection ───────────────────────────────────────────────────
    SelectUp,
    SelectDown,
    SelectLeft,
    SelectRight,
    SelectAll,
    CancelSelection,

    // ── Edit ────────────────────────────────────────────────────────
    StartEdit,
    StartEditWith(String),
    CommitEdit(String),
    CancelEdit,
    InsertChar(char),
    DeleteChar,
    DeleteBackChar,
    DeleteSelection,
    ClearSelection,

    // ── Clipboard ───────────────────────────────────────────────────
    Copy,
    Cut,
    Paste,
    CopySelectionTsv,
    CopySelectionCsv,

    // ── Undo / Redo ─────────────────────────────────────────────────
    Undo,
    Redo,

    // ── File ────────────────────────────────────────────────────────
    Save,
    SaveAs,
    Open(PathBuf),
    Quit,

    // ── Format ──────────────────────────────────────────────────────
    FormatCell(CellFormat),
    FormatScope(FormatScope),
    ClearFormatting,

    // ── Sheet ────────────────────────────────────────────────────────
    NextSheet,
    PrevSheet,
    RenameSheet(String),
    DuplicateSheet,
    DeleteSheet,

    // ── Sort / Filter ────────────────────────────────────────────────
    SortByColumn((usize, bool)),

    // ── Fill / Extrapolate ──────────────────────────────────────────
    FillRange(MainRange),
    ExtrapolateRange(MainRange),

    // ── Menu ────────────────────────────────────────────────────────
    OpenMenu,
    MenuSelect(usize),
    MenuClose,

    // ── Export ──────────────────────────────────────────────────────
    ExportDelimited(ExportFormat),
    ExportOds,
    ExportAscii,

    // ── Misc ────────────────────────────────────────────────────────
    ToggleHelp,
    ToggleAbout,
    Refresh,
    NoOp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    Tsv,
    Csv,
    GenericTsv,
    GenericCsv,
    SelectionTsv,
    SelectionCsv,
}
