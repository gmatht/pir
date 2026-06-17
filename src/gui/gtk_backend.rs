#![allow(non_upper_case_globals)]

use rustxwidgets::backends_gtk_adapter::*;
use rustxwidgets::core::DrawContext;

use crate::grid::{CellAddr, ColumnAddr, HEADER_ROWS, MARGIN_COLS};
use crate::ui_core;

use super::compute;
use super::edit::{KEY_BACKSPACE, KEY_DELETE, KEY_DOWN, KEY_END, KEY_ENTER, KEY_ESC,
    KEY_F2, KEY_HOME, KEY_LEFT, KEY_PAGE_DOWN, KEY_PAGE_UP, KEY_RETURN,
    KEY_RIGHT, KEY_TAB, KEY_UP};

const CELL_W: f64 = 100.0;
const CELL_H: f64 = 28.0;
const HEADER_W: f64 = 50.0;
const HEADER_H: f64 = 28.0;
const VISIBLE_ROWS: u32 = 30;
const VISIBLE_COLS: u32 = 12;

struct GtkCanvasState {
    app: *mut super::App,
    formula_entry: Entry,
    addr_label: Label,
    status_label: Label,
    canvas: Canvas,
    scroll_row: std::cell::Cell<u32>,
    scroll_col: std::cell::Cell<u32>,
    edit_buf: std::cell::RefCell<String>,
    editing: std::cell::Cell<bool>,
    last_row: std::cell::Cell<usize>,
    last_col: std::cell::Cell<usize>,
}

pub fn run_gtk(app: &mut super::App) -> Result<(), Box<dyn std::error::Error>> {
    let _backend = rustxwidgets::backends::gtk::init()
        .map_err(|e| format!("GTK init failed: {e}"))?;

    let win = create_window()?;
    win.set_title(&format!("corro {}", env!("CARGO_PKG_VERSION")));
    win.set_default_size(1200, 800);

    let gtk_app = create_application()?;
    gtk_app.register()?;

    let menubar = crate::gui::menu::build_menu_bar(&gtk_app, &win)?;

    let vbox = create_box(Orientation::Vertical, 0)?;
    vbox.append(&menubar);

    // Formula bar
    let formula_bar = create_box(Orientation::Horizontal, 2)?;
    let addr_label = create_label("A1")?;
    addr_label.set_xalign(0.0);
    addr_label.set_size_request(80, 28);
    let f_label = create_label("  fx  ")?;
    let formula_entry = create_entry()?;
    formula_entry.set_hexpand(true);
    formula_bar.append(&addr_label);
    formula_bar.append(&f_label);
    formula_bar.append(&formula_entry);
    vbox.append(&formula_bar);

    let spreadsheet = create_spreadsheet(200, 50)?;

    // Fit column widths to rendered content (matching ratatui)
    app.fit_main_columns_to_max_width();

    let hr = HEADER_ROWS;
    let lm = MARGIN_COLS;

    let cursor_row = hr;
    let cursor_col = lm;
    app.core.cursor.row = cursor_row;
    app.core.cursor.col = cursor_col;
    app.core.anchor = Some(crate::grid::SheetCursor { row: hr, col: lm });

    let state = GtkCanvasState {
        app,
        formula_entry: formula_entry.clone(),
        addr_label: addr_label.clone(),
        status_label: create_label("Ready")?,
        canvas: spreadsheet.canvas().clone(),
        scroll_row: std::cell::Cell::new(0),
        scroll_col: std::cell::Cell::new(0),
        edit_buf: std::cell::RefCell::new(String::new()),
        editing: std::cell::Cell::new(false),
        last_row: std::cell::Cell::new(cursor_row),
        last_col: std::cell::Cell::new(cursor_col),
    };

    let state = std::rc::Rc::new(state);

    update_formula_bar(&state, cursor_row, cursor_col);

    spreadsheet.set_draw_callback(Box::new({
        let state = state.clone();
        let sheet_state = app.core.workbook.active_sheet().clone();
        move |dc: &mut dyn DrawContext, _w: i32, _h: i32| {
            render_grid(dc, &state, &sheet_state);
        }
    }));

    spreadsheet.on_key(Box::new({
        let state = state.clone();
        move |keyval: u32, _state: u32| -> bool {
            handle_key(keyval, &state)
        }
    }));

    spreadsheet.on_click(Box::new({
        let state = state.clone();
        move |x: f64, y: f64| {
            let col = ((x - HEADER_W) / CELL_W) as i32 + state.scroll_col.get() as i32;
            let row = ((y - HEADER_H) / CELL_H) as i32 + state.scroll_row.get() as i32;
            if col >= 0 && row >= 0 {
                state.last_col.set(col as usize);
                state.last_row.set(row as usize);
                update_state_cursor(&state, row as usize, col as usize);
                state.canvas.queue_redraw();
            }
        }
    }));

    let status_bar = state.status_label.clone();
    vbox.append(&spreadsheet);
    vbox.append(&status_bar);
    win.set_child(&vbox);
    win.present();

    _backend.run().map_err(|e| format!("GUI error: {e}"))?;
    Ok(())
}

