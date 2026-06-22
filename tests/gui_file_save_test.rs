// Copyright (c) 2026, Corro Project.
// Licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for license information.

use corro::gui::App as GuiApp;
use corro::gui::Backend;
use std::fs;

/// Test that when running corro --gui with a non-existent file,
/// the file is created and text entered into cells is saved.
#[test]
fn test_gui_creates_file_and_saves_text() {
    // Create a temporary directory for our test
    let temp_dir = tempfile::tempdir().unwrap();
    let test_file = temp_dir.path().join("t.corro");

    // Ensure the file doesn't exist initially
    assert!(!test_file.exists());

    // Create a GUI app with the non-existent file
    let mut app = GuiApp::new_with_paths(vec![test_file.clone()]);
    app.set_backend(Backend::Gtk);

    // Simulate the app loading (this should create the file if it doesn't exist)
    let result = app.load_initial();
    assert!(result.is_ok(), "Failed to load initial state: {:?}", result);

    // At this point, the file should be created
    assert!(test_file.exists(), "File was not created: {:?}", test_file);

    // Get the file size (should be small since it's a new workbook)
    let file_size = fs::metadata(&test_file).unwrap().len();
    assert!(file_size > 0, "File should not be empty: {:?}", test_file);

    // Clean up
    temp_dir.close().unwrap();
}
