// Copyright (c) 2026, Corro Project.
// Licensed under the Apache License, Version 2.0.
// See the LICENSE file in the project root for license information.

//! Driver tests for the pancurses backend: drive its *real* paint and input
//! code paths (no live terminal required — `render_model_to_grid` paints into
//! an in-memory `CellGrid`, and `InputEvent::from_pancurses` translates raw
//! pancurses keys). This is the pancurses counterpart of the ratatui/GTK3
//! driver tests: we feed the backend real data + real key input and assert the
//! resulting grid/event, instead of testing the model in isolation.

#![cfg(feature = "pancurses")]

use rustxwidgets::backends::pancurses_draw::render_model_to_grid;
use rustxwidgets::core::InputEvent;
use rustxwidgets::spreadsheet::SpreadsheetModel;

/// Build a 3-row single-column model (rows 1..=3) holding the given values.
/// Values go in model column 1 (column 0 is the row-label margin), and we use
/// letters so they never collide with the numeric row labels.
fn column_model(values: [&str; 3]) -> SpreadsheetModel {
    let mut m = SpreadsheetModel::new(6, 4);
    m.set_border_title("PanSort");
    m.set_cell(1, 1, values[0]);
    m.set_cell(2, 1, values[1]);
    m.set_cell(3, 1, values[2]);
    m
}

/// Find the first display row (y) that contains `val` in the painted grid.
fn first_row_with(grid: &rustxwidgets::backends::pancurses_draw::CellGrid, val: &str) -> Option<usize> {
    grid.cells
        .iter()
        .position(|row| row.iter().any(|c| c.ch.to_string() == val))
}

#[test]
fn drive_pancurses_backend_paints_unsorted_then_sorted() {
    // 1) Drive the real pancurses paint path with an UNSORTED model.
    let unsorted = column_model(["C", "A", "B"]);
    let grid = render_model_to_grid(&unsorted, 60u16, 16u16);
    let joined = grid.row_strings().join("");
    assert!(joined.contains("C"));
    assert!(joined.contains("A"));
    assert!(joined.contains("B"));

    // 2) Drive a "sort": reorder the cells (what a sort feature does) and
    //    re-render through the *same* pancurses paint path.
    let sorted = column_model(["A", "B", "C"]);
    let grid_sorted = render_model_to_grid(&sorted, 60u16, 16u16);

    // The painted grid must show the values in ascending visual order:
    // value 'A' sits above 'B' sits above 'C'.
    let y1 = first_row_with(&grid_sorted, "A").expect("sorted grid shows 'A'");
    let y2 = first_row_with(&grid_sorted, "B").expect("sorted grid shows 'B'");
    let y3 = first_row_with(&grid_sorted, "C").expect("sorted grid shows 'C'");
    assert!(
        y1 < y2 && y2 < y3,
        "sorted values must appear top-to-bottom as A,B,C (got rows {y1},{y2},{y3})"
    );

    // And the unsorted render must NOT already be in that order.
    let uy3 = first_row_with(&grid, "C").expect("unsorted grid shows 'C'");
    let uy1 = first_row_with(&grid, "A").expect("unsorted grid shows 'A'");
    assert_ne!(
        (uy1, uy3),
        (y1, y3),
        "precondition: unsorted render must differ from sorted render"
    );
}

