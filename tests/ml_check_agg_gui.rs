#![cfg(any(feature = "gui", feature = "pancurses", target_arch = "wasm32"))]

use corro::grid::{
    CellAddr, ColumnAddr, Grid, GridBox, SheetCursor, HEADER_ROWS, MARGIN_COLS,
};
use corro::gui::compute::{self, CellDisplayStyle, right_col_agg};
use corro::gui::render::CellSink;
use corro::ops::SheetState;
use corro::ui_core;
use std::collections::HashMap;

/// Check that right_col_agg works correctly
#[test]
fn test_right_col_agg() {
    let mut raw = Grid::new(2, 3);
    raw.set(&CellAddr::Main { row: 0, col: 0 }, "10".into());
    raw.set(&CellAddr::Main { row: 0, col: 1 }, "20".into());
    raw.set(&CellAddr::Main { row: 1, col: 0 }, "30".into());
    raw.set(&CellAddr::Main { row: 1, col: 1 }, "40".into());
    raw.set(
        &CellAddr::Header {
            row: 0,
            col: ColumnAddr::Right(0),
        },
        "=TOTAL".into(),
    );

    let state = SheetState::from_grid(raw);
    let gb: &GridBox = &state.grid;
    let mc = gb.main_cols();
    let right_start = MARGIN_COLS + mc;

    let agg = right_col_agg(gb, right_start);
    assert!(agg.is_some());
    assert_eq!(agg.unwrap(), corro::ops::AggFunc::Sum);
}

/// Check that compute_cell_info produces a non-empty aggregate value
#[test]
fn test_compute_cell_info_agg() {
    let mut raw = Grid::new(2, 3);
    raw.set(&CellAddr::Main { row: 0, col: 0 }, "10".into());
    raw.set(&CellAddr::Main { row: 0, col: 1 }, "20".into());
    raw.set(&CellAddr::Main { row: 1, col: 0 }, "30".into());
    raw.set(&CellAddr::Main { row: 1, col: 1 }, "40".into());
    raw.set(
        &CellAddr::Header {
            row: 0,
            col: ColumnAddr::Right(0),
        },
        "=TOTAL".into(),
    );
    let gb = GridBox::from(raw);
    let lm = MARGIN_COLS;
    let mc = gb.main_cols();
    let mr = gb.main_rows();

    let rca = right_col_agg(&gb, lm + mc);

    let info = compute::compute_cell_info(
        &gb, &CellAddr::Right { col: 0, row: 0 },
        false, None, Some(0), None, rca,
        lm + mc, lm, mc, mr,
    );
    eprintln!("formatted='{}' style={:?} is_agg={}", info.formatted, info.style, info.is_agg_cell);
    assert!(!info.formatted.trim().is_empty());
    assert!(info.formatted.contains("30"));
    assert_eq!(info.style, CellDisplayStyle::Aggregate);

    let info2 = compute::compute_cell_info(
        &gb, &CellAddr::Right { col: 0, row: 1 },
        false, None, Some(1), None, rca,
        lm + mc, lm, mc, mr,
    );
    eprintln!("formatted='{}' style={:?}", info2.formatted, info2.style);
    assert!(!info2.formatted.trim().is_empty());
    assert!(info2.formatted.contains("70"));
}

