#![allow(non_upper_case_globals)]

use rustxwidgets::backends_gtk_adapter::*;
use rustxwidgets::backends::gtk;
use rustxwidgets::core::DrawContext;

use std::collections::HashMap;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::grid::{CellAddr, SheetCursor, HEADER_ROWS, MARGIN_COLS};
use crate::ops::{AggFunc, Op, WorkbookOp};
use crate::ui_core;

use super::compute::{self, CellDisplayStyle};
use super::edit::{KEY_BACKSPACE, KEY_DELETE, KEY_DOWN, KEY_END, KEY_ENTER, KEY_ESC,
    KEY_F1, KEY_F2, KEY_HOME, KEY_LEFT, KEY_PAGE_DOWN, KEY_PAGE_UP, KEY_RETURN,
    KEY_RIGHT, KEY_TAB, KEY_UP};
use super::render::{self, CellSink};

const FONT_SIZE: f64 = 12.0;
const ROW_H: f64 = 20.0;
const HEADER_H: f64 = 24.0;
const ROW_LABEL_W: f64 = 50.0;
const MAX_RENDER_ROWS: usize = 500;
const MAX_RENDER_COLS: usize = 50;
const CHAR_W: f64 = 7.2;

struct GtkCanvasSink {
    cells: RefCell<HashMap<(u32, u32), String>>,
    styles: RefCell<HashMap<(u32, u32), CellDisplayStyle>>,
    raw_values: RefCell<HashMap<(u32, u32), String>>,
    cursor_pos: Cell<Option<(u32, u32)>>,
}

impl GtkCanvasSink {
    fn new() -> Self {
        GtkCanvasSink {
            cells: RefCell::new(HashMap::new()),
            styles: RefCell::new(HashMap::new()),
            raw_values: RefCell::new(HashMap::new()),
            cursor_pos: Cell::new(None),
        }
    }

    fn clear(&self) {
        self.cells.borrow_mut().clear();
        self.styles.borrow_mut().clear();
        self.raw_values.borrow_mut().clear();
        self.cursor_pos.set(None);
    }

    fn render_to(
        &self,
        dc: &mut dyn DrawContext,
        col_ixs: &[usize],
        col_widths: &HashMap<usize, usize>,
        display_rows: &[usize],
        _mr: usize,
        _mc: usize,
        cursor_row: usize,
        cursor_col: usize,
        is_editing: bool,
        edit_text: &str,
        selection_anchor: Option<(usize, usize)>,
    ) {
        let cells = self.cells.borrow();
        let styles = self.styles.borrow();

        for (ri, &logical_row) in display_rows.iter().enumerate().take(MAX_RENDER_ROWS) {
            let ry = HEADER_H + ri as f64 * ROW_H;
            let is_sel_row = selection_anchor.map_or(false, |(ar, ac)| {
                let r1 = ar.min(cursor_row);
                let r2 = ar.max(cursor_row);
                let _c1 = ac.min(cursor_col);
                let _c2 = ac.max(cursor_col);
                logical_row >= r1 && logical_row <= r2
            });

for (ci, &c) in col_ixs.iter().enumerate().take(MAX_RENDER_COLS) {
                let cw = *col_widths.get(&c).unwrap_or(&8) as f64 * CHAR_W;
                let rx = col_x(ci, col_ixs, col_widths);
                let is_cursor = logical_row == cursor_row && c == cursor_col;
                let key = (ri as u32, c as u32);

                // Background
                let style = styles.get(&key).copied().unwrap_or(CellDisplayStyle::Default);
                if is_editing && is_cursor {
                    dc.fill_rect(rx, ry, cw, ROW_H, 0.8, 1.0, 0.8, 0.3);
                } else if is_cursor {
                    dc.fill_rect(rx, ry, cw, ROW_H, 0.2, 0.5, 1.0, 0.15);
                } else if is_sel_row && selection_anchor.is_some() {
                    let c1 = selection_anchor.unwrap().1.min(cursor_col);
                    let c2 = selection_anchor.unwrap().1.max(cursor_col);
                    if c >= c1 && c <= c2 {
                        dc.fill_rect(rx, ry, cw, ROW_H, 0.2, 0.5, 1.0, 0.08);
                    }
                } else if matches!(style, CellDisplayStyle::Aggregate | CellDisplayStyle::FooterAggregate) {
                    dc.fill_rect(rx, ry, cw, ROW_H, 0.95, 0.95, 0.99, 1.0);
                }

                // Text
                if is_editing && is_cursor {
                    dc.stroke_rect(rx, ry, cw, ROW_H, 0.0, 0.7, 0.0, 1.0, 2.0);
                    dc.draw_text(rx + 3.0, ry + ROW_H - 5.0, edit_text, "monospace", FONT_SIZE, 0.0, 0.0, 0.0, 1.0);
                } else if is_cursor {
                    dc.stroke_rect(rx, ry, cw, ROW_H, 0.2, 0.5, 1.0, 1.0, 1.5);
                    if let Some(text) = cells.get(&key) {
                        if !text.trim().is_empty() {
                            dc.draw_text(rx + 3.0, ry + ROW_H - 5.0, text, "monospace", FONT_SIZE, 0.0, 0.0, 0.0, 1.0);
                        }
                    }
                } else if matches!(style, CellDisplayStyle::Aggregate | CellDisplayStyle::FooterAggregate) {
                    if let Some(text) = cells.get(&key) {
                        if !text.trim().is_empty() {
                            dc.draw_text(rx + 3.0, ry + ROW_H - 5.0, text, "monospace", FONT_SIZE, 0.0, 0.0, 0.5, 1.0);
                        }
                    }
                } else if matches!(style, CellDisplayStyle::ActiveHeader) {
                    dc.fill_rect(rx, ry, cw, ROW_H, 0.85, 0.85, 0.95, 1.0);
                    if let Some(text) = cells.get(&key) {
                        if !text.trim().is_empty() {
                            dc.draw_text(rx + 3.0, ry + ROW_H - 5.0, text, "monospace", FONT_SIZE, 0.0, 0.0, 0.0, 1.0);
                        }
                    }
                } else if matches!(style, CellDisplayStyle::InactiveHeader) {
                    dc.fill_rect(rx, ry, cw, ROW_H, 0.92, 0.92, 0.92, 1.0);
                    if let Some(text) = cells.get(&key) {
                        if !text.trim().is_empty() {
                            dc.draw_text(rx + 3.0, ry + ROW_H - 5.0, text, "monospace", FONT_SIZE, 0.5, 0.5, 0.5, 1.0);
                        }
                    }
                } else if matches!(style, CellDisplayStyle::Selected) {
                    dc.fill_rect(rx, ry, cw, ROW_H, 0.3, 0.6, 0.3, 0.2);
                    if let Some(text) = cells.get(&key) {
                        if !text.trim().is_empty() {
                            dc.draw_text(rx + 3.0, ry + ROW_H - 5.0, text, "monospace", FONT_SIZE, 0.0, 0.0, 0.0, 1.0);
                        }
                    }
                } else if let Some(text) = cells.get(&key) {
                    if !text.trim().is_empty() {
                        dc.draw_text(rx + 3.0, ry + ROW_H - 5.0, text, "monospace", FONT_SIZE, 0.0, 0.0, 0.0, 1.0);
                    }
                }

            }
        }
    }
}

