use crate::grid::{CellAddr, ColumnAddr, GridBox, SheetCursor, HEADER_ROWS, MARGIN_COLS};
use crate::ops::{Op, WorkbookOp};
use crate::ui_core;
use std::collections::HashMap;
use rustxwidgets::backends_pancurses_adapter::*;

use unicode_width::UnicodeWidthStr;

use super::compute;
use super::render::{self, CellSink};

/// Pancurses adapter: wraps a Spreadsheet ref as a CellSink for the generic fill_cells.
struct SpreadsheetSink<'a> {
    ss: &'a Spreadsheet,
}

impl<'a> SpreadsheetSink<'a> {
    fn new(ss: &'a Spreadsheet) -> Self {
        SpreadsheetSink { ss }
    }
}

impl CellSink for SpreadsheetSink<'_> {
    fn set_cell(&mut self, row: u32, col: u32, text: &str) {
        self.ss.set_cell(row, col, text);
    }
    fn set_cell_style(&mut self, row: u32, col: u32, style: compute::CellDisplayStyle) {
        self.ss.set_cell_style(row, col, style.to_pancurses_style());
    }
    fn set_raw_cell(&mut self, row: u32, col: u32, text: &str) {
        self.ss.set_raw_cell(row, col, text);
    }
    fn set_cursor(&mut self, row: u32, col: u32) {
        self.ss.set_cursor(row, col);
    }
}

/// Populate the spreadsheet widget with cell data for the given viewport.
#[allow(clippy::too_many_arguments)]
fn fill_cells(
    spreadsheet: &Spreadsheet,
    display_rows: &[usize],
    col_ixs: &[usize],
    col_widths: &HashMap<usize, usize>,
    g: &GridBox,
    hr: usize, mr: usize, mc: usize, lm: usize,
    data_width: usize,
    display_cursor_row: usize, display_cursor_col: usize,
    row_agg_func: &[Option<crate::ops::AggFunc>],
) {
    let mut sink = SpreadsheetSink::new(spreadsheet);
    render::fill_cells(
        &mut sink, display_rows, col_ixs, col_widths, g,
        hr, mr, mc, lm, data_width,
        display_cursor_row, display_cursor_col, row_agg_func,
    );
}

