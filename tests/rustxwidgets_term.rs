//! Phase 6 verification: corro's grid data converges onto a rustxWidgets
//! `SpreadsheetModel` through the same `render::fill_cells` pipeline the
//! pancurses backend uses. Rendered via the headless recorder, the model must
//! contain corro's cell text, sheet tab and border title.
#![cfg(feature = "rustxwidgets-term")]

use corro::grid::{CellAddr, Grid, GridBox};
use corro::rustxwidgets_term::{corro_to_model, from_gridbox, render_headless};

#[test]
fn corro_grid_renders_through_rustxwidgets_model() {
    let mut g = GridBox::new(Grid::new(3, 3));
    g.set(&CellAddr::Main { row: 1, col: 1 }, "Hello".to_string());
    g.set(&CellAddr::Main { row: 2, col: 2 }, "World".to_string());
    g.set(&CellAddr::Main { row: 1, col: 2 }, "=SUM(A1:A3)".to_string());

    // Viewport: 1 header row + 3 main rows, 1 margin col + 3 main cols.
    let hr = 1usize;
    let lm = 1usize;
    let display_rows: Vec<usize> = (0..(hr + 3)).collect();
    let col_ixs: Vec<usize> = (0..(lm + 3)).collect();
    let col_widths: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    let row_agg_func: Vec<Option<corro::ops::AggFunc>> = vec![None; display_rows.len()];

    let model = corro_to_model(
        &g, &display_rows, &col_ixs, &col_widths, hr, 3, 3, lm, 4096,
        hr + 1, lm + 1, &row_agg_func, &["Sheet1".to_string(), "Sheet2".to_string()], 0,
        "corro · rustxWidgets", "ready", "A1", "",
    );

    let dc = render_headless(&model, 80, 24);
    let joined: String = dc.texts().iter().map(|s| s.to_string()).collect();

    assert!(joined.contains("Hello"), "cell text 'Hello' missing: {joined:?}");
    assert!(joined.contains("World"), "cell text 'World' missing: {joined:?}");
    assert!(joined.contains("=SUM(A1:A3)"), "formula cell missing: {joined:?}");
    assert!(joined.contains("Sheet1"), "tab 'Sheet1' missing: {joined:?}");
    assert!(joined.contains("corro"), "border title missing: {joined:?}");
}

#[test]
fn from_gridbox_produces_model_with_content() {
    let mut g = GridBox::new(Grid::new(2, 2));
    g.set(&CellAddr::Main { row: 1, col: 1 }, "X".to_string());
    let model = from_gridbox(&g);
    let dc = render_headless(&model, 60, 20);
    assert!(
        dc.texts().iter().any(|t| t.contains("X")),
        "from_gridbox lost content"
    );
}