impl CellSink for GtkCanvasSink {
    fn set_cell(&mut self, row: u32, col: u32, text: &str) {
        self.cells.borrow_mut().insert((row, col), text.to_string());
    }
    fn set_cell_style(&mut self, row: u32, col: u32, style: CellDisplayStyle) {
        self.styles.borrow_mut().insert((row, col), style);
    }
    fn set_raw_cell(&mut self, row: u32, col: u32, text: &str) {
        self.raw_values.borrow_mut().insert((row, col), text.to_string());
    }
    fn set_cursor(&mut self, row: u32, col: u32) {
        self.cursor_pos.set(Some((row, col)));
    }
}

fn col_x(col_ix: usize, col_ixs: &[usize], col_widths: &HashMap<usize, usize>) -> f64 {
    let mut x = ROW_LABEL_W;
    for ci in 0..col_ix {
        let c = col_ixs[ci];
        let cw = *col_widths.get(&c).unwrap_or(&8) as f64 * CHAR_W;
        x += cw;
    }
    x
}

fn col_pixel_width(c: usize, col_widths: &HashMap<usize, usize>) -> f64 {
    *col_widths.get(&c).unwrap_or(&8) as f64 * CHAR_W
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GtkMode {
    Normal,
    Editing,
    Help,
    About,
}

struct GtkCanvasState {
    app: *mut super::App,
    formula_entry: Entry,
    addr_label: Label,
    status_label: Label,
    canvas: Canvas,
    display_rows: RefCell<Vec<usize>>,
    col_ixs: RefCell<Vec<usize>>,
    col_widths: RefCell<HashMap<usize, usize>>,
    scroll_row: Cell<u32>,
    scroll_col: Cell<u32>,
    edit_buf: RefCell<String>,
    editing: Cell<bool>,
    last_row: Cell<usize>,
    last_col: Cell<usize>,
    mode: Cell<GtkMode>,
    data_width: Cell<usize>,
    data_rows: Cell<usize>,
    data_cols: Cell<usize>,
    row_agg_func: RefCell<Vec<Option<AggFunc>>>,
    sink: GtkCanvasSink,
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
    // Make formula entry read-only so clicking it doesn't steal keyboard focus
    if let Some(loader) = gtk::loader() {
        if let Some(set_editable) = loader.symbols.gtk_entry_set_editable {
            unsafe { set_editable(*formula_entry.0.as_ref(), 0); }
        }
    }
    vbox.append(&formula_bar);

    // Compute viewport dimensions (before create_spreadsheet, which uses them for size_request)
    let data_width = 200usize;
    let data_rows = 30usize;
    let data_cols = 12usize;

    let spreadsheet = create_spreadsheet(data_rows, data_cols)?;

    // Fit column widths to rendered content
    app.fit_main_columns_to_max_width();

    let hr = HEADER_ROWS;
    let lm = MARGIN_COLS;
    let cursor_row = hr;
    let cursor_col = lm;
    app.core.cursor.row = cursor_row;
    app.core.cursor.col = cursor_col;
    app.core.anchor = Some(SheetCursor { row: hr, col: lm });

    let sheet_rec = app.core.workbook.active_sheet().clone();
    let cursor = SheetCursor { row: cursor_row, col: cursor_col };
    let (display_rows, _) = ui_core::visible_row_indices(&sheet_rec, cursor, data_rows, 0);
    let (col_ixs, _) = ui_core::visible_col_indices(&sheet_rec, cursor, data_cols, 0);

    let col_widths: HashMap<usize, usize> = col_ixs.iter()
        .map(|&c| (c, sheet_rec.grid.col_width(c).max(1)))
        .collect();

    let row_agg_func = compute::compute_row_agg_func(
        &sheet_rec.grid, &display_rows, hr, sheet_rec.grid.main_rows(),
    );

    let state = GtkCanvasState {
        app,
        formula_entry: formula_entry.clone(),
        addr_label: addr_label.clone(),
        status_label: create_label("Ready")?,
        canvas: spreadsheet.canvas().clone(),
        display_rows: RefCell::new(display_rows),
        col_ixs: RefCell::new(col_ixs),
        col_widths: RefCell::new(col_widths),
        scroll_row: Cell::new(0),
        scroll_col: Cell::new(0),
        edit_buf: RefCell::new(String::new()),
        editing: Cell::new(false),
        last_row: Cell::new(cursor_row),
        last_col: Cell::new(cursor_col),
        mode: Cell::new(GtkMode::Normal),
        data_width: Cell::new(data_width),
        data_rows: Cell::new(data_rows),
        data_cols: Cell::new(data_cols),
        row_agg_func: RefCell::new(row_agg_func),
        sink: GtkCanvasSink::new(),
    };

    let state = Rc::new(state);
    update_formula_bar(&state, cursor_row, cursor_col);

    // Draw callback
    let state_draw = state.clone();
    spreadsheet.set_draw_callback(Box::new(move |dc: &mut dyn DrawContext, w: i32, h: i32| {
        render_grid(dc, &state_draw, w, h);
    }));

    // Key handler — attach to the canvas DrawingArea (the adapter stores the
    // controller to keep it alive).  For GTK4 the DrawingArea is NOT focusable,
    // so we also attach an EventControllerKey to the formula entry (which does
    // accept focus) so keyboard events are always captured.
    let state_key = state.clone();
    spreadsheet.on_key(Box::new(move |keyval: u32, key_state: u32| -> bool {
        handle_key(keyval, key_state, &state_key)
    }));
    // Capture keys from the formula entry too (it can still receive focus even
    // when read-only, and in GTK4 the DrawingArea is not focusable).
    let state_key2 = state.clone();
    let entry_controllers: Rc<RefCell<Vec<Box<dyn std::any::Any>>>> = Rc::new(RefCell::new(Vec::new()));
    if let Some(loader) = gtk::loader() {
        if loader.symbols.gtk_gesture_click_new.is_some() {
            // GTK4: use EventControllerKey
            if let Ok(ctrl) = gtk_dynamic_loader::EventControllerKey::new(loader.clone()) {
                let state_k = state_key2.clone();
                let _ = ctrl.connect_key_pressed(Box::new(move |keyval: u32, key_state: u32| -> i32 {
                    if handle_key(keyval, key_state, &state_k) { 1 } else { 0 }
                }));
                ctrl.add_to_widget(&formula_entry);
                entry_controllers.borrow_mut().push(Box::new(ctrl));
            }
        } else {
            // GTK3: connect key-press-event signal directly
            let entry_ptr = *formula_entry.0.as_ref();
            let l2 = loader.clone();
            let l3 = l2.clone();
            let mut cb = Box::new(move |keyval: u32, key_state: u32| -> bool {
                handle_key(keyval, key_state, &state_key2)
            });
            unsafe {
                let _ = gtk_dynamic_loader::widget_connect_signal_bool(
                    &l2, entry_ptr, "key-press-event",
                    Box::new(move |ev: *mut std::ffi::c_void| -> i32 {
                        let keyval = gtk_dynamic_loader::EventControllerKey::get_keyval_static(&l3, ev);
                        if cb(keyval, 0) { 1 } else { 0 }
                    }),
                );
            }
        }
    }

    // Click handler
    let state_click = state.clone();
    spreadsheet.on_click(Box::new(move |x: f64, y: f64| {
        handle_click(x, y, &state_click);
    }));

    let status_bar = state.status_label.clone();
    vbox.append(&spreadsheet);
    vbox.append(&status_bar);
    win.set_child(&vbox);
    win.present();

    _backend.run().map_err(|e| format!("GUI error: {e}"))?;
    Ok(())
}

// ── Rendering ────────────────────────────────────────────────────────────────

fn update_viewport_dims(state: &GtkCanvasState, w: i32, h: i32) -> bool {
    let new_width = ((w as f64 - ROW_LABEL_W - 2.0) / CHAR_W).max(1.0) as usize;
    let new_rows = ((h as f64 - HEADER_H) / ROW_H).max(1.0) as usize;
    let new_cols = new_width.checked_div(2).unwrap_or(1).max(1);
    let changed = new_width != state.data_width.get()
        || new_rows != state.data_rows.get()
        || new_cols != state.data_cols.get();
    if changed {
        state.data_width.set(new_width);
        state.data_rows.set(new_rows);
        state.data_cols.set(new_cols);
    }
    changed
}

fn render_grid(dc: &mut dyn DrawContext, state: &GtkCanvasState, w: i32, h: i32) {
    dc.clear(1.0, 1.0, 1.0, 1.0);

    let app = unsafe { &*state.app };
    let sheet = app.core.workbook.active_sheet();
    let g = &sheet.grid;
    let hr = HEADER_ROWS;
    let mr = g.main_rows();
    let mc = g.main_cols();
    let lm = MARGIN_COLS;
    let cursor_row = state.last_row.get();
    let cursor_col = state.last_col.get();

    // Update viewport dimensions from canvas size
    if update_viewport_dims(state, w, h) {
        drop(state.display_rows.borrow());
        drop(state.col_ixs.borrow());
        drop(state.col_widths.borrow());
        drop(state.row_agg_func.borrow());
        let app_mut = unsafe { &mut *state.app };
        let rec = app_mut.core.workbook.active_sheet().clone();
        let cursor = SheetCursor { row: cursor_row, col: cursor_col };
        let (new_display_rows, _) = ui_core::visible_row_indices(
            &rec, cursor, state.data_rows.get(), 0);
        let (mut new_col_ixs, _) = ui_core::visible_col_indices(
            &rec, cursor, state.data_cols.get(), 0);
        {
            let sht = app_mut.core.workbook.active_sheet_mut();
            ui_core::trim_visible_cols_to_width(
                &mut sht.grid, &mut new_col_ixs, cursor.col, state.data_width.get());
        }
        let rec = app_mut.core.workbook.active_sheet().clone();
        let new_col_widths: HashMap<usize, usize> = new_col_ixs.iter()
            .map(|&c| (c, rec.grid.col_width(c).max(1)))
            .collect();
        let new_row_agg = compute::compute_row_agg_func(
            &rec.grid, &new_display_rows, hr, rec.grid.main_rows());
        *state.display_rows.borrow_mut() = new_display_rows;
        *state.col_ixs.borrow_mut() = new_col_ixs;
        *state.col_widths.borrow_mut() = new_col_widths;
        *state.row_agg_func.borrow_mut() = new_row_agg;
    }

    let display_rows = state.display_rows.borrow();
    let col_ixs = state.col_ixs.borrow();
    let col_widths = state.col_widths.borrow();
    let row_agg_func = state.row_agg_func.borrow();

    dc.save();

    // Draw column headers
    for (ci, &c) in col_ixs.iter().enumerate() {
        let rx = col_x(ci, &col_ixs, &col_widths);
        let cw = col_pixel_width(c, &col_widths);
        let label = crate::addr::ui_column_fragment(c, mc);
        dc.fill_rect(rx, 0.0, cw, HEADER_H, 0.92, 0.92, 0.92, 1.0);
        let te = dc.text_extents(&label, "monospace", FONT_SIZE);
        if te.2 > 0.0 {
            dc.draw_text(rx + (cw - te.2) / 2.0, HEADER_H - 6.0,
                &label, "monospace", FONT_SIZE, 0.2, 0.2, 0.2, 1.0);
        }
    }

    // Draw row headers
    for (ri, &r) in display_rows.iter().enumerate() {
        let ry = HEADER_H + ri as f64 * ROW_H;
        let label = crate::addr::ui_row_label(r, mr);
        dc.fill_rect(0.0, ry, ROW_LABEL_W, ROW_H, 0.92, 0.92, 0.92, 1.0);
        let te = dc.text_extents(&label, "monospace", FONT_SIZE);
        if te.2 > 0.0 {
            dc.draw_text(ROW_LABEL_W - te.2 - 5.0, ry + ROW_H - 5.0,
                &label, "monospace", FONT_SIZE, 0.2, 0.2, 0.2, 1.0);
        }
    }

    // Draw cells via fill_cells
    let data_width = state.data_width.get();
    {
        let mut sink = GtkCanvasSink::new();
        render::fill_cells(
            &mut sink, &display_rows, &col_ixs, &col_widths, g,
            hr, mr, mc, lm, data_width, cursor_row, cursor_col, &row_agg_func,
        );
        let is_editing = state.editing.get();
        let edit_text = state.edit_buf.borrow();
        let anchor = app.core.anchor.map(|a| (a.row, a.col));
        sink.render_to(dc, &col_ixs, &col_widths, &display_rows, mr, mc,
            cursor_row, cursor_col, is_editing, &edit_text, anchor);
    }

    // Grid lines (batched, replaces per-cell stroke_rect)
    let total_w: f64 = col_ixs.iter().map(|&c| col_pixel_width(c, &col_widths)).sum();
    let total_h = display_rows.len() as f64 * ROW_H;
    // Horizontal lines between rows
    let mut y = HEADER_H;
    for _ in 0..=display_rows.len() {
        dc.fill_rect(ROW_LABEL_W, y, total_w, 0.5, 0.9, 0.9, 0.9, 1.0);
        y += ROW_H;
    }
    // Vertical lines between columns
    let mut x = ROW_LABEL_W;
    for &c in col_ixs.iter() {
        let cw = col_pixel_width(c, &col_widths);
        dc.fill_rect(x, HEADER_H, 0.5, total_h, 0.9, 0.9, 0.9, 1.0);
        x += cw;
    }
    // Rightmost vertical line
    dc.fill_rect(x, HEADER_H, 0.5, total_h, 0.9, 0.9, 0.9, 1.0);

    // Border title
    let border_title = format!("corro  {}r × {}c  ops {}", mr, mc, app.core.ops_applied);
    let te = dc.text_extents(&border_title, "monospace", 10.0);
    if te.2 > 0.0 {
        let hdr_right = ROW_LABEL_W + total_w;
        dc.draw_text(hdr_right - te.2 - 4.0, HEADER_H + 2.0,
            &border_title, "monospace", 10.0, 0.4, 0.4, 0.4, 1.0);
    }

    dc.restore();
}

fn recompute_viewport(state: &GtkCanvasState) {
    let app = unsafe { &mut *state.app };
    let cursor = SheetCursor {
        row: state.last_row.get(),
        col: state.last_col.get(),
    };

    let _sheet = app.core.workbook.active_sheet_mut();

    let data_rows = state.data_rows.get();
    let data_cols = state.data_cols.get();
    let data_width = state.data_width.get();
    let hr = HEADER_ROWS;

    // Recompute visible rows and columns
    let rec = app.core.workbook.active_sheet().clone();
    let (new_display_rows, new_scroll) = ui_core::visible_row_indices(&rec, cursor, data_rows, 0);
    let (mut new_col_ixs, _) = ui_core::visible_col_indices(&rec, cursor, data_cols, 0);
    {
        let sht = app.core.workbook.active_sheet_mut();
        ui_core::trim_visible_cols_to_width(&mut sht.grid, &mut new_col_ixs, cursor.col, data_width);
    }
    let rec = app.core.workbook.active_sheet().clone();
    let new_col_widths: HashMap<usize, usize> = new_col_ixs.iter()
        .map(|&c| (c, rec.grid.col_width(c).max(1)))
        .collect();
    let new_row_agg = compute::compute_row_agg_func(&rec.grid, &new_display_rows, hr, rec.grid.main_rows());

    *state.display_rows.borrow_mut() = new_display_rows;
    *state.col_ixs.borrow_mut() = new_col_ixs;
    *state.col_widths.borrow_mut() = new_col_widths;
    *state.row_agg_func.borrow_mut() = new_row_agg;
    state.scroll_row.set(new_scroll as u32);
}

// ── Click handling ───────────────────────────────────────────────────────────

fn handle_click(x: f64, y: f64, state: &GtkCanvasState) {
    // Find which column was clicked (borrows col_ixs/col_widths, dropped before update_state_cursor)
    let click_col_ix = {
        let col_ixs = state.col_ixs.borrow();
        let col_widths = state.col_widths.borrow();
        let mut click_col_ix = None;
        let mut accumulated = ROW_LABEL_W;
        for (ci, &c) in col_ixs.iter().enumerate() {
            let cw = col_pixel_width(c, &col_widths);
            if x >= accumulated && x < accumulated + cw {
                click_col_ix = Some((ci, c));
                break;
            }
            accumulated += cw;
        }
        click_col_ix
    };

    let click_row_ix = if y >= HEADER_H && y < HEADER_H + display_rows_len(state) as f64 * ROW_H {
        Some(((y - HEADER_H) / ROW_H) as usize)
    } else {
        None
    };

    if let (Some((_ci, c)), Some(ri)) = (click_col_ix, click_row_ix) {
        let logical_row = state.display_rows.borrow().get(ri).copied();
        if let Some(logical_row) = logical_row {
            state.last_col.set(c);
            state.last_row.set(logical_row);
            update_state_cursor(state, logical_row, c);
            state.canvas.queue_redraw();
        }
    }
}

fn display_rows_len(state: &GtkCanvasState) -> usize {
    state.display_rows.borrow().len()
}

// ── Edit helpers ─────────────────────────────────────────────────────────────

fn commit_edit(state: &GtkCanvasState) {
    let app = unsafe { &mut *state.app };
    let text = state.edit_buf.borrow().clone();
    let row = state.last_row.get();
    let col = state.last_col.get();
    let main_row = row.saturating_sub(HEADER_ROWS);
    let main_col = col.saturating_sub(MARGIN_COLS);
    let addr = CellAddr::Main { row: main_row as u32, col: main_col as u32 };
    {
        let sheet = app.core.workbook.active_sheet_mut();
        sheet.grid.set(&addr, text.clone());
    }
    let sheet_id = app.core.workbook.sheet_id(app.core.workbook.active_sheet);
    let op = Op::SetCell { addr, value: text };
    let wbo = WorkbookOp::SheetOp { sheet_id, op };
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
    state.mode.set(GtkMode::Normal);
}

fn start_edit(state: &GtkCanvasState) {
    let app = unsafe { &mut *state.app };
    let row = state.last_row.get();
    let col = state.last_col.get();
    let main_row = row.saturating_sub(HEADER_ROWS);
    let main_col = col.saturating_sub(MARGIN_COLS);
    let addr = CellAddr::Main { row: main_row as u32, col: main_col as u32 };
    let sheet = app.core.workbook.active_sheet();
    let val = sheet.grid.get(&addr).unwrap_or_default();
    *state.edit_buf.borrow_mut() = val;
    state.editing.set(true);
    state.mode.set(GtkMode::Editing);
    state.formula_entry.set_text("");
    state.canvas.queue_redraw();
}

fn start_edit_with(state: &GtkCanvasState, ch: char) {
    start_edit(state);
    state.edit_buf.borrow_mut().push(ch);
    state.canvas.queue_redraw();
}

fn move_cursor(state: &GtkCanvasState, dr: isize, dc: isize) {
    let cur_row = state.last_row.get() as isize;
    let cur_col = state.last_col.get() as isize;
    let new_row = (cur_row + dr).max(HEADER_ROWS as isize) as usize;
    let new_col = (cur_col + dc).max(0isize) as usize;
    state.last_row.set(new_row);
    state.last_col.set(new_col);
    update_state_cursor(state, new_row, new_col);
}

fn update_row_col_from_state(state: &GtkCanvasState) {
    let app = unsafe { &mut *state.app };
    let row = state.last_row.get();
    let col = state.last_col.get();
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
}

fn update_state_cursor(state: &GtkCanvasState, row: usize, col: usize) {
    update_row_col_from_state(state);
    let app = unsafe { &mut *state.app };
    let sheet = app.core.workbook.active_sheet();

    let needs_recompute = {
        let dr = state.display_rows.borrow();
        let cix = state.col_ixs.borrow();
        !dr.contains(&row) || !cix.contains(&col)
    };

    if needs_recompute {
        recompute_viewport(state);
    } else {
        *state.col_widths.borrow_mut() = state.col_ixs.borrow().iter()
            .map(|&c| (c, sheet.grid.col_width(c).max(1)))
            .collect();
        *state.row_agg_func.borrow_mut() = compute::compute_row_agg_func(
            &sheet.grid, &state.display_rows.borrow(), HEADER_ROWS, sheet.grid.main_rows(),
        );
    }

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
        let status_text = format!("   ·  {}", app.core.status);
        state.formula_entry.set_text(&format!("{} {}", val, status_text));
        state.status_label.set_text(&app.core.status);
    } else {
        state.status_label.set_text("Ready");
    }
    // Restore the actual cell value (not the status text) in the display
    state.formula_entry.set_text(&val);
}

