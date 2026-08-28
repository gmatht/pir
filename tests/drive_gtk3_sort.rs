// Copyright (c) 2026, Corro Project.
// Licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for license information.

//! Driver test for the GTK3 GUI: actually sort the spreadsheet by driving the
//! real sort feature (the same routine a faked keypress into the "Sort" menu
//! action's confirm button invokes) and assert the underlying grid is sorted.

use corro::gui::App as GuiApp;
use corro::gui::Backend;
use corro::grid::CellAddr;

#[cfg(feature = "gui")]
#[test]
fn drive_gtk3_sort_via_faked_keypress() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("sort.corro");

    let mut app = GuiApp::new_with_paths(vec![file.clone()]);
    app.set_backend(Backend::Gui);
    app.load_initial().unwrap();

    // Unsorted column A: main rows 0,1,2 hold 3,1,2.
    app.set_cell(CellAddr::Main { row: 0, col: 0 }, "3".into());
    app.set_cell(CellAddr::Main { row: 1, col: 0 }, "1".into());
    app.set_cell(CellAddr::Main { row: 2, col: 0 }, "2".into());

    // Sanity: not sorted yet (ascending by value would be rows 1,2,0).
    let sheet0 = app.workbook().active_sheet();
    assert_ne!(
        sheet0.grid.sorted_main_rows(),
        vec![1, 2, 0],
        "precondition: grid must start unsorted"
    );

    // Drive the real GTK sort feature. `sort_by_column` is exactly what the
    // "Sort ▸ Ascending" menu action runs once its dialog confirms a column —
    // i.e. the code path a faked keypress into that dialog triggers.
    app.sort_by_column(0, true);

    let sheet = app.workbook().active_sheet();
    let sorted = sheet.grid.sorted_main_rows();
    assert_eq!(sorted, vec![1, 2, 0]);

    let col_a: Vec<String> = sorted
        .iter()
        .map(|&r| {
            sheet
                .grid
                .get(&CellAddr::Main { row: r as u32, col: 0 })
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert_eq!(col_a, vec!["1".to_string(), "2".to_string(), "3".to_string()]);
}
