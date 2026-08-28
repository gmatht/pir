// Copyright (c) 2026, Corro Project.
// Licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for license information.

use corro::gui::App as GuiApp;
use corro::gui::Backend;
use corro::grid::CellAddr;
use std::fs;

/// Test that when running corro --gui with a non-existent file, text entered into a cell is
/// saved to disk (the file is created by the save, not by load_initial).
#[cfg(feature = "gui")]
#[test]
fn test_gui_creates_file_and_saves_text() {
    // Create a temporary directory for our test
    let temp_dir = tempfile::tempdir().unwrap();
    let test_file = temp_dir.path().join("t.corro");

    // Ensure the file doesn't exist initially
    assert!(!test_file.exists());

    // Create a GUI app with the non-existent file
    let mut app = GuiApp::new_with_paths(vec![test_file.clone()]);
    app.set_backend(Backend::Gui);

    // Simulate the app loading (this builds an in-memory workbook; it does NOT create the file)
    let result = app.load_initial();
    assert!(result.is_ok(), "Failed to load initial state: {:?}", result);
    assert!(!test_file.exists(), "load_initial must not create the file on disk");

    // Enter text into A1 (main row 0, col 0) and save it
    app.set_cell(CellAddr::Main { row: 0, col: 0 }, "hello corrosion".to_string());
    let saved = app.save();
    assert!(saved.is_ok(), "Failed to save: {:?}", saved);

    // At this point, the file should be created and contain the entered text
    assert!(test_file.exists(), "File was not created by save: {:?}", test_file);
    let content = fs::read_to_string(&test_file).unwrap();
    assert!(
        content.contains("hello corrosion"),
        "Saved file should contain entered text:\n{}",
        content
    );

    // Clean up
    temp_dir.close().unwrap();
}