// ── Keyboard Handling ────────────────────────────────────────────────────────

const GDK_CONTROL_MASK: u32 = 1 << 2;
const GDK_SHIFT_MASK: u32 = 1 << 0;
const GDK_MOD1_MASK: u32 = 1 << 3;

fn handle_key(keyval: u32, key_state: u32, state_rc: &Rc<GtkCanvasState>) -> bool {
    let state: &GtkCanvasState = &**state_rc;
    let app = unsafe { &mut *state.app };
    let mode = state.mode.get();
    let ctrl = (key_state & GDK_CONTROL_MASK) != 0;
    let _shift = (key_state & GDK_SHIFT_MASK) != 0;
    let _alt = (key_state & GDK_MOD1_MASK) != 0;

    // Help/About mode
    match mode {
        GtkMode::Help | GtkMode::About => {
            match keyval {
                KEY_ESC => {
                    state.mode.set(GtkMode::Normal);
                    state.canvas.queue_redraw();
                    return true;
                }
                _ => return true,
            }
        }
        _ => {}
    }

    // Editing mode
    if mode == GtkMode::Editing {
        return handle_edit_key(keyval, state);
    }

    // Ctrl+ shortcuts (checked before normal dispatch)
    if ctrl {
        // Ctrl+letter sends lowercase keyval (0x61-0x7A)
        let handled = match keyval {
            0x63 /* c */ => { handle_copy(state); true }
            0x78 /* x */ => { handle_cut(state); true }
            0x76 /* v */ => { handle_paste(state); true }
            0x7A /* z */ => { handle_undo(state); true }
            0x79 /* y */ => { handle_redo(state); true }
            0x6F /* o */ => { handle_open(state); true }
            0x73 /* s */ => { handle_save(state); true }
            0x66 /* f */ => { handle_find(state); true }
            0x68 /* h */ => { handle_replace(state); true }
            0x71 /* q */ => { let _ = rustxwidgets::backends_gtk_adapter::quit_main_loop(); true }
            0x61 /* a */ => { handle_select_all(state); true }
            0x6E /* n */ => { handle_new_sheet(state_rc); true }
            KEY_PAGE_UP => { handle_prev_sheet(state_rc); true }
            KEY_PAGE_DOWN => { handle_next_sheet(state_rc); true }
            0x77 /* w */ => { handle_delete_sheet(state_rc); true }
            _ => false,
        };
        if handled {
            update_formula_bar(state, state.last_row.get(), state.last_col.get());
            state.canvas.queue_redraw();
            return true;
        }
    }

    // Normal mode
    match keyval {
        KEY_F1 => {
            crate::gui::dialogs::show_keybinds_help();
        }
        KEY_LEFT => move_cursor(state, 0, -1),
        KEY_RIGHT => move_cursor(state, 0, 1),
        KEY_UP => {
            let r = state.last_row.get();
            if r > HEADER_ROWS {
                move_cursor(state, -1, 0);
            }
        }
        KEY_DOWN => move_cursor(state, 1, 0),
        KEY_RETURN | KEY_ENTER => start_edit(state),
        KEY_TAB => move_cursor(state, 0, 1),
        KEY_HOME => {
            state.last_col.set(MARGIN_COLS);
            update_state_cursor(state, state.last_row.get(), state.last_col.get());
        }
        KEY_END => {
            let sheet = app.core.workbook.active_sheet();
            let mc = sheet.grid.main_cols();
            state.last_col.set(MARGIN_COLS + mc - 1);
            update_state_cursor(state, state.last_row.get(), state.last_col.get());
        }
        KEY_PAGE_UP => {
            let p = state.data_rows.get() as usize;
            let cur = state.last_row.get();
            let new = if cur >= p { cur - p } else { HEADER_ROWS };
            state.last_row.set(new);
            update_state_cursor(state, new, state.last_col.get());
        }
        KEY_PAGE_DOWN => {
            let p = state.data_rows.get() as usize;
            let sheet = app.core.workbook.active_sheet();
            let mr = sheet.grid.main_rows();
            let cur = state.last_row.get();
            let new = (cur + p).min(HEADER_ROWS + mr - 1);
            state.last_row.set(new);
            update_state_cursor(state, new, state.last_col.get());
        }
        KEY_DELETE => {
            let row = state.last_row.get();
            let col = state.last_col.get();
            let sheet = app.core.workbook.active_sheet_mut();
            let main_row = row.saturating_sub(HEADER_ROWS);
            let main_col = col.saturating_sub(MARGIN_COLS);
            let addr = CellAddr::Main { row: main_row as u32, col: main_col as u32 };
            sheet.grid.set(&addr, String::new());
            state.formula_entry.set_text("");
            state.canvas.queue_redraw();
        }
        KEY_F2 => start_edit(state),
        // Printable chars start editing (skip Ctrl chars 0-31)
        _ if (32..=126).contains(&keyval) => {
            start_edit_with(state, char::from_u32(keyval).unwrap_or('?'));
        }
        _ => return false,
    }

    if !ctrl {
        update_formula_bar(state, state.last_row.get(), state.last_col.get());
        state.canvas.queue_redraw();
    }
    true
}

