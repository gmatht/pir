// Copyright (c) 2026, Corro Project.
// Licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for license information.
#![cfg(all(target_os = "linux", feature = "gui"))]

use corro::gui::App as GuiApp;
use corro::gui::Backend;
use std::fs;

/// Test that when running corro --gui with a non-existent file,
/// the file is created on first commit and text entered into cells is saved.
#[test]
fn test_gui_committed_edit_creates_file() {
    let temp_dir = tempfile::tempdir().unwrap();
    let test_file = temp_dir.path().join("t.corro");

    assert!(!test_file.exists());

    let mut app = GuiApp::new_with_paths(vec![test_file.clone()]);
    app.set_backend(Backend::Gui);

    // load_initial sets up the in-memory workbook but does NOT create the file.
    // The .corro file is created lazily on the first commit_workbook_op call.
    let result = app.load_initial();
    assert!(result.is_ok(), "Failed to load initial state: {:?}", result);

    // File must NOT exist yet — load_initial only reads, never writes.
    assert!(!test_file.exists(),
        "load_initial should NOT create the file; creation happens on first commit");

    // Simulate what happens when the user edits a cell: the GUI calls
    // commit_workbook_op (via commit_edit in gui_backend.rs).  We exercise
    // the exact same path here through the core workbook API.
    let addr = corro::grid::CellAddr::Main { row: 0, col: 0 };
    app.core.workbook.active_sheet_mut().grid.set(&addr, "hello".into());
    let sheet_id = app.core.workbook.sheet_id(app.core.workbook.active_sheet);
    let op = corro::ops::Op::SetCell { addr, value: "hello".into() };
    let wbo = corro::ops::WorkbookOp::SheetOp { sheet_id, op };
    let mut active_sheet = sheet_id;
    corro::io::commit_workbook_op(
        &test_file,
        &mut app.core.offset,
        &mut app.core.workbook,
        &mut active_sheet,
        &wbo,
    )
    .expect("commit_workbook_op should succeed");

    // Now the file must exist with the committed content
    assert!(test_file.exists(), "File should be created by commit_workbook_op");
    let content = fs::read_to_string(&test_file).unwrap();
    assert!(content.contains("hello"), "file content should contain 'hello'");

    temp_dir.close().unwrap();
}
