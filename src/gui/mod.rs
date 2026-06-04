use crate::core::state::CoreApp;
use crate::grid::{CellAddr, SheetCursor, HEADER_ROWS, MARGIN_COLS};
use crate::io::load_workbook_revisions_partial;
use crate::ops::WorkbookState;
use std::path::PathBuf;

pub mod clipboard;
pub mod dialogs;
pub mod edit;
pub mod keymap;
pub mod menu;
pub mod sheet;

#[cfg(feature = "gui")]
mod gtk_backend;
#[cfg(feature = "pancurses")]
mod pnc_backend;

pub enum Backend {
    Gtk,
    Pancurses,
}

pub struct App {
    core: CoreApp,
    rev_limit: Option<usize>,
    rev_browse: bool,
    backend: Option<Backend>,
}

impl App {
    pub fn new_with_paths(paths: Vec<PathBuf>) -> Self {
        App {
            core: CoreApp {
                path: paths.first().cloned(),
                import_source: None,
                source_path: None,
                revision_limit: None,
                revision_browse: false,
                revision_browse_limit: 0,
                offset: 0,
                state: Default::default(),
                workbook: WorkbookState::new(),
                cursor: SheetCursor { row: HEADER_ROWS, col: MARGIN_COLS },
                anchor: None,
                watcher: None,
                status: String::new(),
                ops_applied: 0,
                op_history: Vec::new(),
                redo_history: Vec::new(),
                view_sheet_id: 0,
                persisted_view_sort_cols: Default::default(),
                linked_source_mtimes: Default::default(),
                unsaved_file: None,
                unsaved_auto_create: false,
                exit_message: None,
                clipboard_snapshot: None,
                edit_target_addr: None,
                edit_range_addrs: None,
                pending_lost_edit: None,
                pending_fit_to_content_on_commit: false,
            },
            rev_limit: None,
            rev_browse: false,
            backend: None,
        }
    }

    pub fn new_with_revision_browser(path: Option<PathBuf>) -> Self {
        let mut app = Self::new_with_paths(path.into_iter().collect());
        app.rev_browse = true;
        app.core.revision_browse = true;
        app
    }

    pub fn new_with_revision_limit(path: Option<PathBuf>, limit: Option<usize>) -> Self {
        let mut app = Self::new_with_paths(path.into_iter().collect());
        app.rev_limit = limit;
        app.core.revision_limit = limit;
        app
    }

    pub fn load_initial(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.core.path.clone();
        if let Some(p) = &path {
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
            match ext.as_str() {
                "corro" => {
                    let mut active_sheet = self.core.workbook.sheet_id(self.core.workbook.active_sheet);
                    load_workbook_revisions_partial(
                        p, self.rev_limit.unwrap_or(usize::MAX),
                        &mut self.core.workbook, &mut active_sheet,
                    ).map_err(|e| format!("failed to load: {e}"))?;
                    if let Some(i) = self.core.workbook.sheets.iter().position(|s| s.id == active_sheet) {
                        self.core.workbook.active_sheet = i;
                    }
                }
                "ods" => {
                    let wb = crate::ods::import_ods_workbook(p).map_err(|e| format!("failed to import ODS: {e}"))?;
                    self.core.workbook = wb;
                }
                "tsv" => {
                    let data = std::fs::read_to_string(p).map_err(|e| format!("failed to read TSV: {e}"))?;
                    crate::io::import_tsv(&data, self.core.workbook.active_sheet_mut());
                }
                "csv" => {
                    let data = std::fs::read_to_string(p).map_err(|e| format!("failed to read CSV: {e}"))?;
                    crate::io::import_csv(&data, self.core.workbook.active_sheet_mut());
                }
                _ => return Err(format!("unsupported file type: {ext}").into()),
            }
        }
        Ok(())
    }

    pub fn set_backend(&mut self, backend: Backend) {
        self.backend = Some(backend);
    }

    pub fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        match self.backend.as_ref().unwrap_or(&Backend::Gtk) {
            Backend::Gtk => self.run_gtk_inner(),
            Backend::Pancurses => self.run_pnc_inner(),
        }
    }

    #[cfg(feature = "gui")]
    fn run_gtk_inner(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        gtk_backend::run_gtk(self)
    }

    #[cfg(not(feature = "gui"))]
    fn run_gtk_inner(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        Err("GTK backend not compiled in (pancurses active); rebuild with --features gui".into())
    }

    #[cfg(feature = "pancurses")]
    fn run_pnc_inner(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        pnc_backend::run_pancurses(self)
    }

    #[cfg(not(feature = "pancurses"))]
    fn run_pnc_inner(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        Err("pancurses backend not compiled in; rebuild with --features pancurses".into())
    }

    pub fn take_final_exit_hint(&mut self) -> Option<String> {
        self.core.exit_message.take()
    }
}
