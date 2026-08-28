//! rustxWidgets-backed terminal UI for corro.
//!
//! Converges corro's spreadsheet view onto the backend-agnostic
//! `rustxwidgets::SpreadsheetModel`. The exact same `render::fill_cells`
//! pipeline used by the pancurses backend feeds a `SpreadsheetModel`, so the
//! rendered content matches the reference fork in `crate::ui`; only the final
//! `DrawContext` differs (headless recorder / ratatui terminal / pancurses grid).
//!
//! This module coexists with `crate::ui` (the ratatui-direct reference fork),
//! which is intentionally kept for behaviour comparison.
#![cfg(feature = "rustxwidgets-term")]

use crate::grid::{GridBox, HEADER_ROWS, MARGIN_COLS};
use crate::gui::compute::{self, CellDisplayStyle};
use crate::gui::render::{fill_cells, CellSink};
use crate::ops::{AggFunc, WorkbookState};
use rustxwidgets::spreadsheet::{paint, SpreadsheetModel};
use std::collections::HashMap;

/// Number of header rows / margin columns in corro's viewport (the grid's
/// `HEADER_ROWS`/`MARGIN_COLS` constants are sentinels, not counts).
const VIEW_HR: usize = 1;
const VIEW_LM: usize = 1;
/// Cap the rendered viewport so huge sheets don't explode the cell grid.
const VIEW_MAX: usize = 60;

/// A `CellSink` that writes into a rustxWidgets `SpreadsheetModel`.
///
/// `render::fill_cells` already emits 0-based display coordinates where the
/// 0th row/column is the header/margin; the model's `paint` uses the exact
/// same 0-based layout, so we forward the coordinates unchanged.
struct SpreadsheetModelSink<'a> {
    m: &'a mut SpreadsheetModel,
}

impl<'a> CellSink for SpreadsheetModelSink<'a> {
    fn set_cell(&mut self, dr: u32, dc: u32, t: &str) {
        self.m.set_cell(dr, dc, t);
    }
    fn set_cell_style(&mut self, dr: u32, dc: u32, style: CellDisplayStyle) {
        // `SpreadsheetModel::set_cell_style` takes the same u8 constants the
        // pancurses backend uses; `to_pancurses_style` is gated to that
        // feature, so map inline to stay feature-agnostic.
        let s: u8 = match style {
            CellDisplayStyle::Default => 0,
            CellDisplayStyle::Cursor => 1,
            CellDisplayStyle::Aggregate => 2,
            CellDisplayStyle::FooterAggregate => 3,
            CellDisplayStyle::Selected => 4,
            CellDisplayStyle::ActiveHeader => 5,
            CellDisplayStyle::InactiveHeader => 6,
        };
        self.m.set_cell_style(dr, dc, s);
    }
    fn set_raw_cell(&mut self, dr: u32, dc: u32, t: &str) {
        self.m.set_cell(dr, dc, t);
    }
    fn set_cursor(&mut self, dr: u32, dc: u32) {
        self.m.set_cursor(dr, dc);
    }
}

/// Build a `SpreadsheetModel` from a corro `GridBox` viewport using the same
/// generic renderer (`render::fill_cells`) the pancurses backend uses.
#[allow(clippy::too_many_arguments)]
pub fn corro_to_model(
    g: &GridBox,
    display_rows: &[usize],
    col_ixs: &[usize],
    col_widths: &HashMap<usize, usize>,
    hr: usize,
    mr: usize,
    mc: usize,
    lm: usize,
    data_width: usize,
    display_cursor_row: usize,
    display_cursor_col: usize,
    row_agg_func: &[Option<AggFunc>],
    tabs: &[String],
    active_tab: usize,
    title: &str,
    status: &str,
    formula_cell: &str,
    formula_entry: &str,
) -> SpreadsheetModel {
    let rows = display_rows.len() as u32;
    let cols = col_ixs.len() as u32;
    let mut model = SpreadsheetModel::new(rows, cols);
    model.set_grid_config(lm as u32, mc as u32);
    model.set_row_counts(hr as u32, mr as u32);
    model.set_border_title(title);
    model.set_status_text(status);
    model.set_tab_data(tabs, active_tab);
    model.set_formula_bar(formula_cell, formula_entry);
    let mut sink = SpreadsheetModelSink { m: &mut model };
    fill_cells(
        &mut sink, display_rows, col_ixs, col_widths, g, hr, mr, mc, lm, data_width,
        display_cursor_row, display_cursor_col, row_agg_func,
    );
    model
}

/// Convenience: build a model for a whole `GridBox` (no viewport scrolling).
pub fn from_gridbox(g: &GridBox) -> SpreadsheetModel {
    let rows = g.main_rows().min(VIEW_MAX);
    let cols = g.main_cols().min(VIEW_MAX);
    let display_rows: Vec<usize> = (0..(VIEW_HR + rows)).collect();
    let col_ixs: Vec<usize> = (0..(VIEW_LM + cols)).collect();
    let col_widths: HashMap<usize, usize> = HashMap::new();
    let row_agg_func: Vec<Option<AggFunc>> = vec![None; display_rows.len()];
    corro_to_model(
        g, &display_rows, &col_ixs, &col_widths, VIEW_HR, rows, cols, VIEW_LM, 4096,
        VIEW_HR, VIEW_LM, &row_agg_func, &["Sheet1".to_string()], 0,
        "corro · rustxWidgets", "ready", "A1", "",
    )
}

/// Build a model from a corro `WorkbookState` (active sheet).
pub fn from_workbook(wb: &WorkbookState) -> SpreadsheetModel {
    let sheet = &wb.sheets[wb.active_sheet];
    let g = &sheet.state.grid;
    let rows = g.main_rows().min(VIEW_MAX);
    let cols = g.main_cols().min(VIEW_MAX);
    let display_rows: Vec<usize> = (0..(VIEW_HR + rows)).collect();
    let col_ixs: Vec<usize> = (0..(VIEW_LM + cols)).collect();
    let col_widths: HashMap<usize, usize> = HashMap::new();
    let row_agg_func: Vec<Option<AggFunc>> = vec![None; display_rows.len()];
    let names: Vec<String> = wb.sheets.iter().map(|s| s.title.clone()).collect();
    corro_to_model(
        g, &display_rows, &col_ixs, &col_widths, VIEW_HR, rows, cols, VIEW_LM, 4096,
        VIEW_HR, VIEW_LM, &row_agg_func, &names, wb.active_sheet,
        "corro · rustxWidgets", "ready", "A1", "",
    )
}

/// Build a model from a corro GUI `App` (active sheet).
pub fn from_app(app: &crate::gui::App) -> SpreadsheetModel {
    from_workbook(app.workbook())
}

/// Render and run a `SpreadsheetModel` through the rustxWidgets terminal
/// backend (ratatui by default; pancurses when that feature is active).
pub fn run_model(model: SpreadsheetModel) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let rxw = rustxwidgets::backends::ratatui::RatatuiApp::init_with_model(model)?;
    rxw.run()?;
    Ok(())
}

/// Headless helper: paint a model into a recording context (no terminal).
pub fn render_headless(model: &SpreadsheetModel, w: u16, h: u16) -> rustxwidgets::backends::headless::RecordingDrawContext {
    let mut dc = rustxwidgets::backends::headless::RecordingDrawContext::new();
    paint(model, &mut dc, w as i32, h as i32);
    dc
}