fn render_grid(dc: &mut dyn DrawContext, state: &GtkCanvasState, sheet_state: &crate::ops::SheetState) {
    dc.clear(1.0, 1.0, 1.0, 1.0);

    let g = &sheet_state.grid;
    let hr = HEADER_ROWS;
    let mr = g.main_rows();
    let mc = g.main_cols();
    let lm = MARGIN_COLS;
    let top_r = state.scroll_row.get() as usize;
    let left_c = state.scroll_col.get() as usize;

    // Determine visible rows/cols from the workbook model
    let data_width = 200usize; // approximate
    let cursor = crate::grid::SheetCursor {
        row: state.last_row.get(),
        col: state.last_col.get(),
    };
    let (visible_rows, _) = ui_core::visible_row_indices(
        sheet_state, cursor, VISIBLE_ROWS as usize, top_r,
    );
    let (mut col_ixs, _) = ui_core::visible_col_indices(
        sheet_state, cursor, VISIBLE_COLS as usize, left_c,
    );
    ui_core::trim_visible_cols_to_width(g, &mut col_ixs, cursor.col, data_width);

    // Ensure column widths are fitted
    // Note: rendered_width_for_column needs a &GridBox, but set_col_width needs &mut.
    // Canvas backends refresh the viewport on each redraw so widths are recalculated.
    // Use col_widths which were already fitted during the cursor-move callback.

    let col_widths: std::collections::HashMap<usize, usize> = col_ixs.iter()
        .map(|&c| (c, g.col_width(c).max(1)))
        .collect();

    let row_agg_func = compute::compute_row_agg_func(g, &visible_rows, hr, mr);

    dc.save();

    // Draw column headers
    for (ci, &c) in col_ixs.iter().enumerate() {
        let x = HEADER_W + ci as f64 * CELL_W;
        let label = crate::addr::ui_column_fragment(c, mc);
        dc.fill_rect(x, 0.0, CELL_W, HEADER_H, 0.92, 0.92, 0.92, 1.0);
        let te = dc.text_extents(&label, "monospace", 12.0);
        dc.draw_text(x + (CELL_W - te.2) / 2.0, HEADER_H - 8.0,
            &label, "monospace", 12.0, 0.2, 0.2, 0.2, 1.0);
    }

    // Draw row headers
    for (ri, &r) in visible_rows.iter().enumerate() {
        let y = HEADER_H + ri as f64 * CELL_H;
        let label = crate::addr::ui_row_label(r, mr);
        dc.fill_rect(0.0, y, HEADER_W, CELL_H, 0.92, 0.92, 0.92, 1.0);
        let te = dc.text_extents(&label, "monospace", 12.0);
        dc.draw_text(HEADER_W - te.2 - 5.0, y + CELL_H - 8.0,
            &label, "monospace", 12.0, 0.2, 0.2, 0.2, 1.0);
    }

    // Grid outline
    let grid_w = col_ixs.len() as f64 * CELL_W;
    let grid_h = visible_rows.len() as f64 * CELL_H;
    dc.stroke_rect(HEADER_W, 0.0, grid_w, grid_h, 0.85, 0.85, 0.85, 1.0, 0.5);

    // Draw cells
    for (ri, &logical_row) in visible_rows.iter().enumerate() {
        for (ci, &c) in col_ixs.iter().enumerate() {
            let addr = cell_addr_for(logical_row, hr, mr, c, lm, mc);
            let is_cursor = logical_row == state.last_row.get() && c == state.last_col.get();
            let is_editing = state.editing.get() && is_cursor;
            let x = HEADER_W + ci as f64 * CELL_W;
            let y = HEADER_H + ri as f64 * CELL_H;

            let cell_info = compute::compute_cell_info(
                g, &addr, is_cursor && !is_editing,
                row_agg_func.get(ri).copied().flatten(),
                if logical_row >= hr && logical_row < hr + mr { Some((logical_row - hr) as u32) } else { None },
                if logical_row >= hr + mr { Some((logical_row - hr - mr) as u32) } else { None },
                compute::right_col_agg(g, c),
                c, lm, mc, mr,
            );

            if is_editing {
                dc.fill_rect(x, y, CELL_W, CELL_H, 0.8, 1.0, 0.8, 0.3);
                dc.stroke_rect(x, y, CELL_W, CELL_H, 0.0, 0.7, 0.0, 1.0, 2.0);
                let buf = state.edit_buf.borrow();
                dc.draw_text(x + 3.0, y + CELL_H - 8.0, &buf, "monospace", 12.0, 0.0, 0.0, 0.0, 1.0);
            } else if is_cursor {
                dc.fill_rect(x, y, CELL_W, CELL_H, 0.2, 0.5, 1.0, 0.15);
                dc.stroke_rect(x, y, CELL_W, CELL_H, 0.2, 0.5, 1.0, 1.0, 1.5);
                if !cell_info.formatted.is_empty() {
                    dc.draw_text(x + 3.0, y + CELL_H - 8.0, &cell_info.formatted, "monospace", 12.0, 0.0, 0.0, 0.0, 1.0);
                }
            } else if matches!(cell_info.style, compute::CellDisplayStyle::Aggregate | compute::CellDisplayStyle::FooterAggregate) {
                dc.fill_rect(x, y, CELL_W, CELL_H, 0.95, 0.95, 0.99, 1.0);
                if !cell_info.formatted.is_empty() {
                    dc.draw_text(x + 3.0, y + CELL_H - 8.0, &cell_info.formatted, "monospace", 12.0, 0.0, 0.0, 0.5, 1.0);
                }
            } else if !cell_info.formatted.is_empty() {
                dc.draw_text(x + 3.0, y + CELL_H - 8.0, &cell_info.formatted, "monospace", 12.0, 0.0, 0.0, 0.0, 1.0);
            }

            // Draw cell grid lines
            dc.stroke_rect(x, y, CELL_W, CELL_H, 0.9, 0.9, 0.9, 1.0, 0.5);
        }
    }

    dc.restore();
}