// ── Clipboard ────────────────────────────────────────────────────────────────

fn clipboard_copy_text(text: &str) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        use std::process::{Command, Stdio};
        let mut child = Command::new("xclip")
            .args(["-selection", "clipboard"])
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| format!("xclip not found: {e}"))?;
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes()).map_err(|e| format!("clipboard write error: {e}"))?;
        }
        child.wait().map_err(|e| format!("xclip wait error: {e}"))?;
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        use std::process::{Command, Stdio};
        use std::io::Write;
        let mut child = Command::new("pbcopy")
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| format!("pbcopy not found: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes()).map_err(|e| format!("clipboard write error: {e}"))?;
        }
        child.wait().map_err(|e| format!("pbcopy wait error: {e}"))?;
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        use std::process::{Command, Stdio};
        use std::io::Write;
        let mut child = Command::new("clip")
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| format!("clip not found: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes()).map_err(|e| format!("clipboard write error: {e}"))?;
        }
        child.wait().map_err(|e| format!("clip wait error: {e}"))?;
        return Ok(());
    }
    #[allow(unreachable_code)]
    Err("clipboard not supported on this platform".into())
}

fn clipboard_read_text() -> Result<String, String> {
    #[cfg(target_os = "linux")]
    {
        use std::process::Command;
        let output = Command::new("xclip")
            .args(["-o", "-selection", "clipboard"])
            .output()
            .map_err(|e| format!("xclip not found: {e}"))?;
        if output.status.success() {
            return String::from_utf8(output.stdout).map_err(|e| format!("clipboard UTF-8 error: {e}"));
        }
        return Err("xclip returned error".into());
    }
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        let output = Command::new("pbpaste")
            .output()
            .map_err(|e| format!("pbpaste not found: {e}"))?;
        if output.status.success() {
            return String::from_utf8(output.stdout).map_err(|e| format!("clipboard UTF-8 error: {e}"));
        }
        return Err("pbpaste returned error".into());
    }
    #[cfg(target_os = "windows")]
    {
        use std::process::Command;
        let output = Command::new("powershell")
            .args(["-Command", "Get-Clipboard"])
            .output()
            .map_err(|e| format!("powershell not found: {e}"))?;
        if output.status.success() {
            return String::from_utf8(output.stdout).map_err(|e| format!("clipboard UTF-8 error: {e}"));
        }
        return Err("powershell returned error".into());
    }
    #[allow(unreachable_code)]
    Err("clipboard not supported on this platform".into())
}