/// Regression test: verify that setting `==TOTAL` at CellAddr::Left (correct)
/// triggers aggregation, while CellAddr::Main (old buggy behavior) does not.
/// This tests the exact bug pattern where GUI editing functions used to
/// construct CellAddr::Main for left-margin edits, making aggregates invisible.
#[test]
fn test_left_margin_addr_must_be_left_not_main() {
    let mut raw = Grid::new(3, 2);
    raw.set(&CellAddr::Main { row: 0, col: 0 }, "10".into());
    raw.set(&CellAddr::Main { row: 0, col: 1 }, "20".into());
    raw.set(&CellAddr::Main { row: 1, col: 0 }, "30".into());
    raw.set(&CellAddr::Main { row: 1, col: 1 }, "40".into());
    raw.set(&CellAddr::Main { row: 2, col: 0 }, "5".into());
    raw.set(&CellAddr::Main { row: 2, col: 1 }, "6".into());

    let gb = GridBox::from(raw);
    let hr = HEADER_ROWS;
    let mr = gb.main_rows();
    let lm = MARGIN_COLS;

    let display_rows: Vec<usize> = (hr..hr + mr).collect();

    // Step 1: set ==TOTAL at the WRONG address (CellAddr::Main) — old bug
    // The GUI backends used to construct CellAddr::Main for left-margin edits
    let mut grid_wrong = gb.clone();
    grid_wrong.set(&CellAddr::Main { row: 0, col: 0 }, "==TOTAL".into());
    let wrong_funcs = compute::compute_row_agg_func(&grid_wrong, &display_rows, hr, mr);
    // left_margin_agg looks at CellAddr::Left { col: MARGIN_COLS-1, row: 0 }
    // so a value at CellAddr::Main is invisible
    assert!(
        wrong_funcs[0].is_none(),
        "Bug: aggregate detected even though value is at CellAddr::Main (wrong address)!"
    );

    // Step 2: set ==TOTAL at the CORRECT address
    let mut grid_correct = grid_wrong;
    grid_correct.set(
        &CellAddr::Left { col: lm - 1, row: 0 },
        "==TOTAL".into(),
    );
    let correct_funcs = compute::compute_row_agg_func(&grid_correct, &display_rows, hr, mr);

    assert!(
        correct_funcs[0].is_some(),
        "Regression: aggregate NOT detected even at correct CellAddr::Left address"
    );
    assert_eq!(
        correct_funcs[0],
        Some(corro::ops::AggFunc::Sum),
        "Aggregate should be Sum for ==TOTAL"
    );

    // Step 3: remaining rows have no aggregate
    for i in 1..mr {
        assert!(
            correct_funcs[i].is_none(),
            "Row {} should have no aggregate", i
        );
    }
}