fn handle_key(keyval: u32, state: &GtkCanvasState) -> bool {
    let app = unsafe { &mut *state.app };

    if state.editing.get() {
        match keyval {
            KEY_RETURN | KEY_ENTER => {
                commit_edit(state);
                move_cursor(state, 1, 0);
                return true;
            }
            KEY_ESC => {
                state.editing.set(false);
                state.edit_buf.borrow_mut().clear();
                update_formula_bar(state, state.last_row.get(), state.last_col.get());
                state.canvas.queue_redraw();
                return true;
            }
            KEY_TAB => {
                commit_edit(state);
                move_cursor(state, 0, 1);
                return true;
            }
            KEY_BACKSPACE => {
                state.edit_buf.borrow_mut().pop();
                state.canvas.queue_redraw();
                return true;
            }
            KEY_DELETE => {
                state.edit_buf.borrow_mut().clear();
                state.canvas.queue_redraw();
                return true;
            }
            KEY_LEFT => {
                commit_and_move(state, |_s| {
                    let c = state.last_col.get();
                    if c > 0 { state.last_col.set(c - 1); }
                });
                return true;
            }
            KEY_RIGHT => {
                commit_and_move(state, |_s| {
                    state.last_col.set(state.last_col.get() + 1);
                });
                return true;
            }
            KEY_UP => {
                commit_and_move(state, |_s| {
                    let r = state.last_row.get();
                    if r > HEADER_ROWS { state.last_row.set(r - 1); }
                });
                return true;
            }
            KEY_DOWN => {
                commit_and_move(state, |_s| {
                    state.last_row.set(state.last_row.get() + 1);
                });
                return true;
            }
            _ => {
                if keyval >= 32 && keyval <= 126 {
                    state.edit_buf.borrow_mut().push(char::from_u32(keyval).unwrap_or('?'));
                    state.formula_entry.set_text(&state.edit_buf.borrow());
                    state.canvas.queue_redraw();
                    return true;
                }
            }
        }
        return false;
    }

    // Non-editing key handling
    match keyval {
        KEY_LEFT => {
            let c = state.last_col.get();
            if c > 0 {
                state.last_col.set(c - 1);
                update_state_cursor(state, state.last_row.get(), state.last_col.get());
            }
        }
        KEY_RIGHT => {
            state.last_col.set(state.last_col.get() + 1);
            update_state_cursor(state, state.last_row.get(), state.last_col.get());
        }
        KEY_UP => {
            let r = state.last_row.get();
            if r > HEADER_ROWS {
                state.last_row.set(r - 1);
                update_state_cursor(state, state.last_row.get(), state.last_col.get());
            }
        }
        KEY_DOWN => {
            state.last_row.set(state.last_row.get() + 1);
            update_state_cursor(state, state.last_row.get(), state.last_col.get());
        }
        KEY_RETURN | KEY_ENTER => start_edit(state),
        KEY_TAB => {
            state.last_col.set(state.last_col.get() + 1);
            update_state_cursor(state, state.last_row.get(), state.last_col.get());
        }
        KEY_HOME => {
            state.last_col.set(MARGIN_COLS);
            update_state_cursor(state, state.last_row.get(), state.last_col.get());
        }
        KEY_END => {
            let mc = app.core.workbook.active_sheet().grid.main_cols();
            state.last_col.set(MARGIN_COLS + mc - 1);
            update_state_cursor(state, state.last_row.get(), state.last_col.get());
        }
        KEY_PAGE_UP => {
            let p = VISIBLE_ROWS as usize;
            if state.last_row.get() >= p {
                state.last_row.set(state.last_row.get() - p);
            } else {
                state.last_row.set(HEADER_ROWS);
            }
            update_state_cursor(state, state.last_row.get(), state.last_col.get());
        }
        KEY_PAGE_DOWN => {
            let mr = app.core.workbook.active_sheet().grid.main_rows();
            let p = VISIBLE_ROWS as usize;
            let n = (state.last_row.get() + p).min(HEADER_ROWS + mr - 1);
            state.last_row.set(n);
            update_state_cursor(state, n, state.last_col.get());
        }
        KEY_DELETE => {
            let sheet = app.core.workbook.active_sheet_mut();
            let main_row = state.last_row.get().saturating_sub(HEADER_ROWS);
            let main_col = state.last_col.get().saturating_sub(MARGIN_COLS);
            let addr = CellAddr::Main { row: main_row as u32, col: main_col as u32 };
            sheet.grid.set(&addr, String::new());
            state.formula_entry.set_text("");
            state.canvas.queue_redraw();
        }
        KEY_F2 => start_edit(state),
        _ => {
            if keyval >= 32 && keyval <= 126 {
                start_edit_with(state, char::from_u32(keyval).unwrap_or('?'));
            } else {
                return false;
            }
        }
    }
    update_formula_bar(state, state.last_row.get(), state.last_col.get());
    state.canvas.queue_redraw();
    true
}