fn handle_copy(state: &GtkCanvasState) {
    let app = unsafe { &mut *state.app };
    let sheet = app.core.workbook.active_sheet();
    let row = state.last_row.get();
    let col = state.last_col.get();
    let main_row = row.saturating_sub(HEADER_ROWS);
    let main_col = col.saturating_sub(MARGIN_COLS);
    let addr = CellAddr::Main { row: main_row as u32, col: main_col as u32 };
    let val = sheet.grid.get(&addr).unwrap_or_default();
    if !val.is_empty() {
        let _ = clipboard_copy_text(&val);
    }
    app.core.status = "Copied".into();
}

fn handle_cut(state: &GtkCanvasState) {
    handle_copy(state);
    let app = unsafe { &mut *state.app };
    let sheet = app.core.workbook.active_sheet_mut();
    let row = state.last_row.get();
    let col = state.last_col.get();
    let main_row = row.saturating_sub(HEADER_ROWS);
    let main_col = col.saturating_sub(MARGIN_COLS);
    let addr = CellAddr::Main { row: main_row as u32, col: main_col as u32 };
    sheet.grid.set(&addr, String::new());
    state.formula_entry.set_text("");
    app.core.status = "Cut".into();
}

fn handle_paste(state: &GtkCanvasState) {
    let text = match clipboard_read_text() {
        Ok(t) => t,
        Err(e) => {
            let app = unsafe { &mut *state.app };
            app.core.status = format!("Paste error: {e}");
            return;
        }
    };
    let app = unsafe { &mut *state.app };
    let row = state.last_row.get();
    let col = state.last_col.get();
    let main_row = row.saturating_sub(HEADER_ROWS);
    let main_col = col.saturating_sub(MARGIN_COLS);
    let addr = CellAddr::Main { row: main_row as u32, col: main_col as u32 };
    let trimmed = text.trim().to_string();
    {
        let sheet = app.core.workbook.active_sheet_mut();
        sheet.grid.set(&addr, trimmed.clone());
    }
    let sheet_id = app.core.workbook.sheet_id(app.core.workbook.active_sheet);
    let op = Op::SetCell { addr, value: trimmed };
    let wbo = WorkbookOp::SheetOp { sheet_id, op };
    if let Some(ref p) = app.core.path.clone() {
        let mut active_sheet = sheet_id;
        let _ = crate::io::commit_workbook_op(
            p, &mut app.core.offset, &mut app.core.workbook,
            &mut active_sheet, &wbo,
        );
        app.core.ops_applied = app.core.ops_applied.saturating_add(1);
    }
    state.formula_entry.set_text(&text);
    app.core.status = "Pasted".into();
}