/// Full pipeline test: load grid data, compute viewport, fill cells,
/// and check aggregate column values
#[test]
fn test_full_agg_pipeline() {
    let mut raw = Grid::new(7, 3);

    // Headers
    raw.set(&CellAddr::Header { row: 0, col: ColumnAddr::Main(0) }, "A".into());
    raw.set(&CellAddr::Header { row: 0, col: ColumnAddr::Main(1) }, "B".into());
    raw.set(&CellAddr::Header { row: 0, col: ColumnAddr::Main(2) }, "=TOTAL".into());

    // Data rows (matching subtotal-tiny)
    raw.set(&CellAddr::Main { row: 0, col: 0 }, "1".into());
    raw.set(&CellAddr::Main { row: 0, col: 1 }, "4".into());
    raw.set(&CellAddr::Main { row: 1, col: 0 }, "2".into());
    raw.set(&CellAddr::Main { row: 1, col: 1 }, "105".into());
    raw.set(&CellAddr::Main { row: 2, col: 0 }, "3".into());
    raw.set(&CellAddr::Main { row: 2, col: 1 }, "109".into());
    raw.set(&CellAddr::Main { row: 3, col: 0 }, "1.5".into());
    raw.set(&CellAddr::Main { row: 3, col: 1 }, "54.5".into());
    raw.set(&CellAddr::Main { row: 4, col: 0 }, "3".into());
    raw.set(&CellAddr::Main { row: 4, col: 1 }, "109".into());
    raw.set(&CellAddr::Main { row: 5, col: 0 }, "7".into());
    raw.set(&CellAddr::Main { row: 5, col: 1 }, "8".into());
    raw.set(&CellAddr::Main { row: 6, col: 0 }, "1".into());
    raw.set(&CellAddr::Main { row: 6, col: 1 }, "2".into());

    // Left-margin row aggregates
    raw.set(&CellAddr::Left { col: MARGIN_COLS - 1, row: 2 }, "=TOTAL".into());   // row 3
    raw.set(&CellAddr::Left { col: MARGIN_COLS - 1, row: 4 }, "=TOTAL".into());   // row 5
    raw.set(&CellAddr::Left { col: MARGIN_COLS - 1, row: 6 }, "=TOTAL".into());   // footer

    // Footer cell to trigger footer row visibility
    raw.set(&CellAddr::Footer { row: 0, col: ColumnAddr::Main(0) }, "footer".into());

    let state = SheetState::from_grid(raw);
    let gb: &GridBox = &state.grid;
    let hr = HEADER_ROWS;
    let mr = gb.main_rows();
    let mc = gb.main_cols();
    let lm = MARGIN_COLS;

    let cursor = SheetCursor { row: hr, col: lm };
    let data_cols = 30usize;
    let data_rows = 30usize;

    let (display_rows, _) = ui_core::visible_row_indices(&state, cursor, data_rows, 0);
    let (col_ixs, _) = ui_core::visible_col_indices(&state, cursor, data_cols, 0);

    eprintln!("display_rows (first 15): {:?}", &display_rows[..display_rows.len().min(15)]);
    eprintln!("col_ixs: {:?}", col_ixs);

    // Check aggregate column C (global = lm + 2) is in viewport
    assert!(
        col_ixs.contains(&(lm + 2)),
        "Col C (global {}) must be in col_ixs: {:?}", lm + 2, col_ixs
    );

    // Setup for fill_cells
    let col_widths: HashMap<usize, usize> = col_ixs.iter()
        .map(|&c| (c, gb.col_width(c).max(1)))
        .collect();
    let row_agg_func = compute::compute_row_agg_func(gb, &display_rows, hr, mr);

    eprintln!("row_agg_func count: {}", row_agg_func.len());

    // Test sink
    struct TestSink {
        cells: HashMap<(u32, u32), String>,
        styles: HashMap<(u32, u32), CellDisplayStyle>,
    }
    impl CellSink for TestSink {
        fn set_cell(&mut self, row: u32, col: u32, text: &str) {
            self.cells.insert((row, col), text.to_string());
        }
        fn set_cell_style(&mut self, row: u32, col: u32, style: CellDisplayStyle) {
            self.styles.insert((row, col), style);
        }
        fn set_raw_cell(&mut self, _r: u32, _c: u32, _t: &str) {}
        fn set_cursor(&mut self, _r: u32, _c: u32) {}
    }
    let mut sink = TestSink { cells: HashMap::new(), styles: HashMap::new() };

    corro::gui::render::fill_cells(
        &mut sink, &display_rows, &col_ixs, &col_widths, gb,
        hr, mr, mc, lm, 999, hr, lm, &row_agg_func,
    );

    // Check aggregate column C values for each display row
    let c_global = lm + 2;
    for (dri, &logical_row) in display_rows.iter().enumerate() {
        // Only check main rows (where aggregates should appear)
        if logical_row >= hr && logical_row < hr + mr {
            let main_row = logical_row - hr;
            let key = (dri as u32, c_global as u32);
            let val = sink.cells.get(&key);
            let style = sink.styles.get(&key);
            eprintln!(
                "display_row={} logical_row={} main_row={} col=C({}) cell={:?} style={:?}",
                dri, logical_row, main_row, c_global, val, style
            );
            // Each main row should have a cell value
            assert!(
                val.is_some(),
                "Aggregate cell missing at display_row={} logical_row={} main_row={}",
                dri, logical_row, main_row
            );
            let v = val.unwrap();
            assert!(
                !v.trim().is_empty(),
                "Aggregate cell empty at display_row={} logical_row={} main_row={}",
                dri, logical_row, main_row
            );
        }
    }
}
