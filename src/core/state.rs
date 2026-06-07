use crate::grid::{CellAddr, MainRange, SheetCursor, SortSpec};
use crate::io::LogWatcher;
use crate::ops::{Op, SheetState, WorkbookState};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;

pub struct CoreApp {
    pub path: Option<PathBuf>,
    pub import_source: Option<PathBuf>,
    pub source_path: Option<PathBuf>,
    pub revision_limit: Option<usize>,
    pub revision_browse: bool,
    pub revision_browse_limit: usize,
    pub offset: u64,
    pub state: SheetState,
    pub workbook: WorkbookState,
    pub cursor: SheetCursor,
    pub anchor: Option<SheetCursor>,
    pub watcher: Option<LogWatcher>,
    pub status: String,
    pub ops_applied: usize,
    pub op_history: Vec<Op>,
    pub redo_history: Vec<Op>,
    pub view_sheet_id: u32,
    pub persisted_view_sort_cols: HashMap<u32, Vec<SortSpec>>,
    pub linked_source_mtimes: HashMap<PathBuf, SystemTime>,
    pub unsaved_file: Option<PathBuf>,
    pub unsaved_auto_create: bool,
    pub exit_message: Option<String>,
    pub clipboard_snapshot: Option<(MainRange, String)>,
    pub edit_target_addr: Option<CellAddr>,
    pub edit_range_addrs: Option<Vec<CellAddr>>,
    pub pending_lost_edit: Option<(CellAddr, String)>,
    pub pending_fit_to_content_on_commit: bool,
}