// ── Undo / Redo ──────────────────────────────────────────────────────────────

fn handle_undo(state: &GtkCanvasState) {
    let app = unsafe { &mut *state.app };
    if let Some(undo_op) = app.core.op_history.pop() {
        let redo_op = app.core.workbook.active_sheet().reverse_op(&undo_op);
        let sheet_id = app.core.workbook.sheet_id(app.core.workbook.active_sheet);
        let wbo = WorkbookOp::SheetOp { sheet_id, op: undo_op };
        if let Some(ref p) = app.core.path.clone() {
            let mut active_sheet = sheet_id;
            let _ = crate::io::commit_workbook_op(
                p, &mut app.core.offset, &mut app.core.workbook,
                &mut active_sheet, &wbo,
            );
            app.core.ops_applied = app.core.ops_applied.saturating_add(1);
        }
        if let Some(redo_op) = redo_op {
            app.core.redo_history.push(redo_op);
        }
        app.core.status = "Undo applied".into();
    } else {
        app.core.status = "Nothing to undo".into();
    }
    recompute_viewport(state);
}

fn handle_redo(state: &GtkCanvasState) {
    let app = unsafe { &mut *state.app };
    if let Some(redo_op) = app.core.redo_history.pop() {
        let undo_op = app.core.workbook.active_sheet().reverse_op(&redo_op);
        let sheet_id = app.core.workbook.sheet_id(app.core.workbook.active_sheet);
        let wbo = WorkbookOp::SheetOp { sheet_id, op: redo_op };
        if let Some(ref p) = app.core.path.clone() {
            let mut active_sheet = sheet_id;
            let _ = crate::io::commit_workbook_op(
                p, &mut app.core.offset, &mut app.core.workbook,
                &mut active_sheet, &wbo,
            );
            app.core.ops_applied = app.core.ops_applied.saturating_add(1);
        }
        if let Some(undo_op) = undo_op {
            app.core.op_history.push(undo_op);
        }
        app.core.status = "Redo applied".into();
    } else {
        app.core.status = "Nothing to redo".into();
    }
    recompute_viewport(state);
}