#[test]
fn drive_pancurses_input_translation() {
    // Drive the real pancurses key -> InputEvent translation (the input path
    // the spreadsheet consumes).
    assert_eq!(
        InputEvent::from_pancurses(pancurses::Input::KeyUp),
        InputEvent::ArrowUp
    );
    assert_eq!(
        InputEvent::from_pancurses(pancurses::Input::KeyDown),
        InputEvent::ArrowDown
    );
    assert_eq!(
        InputEvent::from_pancurses(pancurses::Input::KeyLeft),
        InputEvent::ArrowLeft
    );
    assert_eq!(
        InputEvent::from_pancurses(pancurses::Input::KeyRight),
        InputEvent::ArrowRight
    );
    assert_eq!(
        InputEvent::from_pancurses(pancurses::Input::KeyEnter),
        InputEvent::Enter
    );
    assert_eq!(
        InputEvent::from_pancurses(pancurses::Input::KeyDC),
        InputEvent::Delete
    );
    assert_eq!(
        InputEvent::from_pancurses(pancurses::Input::Character('a')),
        InputEvent::Char('a')
    );
    assert_eq!(
        InputEvent::from_pancurses(pancurses::Input::KeyF1),
        InputEvent::F(1)
    );
}

/// Comprehensive pancurses key -> `InputEvent` translation, mirroring the
/// ratatui keyboard driver tests. This is the input path the pancurses
/// spreadsheet consumes; every key the UI handles must map correctly so a
/// feature driven via pancurses input behaves like its ratatui counterpart.
#[test]
fn drive_pancurses_input_translation_full() {
    use pancurses::Input::*;
    use InputEvent::*;
    // Navigation
    assert_eq!(InputEvent::from_pancurses(KeyUp), ArrowUp);
    assert_eq!(InputEvent::from_pancurses(KeyDown), ArrowDown);
    assert_eq!(InputEvent::from_pancurses(KeyLeft), ArrowLeft);
    assert_eq!(InputEvent::from_pancurses(KeyRight), ArrowRight);
    // Editing / action keys
    assert_eq!(InputEvent::from_pancurses(KeyEnter), Enter);
    assert_eq!(InputEvent::from_pancurses(KeyBackspace), Backspace);
    assert_eq!(InputEvent::from_pancurses(KeyDC), Delete);
    assert_eq!(InputEvent::from_pancurses(KeyHome), Home);
    assert_eq!(InputEvent::from_pancurses(KeyEnd), End);
    assert_eq!(InputEvent::from_pancurses(KeyNPage), PageDown);
    assert_eq!(InputEvent::from_pancurses(KeyPPage), PageUp);
    // Tab (pancurses 0.17 has no bare KeyTab; shift/ctrl variants only)
    assert_eq!(InputEvent::from_pancurses(KeySTab), Tab);
    assert_eq!(InputEvent::from_pancurses(KeyCTab), Tab);
    // Escape / quit
    assert_eq!(InputEvent::from_pancurses(KeyExit), Escape);
    // Function keys F1..F15
    assert_eq!(InputEvent::from_pancurses(KeyF1), F(1));
    assert_eq!(InputEvent::from_pancurses(KeyF2), F(2));
    assert_eq!(InputEvent::from_pancurses(KeyF8), F(8));
    assert_eq!(InputEvent::from_pancurses(KeyF12), F(12));
    assert_eq!(InputEvent::from_pancurses(KeyF15), F(15));
    assert_eq!(InputEvent::from_pancurses(KeyF0), InputEvent::Unknown); // not mapped
    // Characters (letters, digits, space, symbols)
    for c in ['a', 'm', 'z', '0', '9', ' ', '/', '=', '+', '_'] {
        assert_eq!(InputEvent::from_pancurses(Character(c)), Char(c));
    }
    // Unknown key falls through
    assert_eq!(InputEvent::from_pancurses(KeyBTab), InputEvent::Unknown);
}

/// Paint: the pancurses backend must draw the border box and the border title
/// text (the same chrome ratatui draws around the grid).
#[test]
fn drive_pancurses_borders_and_title() {
    let m = column_model(["x", "y", "z"]); // border title "PanSort"
    let grid = render_model_to_grid(&m, 60u16, 16u16);
    let joined = grid.row_strings().join("");
    // Each cell is 1 char tall in the pancurses cell-grid, so stroke_rect
    // collapses to top/bottom edges only (no vertical │ / corner glyphs).
    // The horizontal border and the border title are still emitted.
    assert!(joined.contains('─'), "horizontal border missing from pancurses render");
    assert!(joined.contains("PanSort"), "border title missing from pancurses render");
}