fn commit_edit(state: &GtkCanvasState) {
    let app = unsafe { &mut *state.app };
    let text = state.edit_buf.borrow().clone();
    let main_row = state.last_row.get().saturating_sub(HEADER_ROWS);
    let main_col = state.last_col.get().saturating_sub(MARGIN_COLS);
    let addr = CellAddr::Main { row: main_row as u32, col: main_col as u32 };
    {
        let sheet = app.core.workbook.active_sheet_mut();
        sheet.grid.set(&addr, text.clone());
    }
    let sheet_id = app.core.workbook.sheet_id(app.core.workbook.active_sheet);
    let op = crate::ops::Op::SetCell { addr, value: text };
    let wbo = crate::ops::WorkbookOp::SheetOp { sheet_id, op };
    if let Some(ref p) = app.core.path.clone() {
        let mut active_sheet = sheet_id;
        let _ = crate::io::commit_workbook_op(
            p, &mut app.core.offset, &mut app.core.workbook,
            &mut active_sheet, &wbo,
        );
        app.core.ops_applied = app.core.ops_applied.saturating_add(1);
    }
    state.editing.set(false);
    state.edit_buf.borrow_mut().clear();
}

fn commit_and_move(state: &GtkCanvasState, f: impl FnOnce(&GtkCanvasState)) {
    commit_edit(state);
    f(state);
    update_state_cursor(state, state.last_row.get(), state.last_col.get());
}