// ── Selection ────────────────────────────────────────────────────────────────

fn handle_select_all(state: &GtkCanvasState) {
    let app = unsafe { &mut *state.app };
    let sheet = app.core.workbook.active_sheet();
    let mr = sheet.grid.main_rows();
    let mc = sheet.grid.main_cols();
    if mr > 0 && mc > 0 {
        app.core.anchor = Some(SheetCursor {
            row: HEADER_ROWS,
            col: MARGIN_COLS,
        });
        state.last_row.set(HEADER_ROWS + mr - 1);
        state.last_col.set(MARGIN_COLS + mc - 1);
        update_state_cursor(state, state.last_row.get(), state.last_col.get());
        app.core.status = format!("Selected {}r × {}c", mr, mc);
    }
}

// ── File operations ──────────────────────────────────────────────────────────

fn handle_open(state: &GtkCanvasState) {
    if let Some(path) = crate::gui::dialogs::file_open_dialog() {
        let app = unsafe { &mut *state.app };
        app.core.path = Some(path.clone());
        app.core.status = format!("Opening: {}", path.display());
        // Reload would need to reinitialize the workbook
        eprintln!("Open: {:?}", path);
    }
}

fn handle_save(state: &GtkCanvasState) {
    let app = unsafe { &mut *state.app };
    if let Some(ref path) = app.core.path.clone() {
        app.core.status = format!("Saving: {}", path.display());
        eprintln!("Save: {:?}", path);
    } else {
        if let Some(path) = crate::gui::dialogs::file_save_dialog() {
            app.core.path = Some(path.clone());
            app.core.status = format!("Saved: {}", path.display());
            eprintln!("Save as: {:?}", path);
        }
    }
}

// ── Find / Replace ───────────────────────────────────────────────────────────

fn handle_find(_state: &GtkCanvasState) {
    crate::gui::dialogs::find_dialog(|result| {
        if let Some(text) = result {
            // TODO: implement find logic
            eprintln!("Find: {}", text);
        }
    });
}

fn handle_replace(_state: &GtkCanvasState) {
    crate::gui::dialogs::replace_dialog(|result| {
        if let Some((find, replace)) = result {
            // TODO: implement replace logic
            eprintln!("Replace: {} with {}", find, replace);
        }
    });
}

fn handle_edit_key(keyval: u32, state: &GtkCanvasState) -> bool {
    match keyval {
        KEY_RETURN | KEY_ENTER => {
            commit_edit(state);
            move_cursor(state, 1, 0);
            return true;
        }
        KEY_ESC => {
            state.editing.set(false);
            state.edit_buf.borrow_mut().clear();
            state.mode.set(GtkMode::Normal);
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
            commit_edit(state);
            let c = state.last_col.get();
            if c > 0 {
                state.last_col.set(c - 1);
                update_state_cursor(state, state.last_row.get(), state.last_col.get());
            }
            return true;
        }
        KEY_RIGHT => {
            commit_edit(state);
            state.last_col.set(state.last_col.get() + 1);
            update_state_cursor(state, state.last_row.get(), state.last_col.get());
            return true;
        }
        KEY_UP => {
            commit_edit(state);
            let r = state.last_row.get();
            if r > HEADER_ROWS {
                state.last_row.set(r - 1);
                update_state_cursor(state, state.last_row.get(), state.last_col.get());
            }
            return true;
        }
        KEY_DOWN => {
            commit_edit(state);
            state.last_row.set(state.last_row.get() + 1);
            update_state_cursor(state, state.last_row.get(), state.last_col.get());
            return true;
        }
        _ if (32..=126).contains(&keyval) => {
            state.edit_buf.borrow_mut().push(char::from_u32(keyval).unwrap_or('?'));
            state.formula_entry.set_text(&state.edit_buf.borrow());
            state.canvas.queue_redraw();
            true
        }
        _ => false,
    }
}

