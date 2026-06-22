//! Reproduce: `corro.exe --gui tmp.corro` → enter text → save creates tmp.corro with text.
//! This test uses the same low-level functions that the GUI calls:
//!   - `commit_workbook_op` (called by `commit_edit` in gui_backend.rs)
//!   - `load_workbook_revisions_partial` (called by `load_initial` in gui::App)
//! The file must be created and the text must survive a reload.

use std::path::Path;

fn commit_cell(path: &Path, wb: &mut corro::ops::WorkbookState, row: u32, col: u32, val: &str) {
    let addr = corro::grid::CellAddr::Main { row, col };
    wb.active_sheet_mut().grid.set(&addr, val.to_string());
    let sheet_id = wb.sheet_id(wb.active_sheet);
    let op = corro::ops::Op::SetCell { addr, value: val.to_string() };
    let wbo = corro::ops::WorkbookOp::SheetOp { sheet_id, op };
    let mut active_sheet = sheet_id;
    corro::io::commit_workbook_op(path, &mut 0, wb, &mut active_sheet, &wbo).unwrap();
}

#[test]
fn enter_text_via_commit_op_creates_file_and_survives_reload() {
    let tmp = std::env::temp_dir().join("test_commit_creates.corro");
    let _ = std::fs::remove_file(&tmp);
    assert!(!tmp.exists(), "test requires non-existent file");

    // --- Phase 1: Start with empty workbook (simulating --gui tmp.corro) ---
    let mut wb = corro::ops::WorkbookState::new();

    // --- Phase 2: Commit a cell edit via the exact function the GUI uses ---
    commit_cell(&tmp, &mut wb, 0, 0, "hello");

    // --- Phase 3: File must exist with the text ---
    assert!(tmp.exists(), ".corro file should be created by commit_workbook_op");
    let content = std::fs::read_to_string(&tmp).unwrap();
    assert!(content.contains("hello"), "file content should contain 'hello'");

    // --- Phase 4: Reload via load_workbook_revisions_partial (same as gui::App::load_initial) ---
    let mut loaded = corro::ops::WorkbookState::new();
    let mut active = loaded.sheet_id(loaded.active_sheet);
    let (_offset, _replay) = corro::io::load_workbook_revisions_partial(&tmp, usize::MAX, &mut loaded, &mut active).unwrap();
    let addr = corro::grid::CellAddr::Main { row: 0, col: 0 };
    let val = loaded.active_sheet().grid.get(&addr);
    assert_eq!(val, Some("hello".into()));

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn multiple_edits_are_preserved_after_reload() {
    let tmp = std::env::temp_dir().join("test_commit_multiple.corro");
    let _ = std::fs::remove_file(&tmp);

    let mut wb = corro::ops::WorkbookState::new();
    commit_cell(&tmp, &mut wb, 0, 0, "A1");
    commit_cell(&tmp, &mut wb, 1, 2, "C2");
    commit_cell(&tmp, &mut wb, 5, 3, "D6");

    assert!(tmp.exists());
    let content = std::fs::read_to_string(&tmp).unwrap();
    assert!(content.contains("A1"));
    assert!(content.contains("C2"));

    let mut loaded = corro::ops::WorkbookState::new();
    let mut active = loaded.sheet_id(loaded.active_sheet);
    let _ = corro::io::load_workbook_revisions_partial(&tmp, usize::MAX, &mut loaded, &mut active).unwrap();

    assert_eq!(loaded.active_sheet().grid.get(&corro::grid::CellAddr::Main { row: 0, col: 0 }), Some("A1".into()));
    assert_eq!(loaded.active_sheet().grid.get(&corro::grid::CellAddr::Main { row: 1, col: 2 }), Some("C2".into()));
    assert_eq!(loaded.active_sheet().grid.get(&corro::grid::CellAddr::Main { row: 5, col: 3 }), Some("D6".into()));

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn file_is_not_created_before_first_edit() {
    let tmp = std::env::temp_dir().join("test_no_file_before_edit.corro");
    let _ = std::fs::remove_file(&tmp);
    assert!(!tmp.exists());

    // Just creating a WorkbookState should NOT create the file
    let _wb = corro::ops::WorkbookState::new();
    assert!(!tmp.exists(), "file should not exist until commit");

    let _ = std::fs::remove_file(&tmp);
}