fn start_edit(state: &GtkCanvasState) {
    let app = unsafe { &mut *state.app };
    let main_row = state.last_row.get().saturating_sub(HEADER_ROWS);
    let main_col = state.last_col.get().saturating_sub(MARGIN_COLS);
    let addr = CellAddr::Main { row: main_row as u32, col: main_col as u32 };
    let sheet = app.core.workbook.active_sheet();
    if let Some(val) = sheet.grid.get(&addr) {
        *state.edit_buf.borrow_mut() = val;
    } else {
        state.edit_buf.borrow_mut().clear();
    }
    state.editing.set(true);
    state.formula_entry.set_text("");
    state.canvas.queue_redraw();
}

fn start_edit_with(state: &GtkCanvasState, ch: char) {
    start_edit(state);
    state.edit_buf.borrow_mut().push(ch);
    state.canvas.queue_redraw();
}

fn move_cursor(state: &GtkCanvasState, dr: usize, dc: usize) {
    let new_row = state.last_row.get() + dr;
    let new_col = state.last_col.get() + dc;
    state.last_row.set(new_row);
    state.last_col.set(new_col);
    update_state_cursor(state, new_row, new_col);
}

fn update_state_cursor(state: &GtkCanvasState, row: usize, col: usize) {
    let app = unsafe { &mut *state.app };
    app.core.cursor.row = row;
    app.core.cursor.col = col;
    // Grow grid if needed
    let sheet = app.core.workbook.active_sheet_mut();
    let mr = sheet.grid.main_rows();
    let mc = sheet.grid.main_cols();
    let main_row = row.saturating_sub(HEADER_ROWS);
    let main_col = col.saturating_sub(MARGIN_COLS);
    if main_col >= mc && compute::trailing_blank_main_cols(&sheet.grid) < crate::ui_core::NAV_BLANK_COLS {
        sheet.grid.grow_main_col_at_right();
    }
    if main_row >= mr && compute::trailing_blank_main_rows(&sheet.grid) < crate::ui_core::NAV_BLANK_ROWS {
        sheet.grid.grow_main_row_at_bottom();
    }
    sheet.grid.ensure_extent_for_cursor(row, col);
    update_formula_bar(state, row, col);
    state.canvas.queue_redraw();
}

fn update_formula_bar(state: &GtkCanvasState, row: usize, col: usize) {
    let app = unsafe { &mut *state.app };
    let sheet = app.core.workbook.active_sheet();
    let main_row = row.saturating_sub(HEADER_ROWS);
    let main_col = col.saturating_sub(MARGIN_COLS);
    let addr = CellAddr::Main { row: main_row as u32, col: main_col as u32 };
    let val = sheet.grid.get(&addr).unwrap_or_default();
    state.formula_entry.set_text(&val);
    let addr_str = format!("{}",
        crate::addr::sheet_cursor_to_addr(
            crate::addr::LogicalRow(row),
            crate::addr::GlobalCol(col),
            crate::addr::MainRows(sheet.grid.main_rows()),
            crate::addr::MainCols(sheet.grid.main_cols()),
        )
    );
    state.addr_label.set_text(&addr_str);
    if !app.core.status.is_empty() {
        state.status_label.set_text(&app.core.status);
    } else {
        state.status_label.set_text("Ready");
    }
}

fn cell_addr_for(logical_row: usize, hr: usize, mr: usize, c: usize, lm: usize, mc: usize) -> CellAddr {
    if logical_row < hr {
        let hdr_row = logical_row as u32;
        if c < lm {
            CellAddr::Header { row: hdr_row, col: ColumnAddr::Left(c) }
        } else if c < lm + mc {
            CellAddr::Header { row: hdr_row, col: ColumnAddr::Main((c - lm) as u32) }
        } else {
            CellAddr::Header { row: hdr_row, col: ColumnAddr::Right(c - lm - mc) }
        }
    } else if logical_row < hr + mr {
        let main_row = (logical_row - hr) as u32;
        if c < lm {
            CellAddr::Left { row: main_row, col: c }
        } else if c < lm + mc {
            CellAddr::Main { row: main_row, col: (c - lm) as u32 }
        } else {
            CellAddr::Right { row: main_row, col: c - lm - mc }
        }
    } else {
        let ftr_row = (logical_row - hr - mr) as u32;
        if c < lm {
            CellAddr::Footer { row: ftr_row, col: ColumnAddr::Left(c) }
        } else if c < lm + mc {
            CellAddr::Footer { row: ftr_row, col: ColumnAddr::Main((c - lm) as u32) }
        } else {
            CellAddr::Footer { row: ftr_row, col: ColumnAddr::Right(c - lm - mc) }
        }
    }
}