pub fn run_pancurses(app: &mut super::App) -> Result<(), Box<dyn std::error::Error>> {
    let _backend = rustxwidgets::backends::pancurses::init()
        .map_err(|e| format!("pancurses init failed: {e}"))?;

    let win = create_window()?;
    win.set_title("corro");

    // Tighten main columns to match ratatui's fit_column_to_rendered_content
    // (called during ui::App::load_initial). Without this, columns set wider
    // than max_col_width by auto_fit_column would remain uncapped.
    app.fit_main_columns_to_max_width();

    // ── Available data width / rows (matching ratatui's draw_visual) ──
    // Use environment variables to allow test backends to control size.
    let (term_cols, term_rows) = {
        let env_cols = std::env::var("CORRO_TERM_COLS").ok().and_then(|s| s.parse().ok());
        let env_rows = std::env::var("CORRO_TERM_ROWS").ok().and_then(|s| s.parse().ok());
        if let (Some(c), Some(r)) = (env_cols, env_rows) {
            (c, r)
        } else {
            let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
            if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0
            {
                (ws.ws_col as usize, ws.ws_row as usize)
            } else {
                let cols = std::env::var("COLUMNS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(80);
                (cols, 50usize)
            }
        }
    };
    let data_width = term_cols
        .saturating_sub(2)
        .saturating_sub(ui_core::ROW_LABEL_CHARS)
        .max(1);
    let data_cols = data_width.checked_div(2).unwrap_or(1).max(1);

    // ── Viewport rows (matching ratatui's draw_visual) ──────────────────
    // The widget layout is: menu(1) + formula(1) + border(1) + header(1) +
    // separator(1) + data_rows + border_bottom(1) + status(1) = term_rows
    // Ratatui uses the same layout: menu(1) + formula(1) + grid(h) + hints(1)
    // where grid area height = term_rows - 3, inner_h = h - 2 (borders),
    // and data_rows = inner_h - 1 = term_rows - 6.
    let data_rows = term_rows
        .saturating_sub(6)
        .max(1);

    // ── Visible columns (matching ratatui's visible_col_indices) ──────
    let hr = HEADER_ROWS;
    let mr = app.core.workbook.active_sheet().grid.main_rows();
    let mc = app.core.workbook.active_sheet().grid.main_cols();

    let display_cursor_row = HEADER_ROWS;
    let display_cursor_col = MARGIN_COLS;
    app.core.cursor.row = display_cursor_row;
    app.core.cursor.col = display_cursor_col;
    app.core.anchor = Some(SheetCursor { row: HEADER_ROWS, col: MARGIN_COLS });

    let cursor = SheetCursor {
        row: display_cursor_row,
        col: display_cursor_col,
    };

    // ── Visible rows (matching ratatui's visible_row_indices) ──────────
    let sheet_rec = app.core.workbook.active_sheet().clone();
    let (display_rows, _row_scroll) =
        ui_core::visible_row_indices(&sheet_rec, cursor, data_rows, 0);

    // Use the live (pre-clone) grid for column width fitting, then clone
    // so the resulting overrides are present in the snapshot.
    let (mut col_ixs, _col_scroll) =
        ui_core::visible_col_indices(&sheet_rec, cursor, data_cols, 0);
    {
        let sht = app.core.workbook.active_sheet_mut();
        let grd = &mut sht.grid;
        // Match ratatui's fit_visible_columns_capped (proportional allocation)
        // then trim columns that don't fit.
        crate::ui_core::fit_visible_columns_capped(grd, &col_ixs, cursor.col, data_width);
        crate::ui_core::trim_visible_cols_to_width(grd, &mut col_ixs, cursor.col, data_width);
    }

    // Re-read the sheet after width adjustments.
    let sheet_rec = app.core.workbook.active_sheet().clone();
    let g = &sheet_rec.grid;
    let mr = g.main_rows();
    let mc = g.main_cols();
    let lm = MARGIN_COLS;

    // ── Column layout with widths matching ratatui's grid.col_width() ──
    // In ratatui, header and data rows use 1-char gaps everywhere (including
    // at left-margin→main and main→right-margin boundaries).  Only the
    // separator row draws a `│` at the boundary, handled by the widget.
    let mut layout: Vec<(u32, u32, String)> = Vec::new();
    let mut col_widths: HashMap<usize, usize> = HashMap::new();
    for &c in col_ixs.iter() {
        let w = g.col_width(c).max(1);
        col_widths.insert(c, w);
        let label = crate::addr::ui_column_fragment(c, mc);
        layout.push((c as u32, w as u32, label));
    }

    // ── Precompute aggregate info for each visible row ────────────────
    let row_agg_func = compute::compute_row_agg_func(g, &display_rows, hr, mr);

    // ── Spreadsheet ────────────────────────────────────────────────────
    let total_rows = display_rows.len() as u32;
    let total_cols = layout.len() as u32;
    let spreadsheet = create_spreadsheet(total_rows, total_cols)?;

    // Row labels
    let mut row_labels: Vec<(u32, String)> = Vec::new();
    for (idx, &r) in display_rows.iter().enumerate() {
        let label = crate::addr::ui_row_label(r, mr);
        row_labels.push((idx as u32, label));
    }
    spreadsheet.set_row_labels(row_labels);

    // ── Cell data for ALL visible rows and columns ────────────────────
    fill_cells(&spreadsheet, &display_rows, &col_ixs, &col_widths,
        g, hr, mr, mc, lm, data_width,
        display_cursor_row, display_cursor_col, &row_agg_func);

    spreadsheet.set_column_layout(layout);
    spreadsheet.set_grid_config(lm as u32, mc as u32);
    spreadsheet.set_row_counts(hr as u32, mr as u32);

    // Store cursor cell raw value at the cursor's position for formula bar lookup
    {
        let cursor_main_row = display_cursor_row.saturating_sub(hr);
        let cursor_col_addr = ColumnAddr::from_global(display_cursor_col, mc);
        let cursor_addr = if display_cursor_row < hr {
            CellAddr::Header { row: display_cursor_row as u32, col: cursor_col_addr }
        } else if display_cursor_row < hr + mr {
            if display_cursor_col < lm {
                CellAddr::Left { row: cursor_main_row as u32, col: display_cursor_col }
            } else if display_cursor_col < lm + mc {
                CellAddr::Main { row: cursor_main_row as u32, col: (display_cursor_col - lm) as u32 }
            } else {
                CellAddr::Right { row: cursor_main_row as u32, col: display_cursor_col - lm - mc }
            }
        } else {
            CellAddr::Footer { row: (display_cursor_row - hr - mr) as u32, col: cursor_col_addr }
        };
        let cursor_display_ri = display_rows.iter().position(|&r| r == display_cursor_row).unwrap_or(0);
        let cursor_raw_val = g.get(&cursor_addr).unwrap_or_default();
        spreadsheet.set_raw_cell(cursor_display_ri as u32, display_cursor_col as u32, &cursor_raw_val);
        // Also re-store the formatted display text for the cursor cell, so the
        // cells map always has correctly-aligned text regardless of any prior
        // fill_cells state.  Compute the effective display and align it to the
        // column width, matching fill_cells logic.
        {
            let cw_cursor = g.col_width(display_cursor_col).max(1);
            let effective_cursor = crate::formula::cell_effective_display(g, &cursor_addr);
            let formatted_cursor = crate::ui_core::format_cell_display(g, &cursor_addr, effective_cursor);
            let fw = formatted_cursor.width();
            let align_cursor = crate::ui_core::effective_cell_align(g, &cursor_addr, &formatted_cursor);
            let cursor_display_text = if fw > cw_cursor
                && (align_cursor.is_none() || align_cursor == Some(crate::grid::TextAlign::Left))
            {
                // Text would spill into adjacent columns — keep the full text
                // (matching fill_cells spill logic) so the widget renders
                // the overflow correctly instead of truncating it.
                formatted_cursor
            } else {
                crate::ui_core::align_cell_display(formatted_cursor, cw_cursor, align_cursor)
            };
            if !cursor_display_text.trim().is_empty() {
                spreadsheet.set_cell(cursor_display_ri as u32, display_cursor_col as u32, &cursor_display_text);
                spreadsheet.set_cell_style(cursor_display_ri as u32, display_cursor_col as u32, compute::CellDisplayStyle::Cursor.to_pancurses_style());
            }
        }
        spreadsheet.set_cursor(cursor_display_ri as u32, display_cursor_col as u32);

    }

    // Tab bar (styled matching ratatui: inactive=white fg+gray bg, active=bold+black fg+yellow bg)
    if app.core.workbook.sheet_count() > 1 {
        let titles: Vec<String> = app.core.workbook.sheets.iter()
            .map(|s| s.title.clone())
            .collect();
        let active = app.core.workbook.active_sheet;
        spreadsheet.set_tab_data(&titles, active);
    }

    // Border title
    let total_ops = app.core.ops_applied;
    let border_title =
        format!("corro  {}r × {}c  ops {}", mr, mc, total_ops);
    spreadsheet.set_border_title(&border_title);

    // Menu
    spreadsheet.set_menu_text(" [File]   Edit    Insert    Format    Sheet    Help");

    // Formula bar trailing: show app status text (matches ratatui's
    // mode_prompt_widget which appends "   ·  {status}" after the cell value).
    if !app.core.status.is_empty() {
        spreadsheet.set_formula_bar_trailing(&format!("   ·  {}", app.core.status));
    } else {
        spreadsheet.set_formula_bar_trailing("");
    }

    win.set_child(&spreadsheet);
    rustxwidgets::backends::pancurses::set_focus(spreadsheet.id());
    win.present();

    // ── Cursor move callback: grow grid extent + update viewport ─────────
    // The ratatui backend recomputes the viewport each frame so pressing arrow
    // keys scrolls the visible columns/rows and the grid extent grows as needed.
    let display_rows_for_cb = std::rc::Rc::new(std::cell::RefCell::new(display_rows.clone()));
    let display_rows_for_ce = display_rows_for_cb.clone();
    let mut col_ixs_cb = col_ixs.clone();
    let sheet_cb = spreadsheet.clone();
    let sid = spreadsheet.id();
    let app_ptr: *mut super::App = app;
    let hr_cb = hr;
    let hr_ce = hr;
    let mr_cb = mr;
    let lm_ce = lm;
    let data_rows_cb = data_rows;
    let data_cols_cb = data_cols;
    let data_width_cb = data_width;
    let mut prev_cursor_col = cursor.col;
    let mut prev_cursor_row = cursor.row;
    add_cursor_move_callback(move |_display_row, _display_col| {
        // SAFETY: app is &mut App alive for the entire event loop
        let app = unsafe { &mut *app_ptr };

        // Sync formula bar trailing with app status (ratatui shows status in formula bar)
        if !app.core.status.is_empty() {
            sheet_cb.set_formula_bar_trailing(&format!("   ·  {}", app.core.status));
        } else {
            sheet_cb.set_formula_bar_trailing("");
        }

        let display_idx = _display_row as usize;
        let mut need_viewport_recompute = false;

        // Handle sentinel values for scrolling past viewport boundaries
        if _display_row == u32::MAX {
            // Scroll up sentinel: user pressed Up at the first visible row
            if app.core.cursor.row > 0 {
                app.core.cursor.row -= 1;
            }
            need_viewport_recompute = true;
        } else if _display_row == u32::MAX - 1 {
            // Scroll down sentinel: user pressed Down at the last visible row.
            // Grow the grid if the cursor is at the last main row with few
            // trailing blanks (matching ratatui's move_cursor_one_row_vertical).
            let cursor_row = app.core.cursor.row;
            {
                let sheet = app.core.workbook.active_sheet_mut();
                let mr = sheet.grid.main_rows();
                if cursor_row == hr_cb + mr.saturating_sub(1)
                    && compute::trailing_blank_main_rows(&sheet.grid) < crate::ui_core::NAV_BLANK_ROWS
                {
                    sheet.grid.grow_main_row_at_bottom();
                }
            }
            app.core.cursor.row += 1;
            need_viewport_recompute = true;
        } else if let Some(&logical_row) = display_rows_for_cb.borrow().get(display_idx) {
            app.core.cursor.row = logical_row;

            // Grow the grid if cursor moves from the last main row with few
            // trailing blanks (matching ratatui's move_cursor_one_row_vertical).
            if logical_row >= hr_cb + mr_cb {
                let sheet = app.core.workbook.active_sheet_mut();
                let cur_mr = sheet.grid.main_rows();
                if prev_cursor_row == hr_cb + cur_mr.saturating_sub(1)
                    && compute::trailing_blank_main_rows(&sheet.grid) < crate::ui_core::NAV_BLANK_ROWS
                {
                    sheet.grid.grow_main_row_at_bottom();
                }
            }

            // Check if cursor moved into header or footer region; if so,
            // recompute the viewport so those rows become visible.
            if logical_row < hr_cb || logical_row >= hr_cb + mr_cb {
                need_viewport_recompute = true;
            }
        }
        app.core.cursor.col = _display_col as usize;

        if need_viewport_recompute {
            let cursor = app.core.cursor;
            // Determine viewport and fit columns BEFORE cloning so the
            // resulting width overrides are reflected in the snapshot.
            let (new_display_rows, new_mr, new_mc, mut new_ixs) = {
                let rec = app.core.workbook.active_sheet().clone();
                let (new_display_rows, _) =
                    crate::ui_core::visible_row_indices(&rec, cursor, data_rows_cb, 0);
                let new_mr = rec.grid.main_rows();
                let new_mc = rec.grid.main_cols();
                let (mut new_ixs, _) =
                    crate::ui_core::visible_col_indices(&rec, cursor, data_cols_cb, 0);
                // Fit and trim columns on the live grid so the clone inherits the widths.
                {
                    let sht = app.core.workbook.active_sheet_mut();
                    crate::ui_core::fit_visible_columns_capped(
                        &mut sht.grid, &new_ixs, cursor.col, data_width_cb,
                    );
                    crate::ui_core::trim_visible_cols_to_width(
                        &mut sht.grid, &mut new_ixs, cursor.col, data_width_cb,
                    );
                }
                (new_display_rows, new_mr, new_mc, new_ixs)
            };
            // Re-read the sheet after width adjustments.
            let rec = app.core.workbook.active_sheet().clone();
            // Update border title when grid grew (matching the non-recompute path)
            let boundary_title = format!(
                "corro  {}r × {}c  ops {}",
                new_mr, new_mc, app.core.ops_applied
            );
            spreadsheet_set_border_title(sid, &boundary_title);
            let new_labels: Vec<(u32, String)> = new_display_rows.iter()
                .enumerate()
                .map(|(idx, &r)| {
                    let label = crate::addr::ui_row_label(r, new_mr);
                    (idx as u32, label)
                })
                .collect();
            spreadsheet_set_row_labels(sid, new_labels);
            // Update column layout with fitted widths
            {
                let g = &rec.grid;
                let new_layout: Vec<(u32, u32, String)> = new_ixs
                    .iter()
                    .map(|&c| {
                        let w = g.col_width(c).max(1);
                        let label = crate::addr::ui_column_fragment(c, new_mc);
                        (c as u32, w as u32, label)
                    })
                    .collect();
                spreadsheet_set_column_layout(sid, new_layout);
                col_ixs_cb = new_ixs.clone();
                spreadsheet_set_grid_config(sid, MARGIN_COLS as u32, new_mc as u32);
            }
            // Repopulate all visible cells for the new viewport
            let new_col_widths: HashMap<usize, usize> = col_ixs_cb.iter()
                .map(|&c| (c, rec.grid.col_width(c).max(1)))
                .collect();
            let new_row_agg = compute::compute_row_agg_func(&rec.grid, &new_display_rows, hr_cb, new_mr);
            fill_cells(
                &sheet_cb, &new_display_rows, &col_ixs_cb, &new_col_widths,
                &rec.grid, hr_cb, new_mr, new_mc, MARGIN_COLS, data_width_cb,
                cursor.row, cursor.col, &new_row_agg,
            );
            if let Some(new_display_ri) = new_display_rows.iter().position(|&r| r == cursor.row) {
                let cursor_addr = crate::addr::sheet_cursor_to_addr(
                    crate::addr::LogicalRow(cursor.row),
                    crate::addr::GlobalCol(cursor.col),
                    crate::addr::MainRows(new_mr),
                    crate::addr::MainCols(new_mc),
                );
                if let Some(raw_val) = rec.grid.get(&cursor_addr) {
                    sheet_cb.set_raw_cell(new_display_ri as u32, cursor.col as u32, &raw_val);
                } else {
                    sheet_cb.set_raw_cell(new_display_ri as u32, cursor.col as u32, "");
                }
                sheet_cb.set_cursor(new_display_ri as u32, cursor.col as u32);
            }
            *display_rows_for_cb.borrow_mut() = new_display_rows;
            prev_cursor_row = app.core.cursor.row;
            prev_cursor_col = app.core.cursor.col;
            return;
        }

        if let Some(&logical_row) = display_rows_for_cb.borrow().get(display_idx) {
            let sheet = app.core.workbook.active_sheet_mut();
            let prev_mr = sheet.grid.main_rows();
            let prev_mc = sheet.grid.main_cols();
            let lm = MARGIN_COLS;

            // Expand main columns when moving right past the last main column
            // (matching ratatui's move_cursor_one_col_horizontal).
            let new_col = _display_col as usize;
            if new_col > prev_cursor_col {
                if prev_cursor_col == lm + prev_mc.saturating_sub(1)
                    && compute::trailing_blank_main_cols(&sheet.grid) < crate::ui_core::NAV_BLANK_COLS
                {
                    sheet.grid.grow_main_col_at_right();
                }
            }

            // Expand main rows when moving down from the last main row
            // (matching ratatui's move_cursor_one_row_vertical).
            if logical_row > prev_cursor_row {
                if prev_cursor_row == hr_cb + prev_mr.saturating_sub(1)
                    && compute::trailing_blank_main_rows(&sheet.grid) < crate::ui_core::NAV_BLANK_ROWS
                {
                    sheet.grid.grow_main_row_at_bottom();
                }
            }

            prev_cursor_col = new_col;
            prev_cursor_row = logical_row;

            // When cursor moves to the row just beyond the current extent,
            // grow the grid (matching ratatui's move_cursor_one_row_vertical)
            // for cases where the cursor jumps multiple rows at once.
            // Use the current main row count (after any growth above) to
            // avoid redundant growth: prev_mr may be stale if the earlier
            // row-growth condition at line ~753 already fired.
            let cur_mr = sheet.grid.main_rows();
            if logical_row >= hr_cb + cur_mr {
                sheet.grid.grow_main_row_at_bottom();
            }
            sheet.grid.ensure_extent_for_cursor(logical_row, _display_col as usize);
            if sheet.grid.main_rows() != prev_mr || sheet.grid.main_cols() != prev_mc {
                // Grid grew — update border title and row labels
                let mr = sheet.grid.main_rows();
                let mc = sheet.grid.main_cols();
                let boundary_title = format!(
                    "corro  {}r × {}c  ops {}",
                    mr, mc, app.core.ops_applied
                );
                spreadsheet_set_border_title(sid, &boundary_title);
                let new_labels: Vec<(u32, String)> = display_rows_for_cb.borrow().iter()
                    .enumerate()
                    .map(|(idx, &r)| {
                        let label = crate::addr::ui_row_label(r, mr);
                        (idx as u32, label)
                    })
                    .collect();
                spreadsheet_set_row_labels(sid, new_labels);
                // Also update column layout when main columns grew
                let rec = app.core.workbook.active_sheet().clone();
                let cursor = app.core.cursor;
                let (mut new_ixs, _) =
                    crate::ui_core::visible_col_indices(&rec, cursor, data_cols_cb, 0);
                // Fit and trim columns on the live grid so the clone inherits the widths.
                {
                    let sht = app.core.workbook.active_sheet_mut();
                    crate::ui_core::fit_visible_columns_capped(
                        &mut sht.grid, &new_ixs, cursor.col, data_width_cb,
                    );
                    crate::ui_core::trim_visible_cols_to_width(
                        &mut sht.grid, &mut new_ixs, cursor.col, data_width_cb,
                    );
                }
                let rec = app.core.workbook.active_sheet().clone();
                let g = &rec.grid;
                let mc = g.main_cols();
                let new_layout: Vec<(u32, u32, String)> = new_ixs
                    .iter()
                    .map(|&c| {
                        let w = g.col_width(c).max(1);
                        let label = crate::addr::ui_column_fragment(c, mc);
                        (c as u32, w as u32, label)
                    })
                    .collect();
                spreadsheet_set_column_layout(sid, new_layout);
                col_ixs_cb = new_ixs;
                // Keep the widget's margin_cols and main_cols in sync
                spreadsheet_set_grid_config(sid, lm as u32, mc as u32);
                // Repopulate cells with updated column layout after growth
                let dr: Vec<usize> = display_rows_for_cb.borrow().clone();
                let new_col_widths: HashMap<usize, usize> = col_ixs_cb.iter()
                    .map(|&c| (c, g.col_width(c).max(1)))
                    .collect();
                let new_row_agg = compute::compute_row_agg_func(g, &dr, hr_cb, mr);
                fill_cells(
                    &sheet_cb, &dr, &col_ixs_cb, &new_col_widths,
                    g, hr_cb, mr, mc, MARGIN_COLS, data_width_cb,
                    cursor.row, cursor.col, &new_row_agg,
                );
            } else if !col_ixs_cb.contains(&(_display_col as usize)) {
                // Update column viewport when cursor column moves outside the
                // currently visible range (matching ratatui's per-frame recompute).
                let rec = app.core.workbook.active_sheet().clone();
                let cursor = app.core.cursor;
                let (mut new_ixs, _) =
                    crate::ui_core::visible_col_indices(&rec, cursor, data_cols_cb, 0);
                // Fit and trim columns on the live grid so the clone inherits the widths.
                {
                    let sht = app.core.workbook.active_sheet_mut();
                    crate::ui_core::fit_visible_columns_capped(
                        &mut sht.grid, &new_ixs, cursor.col, data_width_cb,
                    );
                    crate::ui_core::trim_visible_cols_to_width(
                        &mut sht.grid, &mut new_ixs, cursor.col, data_width_cb,
                    );
                }
                let rec = app.core.workbook.active_sheet().clone();
                let g = &rec.grid;
                let mc = g.main_cols();
                let new_layout: Vec<(u32, u32, String)> = new_ixs
                    .iter()
                    .map(|&c| {
                        let w = g.col_width(c).max(1);
                        let label = crate::addr::ui_column_fragment(c, mc);
                        (c as u32, w as u32, label)
                    })
                    .collect();
                spreadsheet_set_column_layout(sid, new_layout);
                col_ixs_cb = new_ixs;
                // Repopulate cells with updated column viewport
                let dr: Vec<usize> = display_rows_for_cb.borrow().clone();
                let new_col_widths: HashMap<usize, usize> = col_ixs_cb.iter()
                    .map(|&c| (c, g.col_width(c).max(1)))
                    .collect();
                let new_row_agg = compute::compute_row_agg_func(g, &dr, hr_cb, mc);
                fill_cells(
                    &sheet_cb, &dr, &col_ixs_cb, &new_col_widths,
                    g, hr_cb, g.main_rows(), mc, MARGIN_COLS, data_width_cb,
                    cursor.row, cursor.col, &new_row_agg,
                );
            } else {
                // Cursor moved within the current viewport — refresh cells to ensure
                // formatted display values are used (commits overwrite cells with raw values).
                let rec = app.core.workbook.active_sheet().clone();
                let g = &rec.grid;
                let mc = g.main_cols();
                let cursor = app.core.cursor;
                let dr: Vec<usize> = display_rows_for_cb.borrow().clone();
                let new_col_widths: HashMap<usize, usize> = col_ixs_cb.iter()
                    .map(|&c| (c, g.col_width(c).max(1)))
                    .collect();
                let new_row_agg = compute::compute_row_agg_func(g, &dr, hr_cb, g.main_rows());
                fill_cells(
                    &sheet_cb, &dr, &col_ixs_cb, &new_col_widths,
                    g, hr_cb, g.main_rows(), mc, MARGIN_COLS, data_width_cb,
                    cursor.row, cursor.col, &new_row_agg,
                );
            }
        }
    });

    // ── Commit edit callback: persist cell edits to workbook ──────────
    // After the commit, re-align the committed cell so the grid shows
    // the correctly-aligned display text (spreadsheet_commit_edit stores
    // the RAW value in the cells HashMap, but display text must be aligned).
    let commit_sheet = spreadsheet.clone();
    let app_ptr_ce = app_ptr;
    add_commit_edit_callback(move |display_row, col, value| {
        let app = unsafe { &mut *app_ptr_ce };
        let dr = display_rows_for_ce.borrow();
        let logical_row = dr.get(display_row as usize).copied().unwrap_or(0);
        let main_row = logical_row.saturating_sub(hr_ce);
        let main_col = col.saturating_sub(lm_ce as u32);
        let addr = CellAddr::Main { row: main_row as u32, col: main_col as u32 };
        let sheet_id = app.core.workbook.sheet_id(app.core.workbook.active_sheet);
        let op = Op::SetCell { addr, value };
        let wbo = WorkbookOp::SheetOp { sheet_id, op };
        if let Some(ref p) = app.core.path.clone() {
            let mut active_sheet = sheet_id;
            let _ = crate::io::commit_workbook_op(
                p,
                &mut app.core.offset,
                &mut app.core.workbook,
                &mut active_sheet,
                &wbo,
            );
            app.core.ops_applied = app.core.ops_applied.saturating_add(1);
        }
        // Re-align the committed cell's display text (spreadsheet_commit_edit
        // stored the raw value, but we need the aligned version).
        let rec = app.core.workbook.active_sheet().clone();
        let g = &rec.grid;
        if let Some(effective) = logical_row.checked_sub(hr_ce).and_then(|mr| {
            g.get(&CellAddr::Main { row: mr as u32, col: main_col as u32 })
        }) {
            let formatted = crate::ui_core::format_cell_display(g, &addr, effective);
            let fw = formatted.width();
            let cw = g.col_width(col as usize).max(1);
            let align = crate::ui_core::effective_cell_align(g, &addr, &formatted);
            let aligned = if fw > cw
                && (align.is_none() || align == Some(crate::grid::TextAlign::Left))
            {
                // Text would spill into adjacent columns — keep the full text
                // (matching fill_cells spill logic).
                formatted
            } else {
                crate::ui_core::align_cell_display(formatted, cw, align)
            };
            commit_sheet.set_cell(display_row, col, &aligned);
        }
    });

    _backend.run().map_err(|e| format!("pancurses error: {e}"))?;
    Ok(())
}