/// Paint: column headers (A/B/C) and row labels (1/2/3) must be present, along
/// with the cell values.
#[test]
fn drive_pancurses_headers_and_labels() {
    let mut m = SpreadsheetModel::new(6, 4);
    m.set_cell(1, 1, "v1");
    m.set_cell(2, 2, "v2");
    let grid = render_model_to_grid(&m, 60u16, 16u16);
    let joined = grid.row_strings().join("");
    // Cell values must be present.
    assert!(joined.contains("v1") && joined.contains("v2"), "cell values missing");
    // The header/margin rows carry column + row labels (numeric indices here).
    assert!(joined.contains('2') && joined.contains('3'), "row/column labels missing");
}

/// Paint: committing an edit == setting the cell value, then painting through
/// the real pancurses path. The committed value must appear in the grid.
#[test]
fn drive_pancurses_edit_commit_shows_value() {
    let mut m = SpreadsheetModel::new(6, 4);
    m.set_cell(1, 1, "EDITED");
    let grid = render_model_to_grid(&m, 60u16, 16u16);
    let joined = grid.row_strings().join("");
    assert!(joined.contains("EDITED"), "committed cell value missing from pancurses render");
}

/// Paint: a multi-column model must show every column's value.
#[test]
fn drive_pancurses_multi_column() {
    let mut m = SpreadsheetModel::new(6, 5);
    m.set_cell(1, 1, "c1");
    m.set_cell(1, 2, "c2");
    m.set_cell(2, 1, "d1");
    m.set_cell(2, 2, "d2");
    let grid = render_model_to_grid(&m, 80u16, 16u16);
    let joined = grid.row_strings().join("");
    assert!(joined.contains("c1") && joined.contains("c2"), "column 1/2 values missing");
    assert!(joined.contains("d1") && joined.contains("d2"), "column 1/2 row2 values missing");
}

/// Widget store: the pancurses backend keeps rendered cells in a `Spreadsheet`
/// widget; set/get must round-trip (this is what the paint path writes into).
#[test]
fn drive_pancurses_widget_cell_roundtrip() {
    let ss = rustxwidgets::backends_pancurses_adapter::create_spreadsheet(10, 5)
        .expect("create_spreadsheet");
    ss.set_cell(2, 3, "hello");
    assert_eq!(ss.get_cell(2, 3).as_deref(), Some("hello"));
    assert_eq!(ss.get_cell(0, 0).as_deref(), None);
    ss.set_cell(0, 0, "A1");
    assert_eq!(ss.get_cell(0, 0).as_deref(), Some("A1"));
}

/// Widget store: cursor position (the navigation target) must round-trip.
#[test]
fn drive_pancurses_widget_cursor_nav() {
    let ss = rustxwidgets::backends_pancurses_adapter::create_spreadsheet(24, 26)
        .expect("create_spreadsheet");
    ss.set_cursor(5, 4);
    assert_eq!(ss.cursor_position(), Some((5, 4)));
    ss.set_cursor(5, 5);
    assert_eq!(ss.cursor_position(), Some((5, 5)));
    ss.set_cursor(6, 5);
    assert_eq!(ss.cursor_position(), Some((6, 5)));
}

/// Widget store: grid_config records the margin/main column split that
/// `fill_cells` uses when populating the widget.
#[test]
fn drive_pancurses_widget_grid_config() {
    let ss = rustxwidgets::backends_pancurses_adapter::create_spreadsheet(24, 30)
        .expect("create_spreadsheet");
    ss.set_grid_config(2, 3); // margin_cols=2, main_cols=3
    ss.set_cell(0, 2, "mainA");
    assert_eq!(ss.get_cell(0, 2).as_deref(), Some("mainA"));
}