// ── Sheet Management ─────────────────────────────────────────────────────────

fn clamp_cursor_to_sheet(state: &GtkCanvasState) {
    let app = unsafe { &mut *state.app };
    let sheet = app.core.workbook.active_sheet();
    let mr = sheet.grid.main_rows();
    let mc = sheet.grid.main_cols();
    let max_row = HEADER_ROWS + mr.saturating_sub(1);
    let max_col = MARGIN_COLS + mc.saturating_sub(1);
    let cur_row = state.last_row.get();
    let cur_col = state.last_col.get();
    if cur_row > max_row || cur_col > max_col || cur_row < HEADER_ROWS || cur_col < MARGIN_COLS {
        let new_row = cur_row.min(max_row).max(HEADER_ROWS);
        let new_col = cur_col.min(max_col).max(MARGIN_COLS);
        state.last_row.set(new_row);
        state.last_col.set(new_col);
        app.core.cursor.row = new_row;
        app.core.cursor.col = new_col;
    }
}

fn handle_activate_sheet_id(state: &GtkCanvasState, sheet_id: u32) {
    let app = unsafe { &mut *state.app };
    let op = WorkbookOp::ActivateSheet { id: sheet_id };
    if let Err(e) = crate::ops::apply_workbook_op(
        &mut app.core.workbook, &mut 0, op.clone(),
    ) {
        app.core.status = format!("Sheet switch error: {e}");
        return;
    }
    if let Some(ref p) = app.core.path.clone() {
        let mut _active = 0;
        let _ = crate::io::commit_workbook_op(
            p, &mut app.core.offset, &mut app.core.workbook,
            &mut _active, &op,
        );
    }
    clamp_cursor_to_sheet(state);
    recompute_viewport(state);
    update_formula_bar(state, state.last_row.get(), state.last_col.get());
    app.core.status = format!("Sheet: {}", app.core.workbook.sheet_title(app.core.workbook.active_sheet));
    state.canvas.queue_redraw();
}

fn handle_new_sheet(state: &GtkCanvasState) {
    let app = unsafe { &mut *state.app };
    let new_id = app.core.workbook.next_sheet_id;
    let title = format!("Sheet{}", new_id);

    let op = WorkbookOp::NewSheet { id: new_id, title: title.clone() };
    if let Err(e) = crate::ops::apply_workbook_op(
        &mut app.core.workbook, &mut 0, op.clone(),
    ) {
        app.core.status = format!("New sheet error: {e}");
        return;
    }
    if let Some(ref p) = app.core.path.clone() {
        let mut _active = 0;
        let _ = crate::io::commit_workbook_op(
            p, &mut app.core.offset, &mut app.core.workbook,
            &mut _active, &op,
        );
    }

    let activate = WorkbookOp::ActivateSheet { id: new_id };
    if let Err(e) = crate::ops::apply_workbook_op(
        &mut app.core.workbook, &mut 0, activate.clone(),
    ) {
        app.core.status = format!("Activate sheet error: {e}");
        return;
    }
    if let Some(ref p) = app.core.path.clone() {
        let mut _active = 0;
        let _ = crate::io::commit_workbook_op(
            p, &mut app.core.offset, &mut app.core.workbook,
            &mut _active, &activate,
        );
    }

    state.last_row.set(HEADER_ROWS);
    state.last_col.set(MARGIN_COLS);
    app.core.cursor.row = HEADER_ROWS;
    app.core.cursor.col = MARGIN_COLS;
    app.core.anchor = Some(SheetCursor { row: HEADER_ROWS, col: MARGIN_COLS });
    recompute_viewport(state);
    update_formula_bar(state, state.last_row.get(), state.last_col.get());
    app.core.status = format!("New sheet: {}", title);
    state.canvas.queue_redraw();
}

fn handle_delete_sheet(state: &GtkCanvasState) {
    let app = unsafe { &mut *state.app };
    let wb = &mut app.core.workbook;
    if wb.sheet_count() <= 1 {
        app.core.status = "Cannot delete the last sheet".into();
        return;
    }
    let idx = wb.active_sheet;
    let id = wb.sheet_id(idx);
    let title = wb.sheet_title(idx).to_string();
    let op = WorkbookOp::DeleteSheet { id };
    if let Err(e) = crate::ops::apply_workbook_op(wb, &mut 0, op.clone()) {
        app.core.status = format!("Delete sheet error: {e}");
        return;
    }
    if let Some(ref p) = app.core.path.clone() {
        let mut _active = 0;
        let _ = crate::io::commit_workbook_op(
            p, &mut app.core.offset, &mut app.core.workbook,
            &mut _active, &op,
        );
    }
    clamp_cursor_to_sheet(state);
    recompute_viewport(state);
    update_formula_bar(state, state.last_row.get(), state.last_col.get());
    app.core.status = format!("Deleted sheet: {}", title);
    state.canvas.queue_redraw();
}

fn handle_next_sheet(state: &GtkCanvasState) {
    let app = unsafe { &mut *state.app };
    let wb = &app.core.workbook;
    let count = wb.sheet_count();
    if count <= 1 { return; }
    let next = (wb.active_sheet + 1) % count;
    let id = wb.sheet_id(next);
    let _ = wb;
    handle_activate_sheet_id(state, id);
}

fn handle_prev_sheet(state: &GtkCanvasState) {
    let app = unsafe { &mut *state.app };
    let wb = &app.core.workbook;
    let count = wb.sheet_count();
    if count <= 1 { return; }
    let prev = (wb.active_sheet + count - 1) % count;
    let id = wb.sheet_id(prev);
    let _ = wb;
    handle_activate_sheet_id(state, id);
}
