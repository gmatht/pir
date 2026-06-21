#![allow(non_upper_case_globals)]

use rustxwidgets::backends_nwg_adapter::*;
use rustxwidgets::core::DrawContext;
use rustxwidgets::backends_nwg_adapter::Orientation;

use std::collections::HashMap;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::os::raw::c_void;

use crate::grid::{CellAddr, SheetCursor, HEADER_ROWS, MARGIN_COLS};
use crate::ops::{AggFunc, Op, WorkbookOp};
use crate::ui_core;

use super::compute::{self, CellDisplayStyle};
use super::render::{self, CellSink};



const FONT_SIZE: f64 = 12.0;
const ROW_H: f64 = 20.0;
const HEADER_H: f64 = 24.0;
const ROW_LABEL_W: f64 = 50.0;
const MAX_RENDER_ROWS: usize = 500;
const MAX_RENDER_COLS: usize = 50;
const CHAR_W: f64 = 7.2;

// Windows virtual key codes (WM_KEYDOWN wparam)
const VK_RETURN: u32 = 0x0D;
const VK_ESCAPE: u32 = 0x1B;
const VK_BACK: u32 = 0x08;
const VK_DELETE: u32 = 0x2E;
const VK_LEFT: u32 = 0x25;
const VK_UP: u32 = 0x26;
const VK_RIGHT: u32 = 0x27;
const VK_DOWN: u32 = 0x28;
const VK_TAB: u32 = 0x09;
const VK_HOME: u32 = 0x24;
const VK_END: u32 = 0x23;
const VK_PRIOR: u32 = 0x21;
const VK_NEXT: u32 = 0x22;
const VK_F1: u32 = 0x70;
const VK_F2: u32 = 0x71;


#[allow(dead_code)]
struct NwgCanvasSink {
    cells: RefCell<HashMap<(u32, u32), String>>,
    styles: RefCell<HashMap<(u32, u32), CellDisplayStyle>>,
    raw_values: RefCell<HashMap<(u32, u32), String>>,
    cursor_pos: Cell<Option<(u32, u32)>>,
}

impl NwgCanvasSink {
    fn new() -> Self {
        NwgCanvasSink {
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

impl CellSink for NwgCanvasSink {
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
#[allow(dead_code)]
enum NwgMode {
    Normal,
    Editing,
    Help,
    About,
}

#[allow(dead_code)]
struct NwgCanvasState {
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
    mode: Cell<NwgMode>,
    data_width: Cell<usize>,
    data_rows: Cell<usize>,
    data_cols: Cell<usize>,
    row_agg_func: RefCell<Vec<Option<AggFunc>>>,
    sink: NwgCanvasSink,
}

fn build_nwg_menu(win: &Window) -> Result<(MenuBar, Rc<RefCell<HashMap<String, Box<dyn FnMut()>>>>), Box<dyn std::error::Error>> {
    let registry: Rc<RefCell<HashMap<String, Box<dyn FnMut()>>>> = Rc::new(RefCell::new(HashMap::new()));

    // Register actions
    let action_names = &[
        "open", "save", "save_as", "quit",
        "undo", "redo", "cut", "copy", "paste", "delete_cell", "select_all", "find", "replace",
        "toggle_headers", "toggle_margins",
        "new_sheet", "rename_sheet", "delete_sheet",
        "sort_asc", "sort_desc", "balance_books",
        "export_tsv", "export_csv", "export_ods", "export_ascii",
        "about", "help_keybinds",
    ];
    for &name in action_names {
        let action = create_simple_action(name, registry.clone())?;
        let name_owned = name.to_string();
        action.connect_activate(move |_| {
            eprintln!("Menu action: {}", name_owned);
        })?;
    }

    // Build menu model
    let mut file_menu = create_menu()?;
    file_menu.append("Open", "app.open");
    file_menu.append("Save", "app.save");
    file_menu.append("Save As", "app.save_as");
    file_menu.append("Export TSV", "app.export_tsv");
    file_menu.append("Export CSV", "app.export_csv");
    file_menu.append("Export ODS", "app.export_ods");
    file_menu.append("Export ASCII", "app.export_ascii");
    file_menu.append("Quit", "app.quit");

    let mut edit_menu = create_menu()?;
    edit_menu.append("Undo", "app.undo");
    edit_menu.append("Redo", "app.redo");
    edit_menu.append("Cut", "app.cut");
    edit_menu.append("Copy", "app.copy");
    edit_menu.append("Paste", "app.paste");
    edit_menu.append("Delete", "app.delete_cell");
    edit_menu.append("Select All", "app.select_all");
    edit_menu.append("Find", "app.find");
    edit_menu.append("Replace", "app.replace");

    let mut view_menu = create_menu()?;
    view_menu.append("Toggle Headers", "app.toggle_headers");
    view_menu.append("Toggle Margins", "app.toggle_margins");

    let mut sheet_menu = create_menu()?;
    sheet_menu.append("New Sheet", "app.new_sheet");
    sheet_menu.append("Rename Sheet", "app.rename_sheet");
    sheet_menu.append("Delete Sheet", "app.delete_sheet");

    let mut data_menu = create_menu()?;
    data_menu.append("Sort Ascending", "app.sort_asc");
    data_menu.append("Sort Descending", "app.sort_desc");
    data_menu.append("Balance Books", "app.balance_books");

    let mut help_menu = create_menu()?;
    help_menu.append("Keybindings", "app.help_keybinds");
    help_menu.append("About", "app.about");

    let mut menubar_model = create_menu()?;
    menubar_model.append_submenu("File", &file_menu);
    menubar_model.append_submenu("Edit", &edit_menu);
    menubar_model.append_submenu("View", &view_menu);
    menubar_model.append_submenu("Sheet", &sheet_menu);
    menubar_model.append_submenu("Data", &data_menu);
    menubar_model.append_submenu("Help", &help_menu);

    let menubar = create_menubar(&menubar_model, win.hwnd(), registry.clone())?;
    Ok((menubar, registry))
}

pub fn run_nwg(app: &mut super::App) -> Result<(), Box<dyn std::error::Error>> {
    let nwg_app = rustxwidgets::backends::nwg::init()
        .map_err(|e| format!("NWG init failed: {e}"))?;

    let parent_cell: Rc<RefCell<Option<*mut c_void>>> = Rc::new(RefCell::new(None));

    let win = create_window(&parent_cell)?;
    win.set_title(&format!("corro {}", env!("CARGO_PKG_VERSION")));
    win.set_default_size(1200, 800);

    let win_hwnd = win.hwnd();
    let mut vbox = create_box(Orientation::Vertical, 0, win_hwnd)?;

    // Menu bar
    let (menubar, _action_registry) = build_nwg_menu(&win)?;
    vbox.append(&menubar);

    // Formula bar
    let mut formula_bar = create_box(Orientation::Horizontal, 2, win_hwnd)?;
    let addr_label = create_label(win_hwnd)?;
    let f_label = create_label(win_hwnd)?;
    f_label.set_text("  fx  ");
    let formula_entry = create_entry(win_hwnd)?;
    formula_bar.append(&addr_label);
    formula_bar.append(&f_label);
    formula_bar.append(&formula_entry);
    formula_bar.set_child_hexpand(&formula_entry, true);
    vbox.append(&formula_bar);

    // Compute viewport dimensions
    let data_width = 200usize;
    let data_rows = 30usize;
    let data_cols = 12usize;

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

    let canvas = create_canvas(win_hwnd)?;

    let state = NwgCanvasState {
        app,
        formula_entry: formula_entry.clone(),
        addr_label: addr_label.clone(),
        status_label: create_label(win_hwnd)?,
        canvas: canvas.clone(),
        display_rows: RefCell::new(display_rows),
        col_ixs: RefCell::new(col_ixs),
        col_widths: RefCell::new(col_widths),
        scroll_row: Cell::new(0),
        scroll_col: Cell::new(0),
        edit_buf: RefCell::new(String::new()),
        editing: Cell::new(false),
        last_row: Cell::new(cursor_row),
        last_col: Cell::new(cursor_col),
        mode: Cell::new(NwgMode::Normal),
        data_width: Cell::new(data_width),
        data_rows: Cell::new(data_rows),
        data_cols: Cell::new(data_cols),
        row_agg_func: RefCell::new(row_agg_func),
        sink: NwgCanvasSink::new(),
    };

    let state = Rc::new(state);
    update_formula_bar(&state, cursor_row, cursor_col);

    // Draw callback
    let state_draw = state.clone();
    canvas.set_draw_callback(Box::new(move |dc: &mut dyn DrawContext, w: i32, h: i32| {
        render_grid(dc, &state_draw, w, h);
    }));

    // Key handler
    let state_key = state.clone();
    canvas.on_key(Box::new(move |keyval: u32| -> bool {
        handle_key(keyval, &state_key)
    }));

    // Click handler
    let state_click = state.clone();
    canvas.on_click(Box::new(move |x: f64, y: f64| {
        handle_click(x, y, &state_click);
    }));

    let status_label = state.status_label.clone();
    vbox.append(&canvas);
    vbox.set_child_vexpand(&canvas, true);
    vbox.append(&status_label);
    win.set_child_box(&vbox);
    win.present();

    nwg_app.run().map_err(|e| format!("NWG error: {e}"))?;
    Ok(())
}

// ── Rendering ────────────────────────────────────────────────────────────

fn update_viewport_dims(state: &NwgCanvasState, w: i32, h: i32) -> bool {
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

fn render_grid(dc: &mut dyn DrawContext, state: &NwgCanvasState, w: i32, h: i32) {
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
        let mut sink = NwgCanvasSink::new();
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

    // Grid lines (thin fill_rect)
    let total_w: f64 = col_ixs.iter().map(|&c| col_pixel_width(c, &col_widths)).sum();
    let total_h = display_rows.len() as f64 * ROW_H;
    let mut y = HEADER_H;
    for _ in 0..=display_rows.len() {
        dc.fill_rect(ROW_LABEL_W, y, total_w, 0.5, 0.9, 0.9, 0.9, 1.0);
        y += ROW_H;
    }
    let mut x = ROW_LABEL_W;
    for &c in col_ixs.iter() {
        let cw = col_pixel_width(c, &col_widths);
        dc.fill_rect(x, HEADER_H, 0.5, total_h, 0.9, 0.9, 0.9, 1.0);
        x += cw;
    }
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

// ── Viewport helpers ─────────────────────────────────────────────────────

fn recompute_viewport(state: &NwgCanvasState) {
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

// ── Click handling ───────────────────────────────────────────────────────

fn handle_click(x: f64, y: f64, state: &NwgCanvasState) {
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
            if state.editing.get() {
                commit_edit(state);
            }
            state.last_col.set(c);
            state.last_row.set(logical_row);
            let app = unsafe { &mut *state.app };
            app.core.anchor = Some(SheetCursor { row: logical_row, col: c });
            update_state_cursor(state, logical_row, c);
            state.canvas.queue_redraw();
        }
    }
}

fn display_rows_len(state: &NwgCanvasState) -> usize {
    state.display_rows.borrow().len()
}

// ── Edit helpers ─────────────────────────────────────────────────────────

fn commit_edit(state: &NwgCanvasState) {
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
    state.mode.set(NwgMode::Normal);
}

fn start_edit(state: &NwgCanvasState) {
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
    state.mode.set(NwgMode::Editing);
    state.formula_entry.set_text("");
    state.canvas.queue_redraw();
}

fn start_edit_with(state: &NwgCanvasState, ch: char) {
    start_edit(state);
    state.edit_buf.borrow_mut().push(ch);
    state.canvas.queue_redraw();
}

fn move_cursor(state: &NwgCanvasState, dr: isize, dc: isize) {
    let cur_row = state.last_row.get() as isize;
    let cur_col = state.last_col.get() as isize;
    let new_row = (cur_row + dr).max(HEADER_ROWS as isize) as usize;
    let new_col = (cur_col + dc).max(0isize) as usize;
    state.last_row.set(new_row);
    state.last_col.set(new_col);
    update_state_cursor(state, new_row, new_col);
}

fn update_row_col_from_state(state: &NwgCanvasState) {
    let app = unsafe { &mut *state.app };
    let row = state.last_row.get();
    let col = state.last_col.get();
    app.core.cursor.row = row;
    app.core.cursor.col = col;

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

fn update_state_cursor(state: &NwgCanvasState, row: usize, col: usize) {
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

fn update_formula_bar(state: &NwgCanvasState, row: usize, col: usize) {
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
    state.formula_entry.set_text(&val);
}

// ── Keyboard Handling ────────────────────────────────────────────────────

fn handle_key(keyval: u32, state_rc: &Rc<NwgCanvasState>) -> bool {
    let state: &NwgCanvasState = &**state_rc;
    let app = unsafe { &mut *state.app };
    let mode = state.mode.get();
    let key = keyval & 0xFF;

    match mode {
        NwgMode::Help | NwgMode::About => {
            if key == VK_ESCAPE {
                state.mode.set(NwgMode::Normal);
                state.canvas.queue_redraw();
                return true;
            }
            return true;
        }
        _ => {}
    }

    if state.editing.get() {
        return handle_edit_key(key, state);
    }

    match key {
        VK_F1 => {
            state.mode.set(NwgMode::Help);
            state.canvas.queue_redraw();
            true
        }
        VK_F2 => {
            start_edit(state);
            true
        }
        VK_RETURN => {
            move_cursor(state, 1, 0);
            true
        }
        VK_TAB => {
            move_cursor(state, 0, 1);
            true
        }
        VK_ESCAPE => {
            app.core.anchor = Some(SheetCursor {
                row: state.last_row.get(),
                col: state.last_col.get(),
            });
            state.canvas.queue_redraw();
            true
        }
        VK_LEFT => {
            move_cursor(state, 0, -1);
            true
        }
        VK_RIGHT => {
            move_cursor(state, 0, 1);
            true
        }
        VK_UP => {
            if state.last_row.get() > HEADER_ROWS {
                move_cursor(state, -1, 0);
            }
            true
        }
        VK_DOWN => {
            move_cursor(state, 1, 0);
            true
        }
        VK_HOME => {
            state.last_col.set(MARGIN_COLS);
            update_state_cursor(state, state.last_row.get(), MARGIN_COLS);
            true
        }
        VK_END => {
            state.last_col.set(state.last_col.get() + 10);
            update_state_cursor(state, state.last_row.get(), state.last_col.get());
            true
        }
        VK_PRIOR => {
            let dr = state.data_rows.get();
            let new_row = state.last_row.get().saturating_sub(dr);
            state.last_row.set(new_row.max(HEADER_ROWS));
            update_state_cursor(state, state.last_row.get(), state.last_col.get());
            true
        }
        VK_NEXT => {
            let dr = state.data_rows.get();
            state.last_row.set(state.last_row.get() + dr);
            update_state_cursor(state, state.last_row.get(), state.last_col.get());
            true
        }
        VK_DELETE => {
            handle_delete(state);
            true
        }
        VK_BACK => {
            handle_delete(state);
            true
        }
        _ if (32..=126).contains(&key) => {
            start_edit_with(state, char::from_u32(key).unwrap_or('?'));
            true
        }
        _ => false,
    }
}

fn handle_delete(state: &NwgCanvasState) {
    let app = unsafe { &mut *state.app };
    let row = state.last_row.get();
    let col = state.last_col.get();
    let main_row = row.saturating_sub(HEADER_ROWS);
    let main_col = col.saturating_sub(MARGIN_COLS);
    if let Some(anchor) = app.core.anchor {
        let r1 = anchor.row.min(row);
        let r2 = anchor.row.max(row);
        let c1 = anchor.col.min(col);
        let c2 = anchor.col.max(col);
        let ro = r2 - r1 + 1;
        let co = c2 - c1 + 1;
        if ro > 1 || co > 1 {
            for r in r1..=r2 {
                for c in c1..=c2 {
                    let main_r = r.saturating_sub(HEADER_ROWS);
                    let main_c = c.saturating_sub(MARGIN_COLS);
                    let addr = CellAddr::Main { row: main_r as u32, col: main_c as u32 };
                    app.core.workbook.active_sheet_mut().grid.set(&addr, String::new());
                }
            }
            let sheet_id = app.core.workbook.sheet_id(app.core.workbook.active_sheet);
            let addr = CellAddr::Main { row: main_row as u32, col: main_col as u32 };
            let val = String::new();
            let op = Op::SetCell { addr, value: val };
            let wbo = WorkbookOp::SheetOp { sheet_id, op };
            if let Some(ref p) = app.core.path.clone() {
                let mut active_sheet = sheet_id;
                let _ = crate::io::commit_workbook_op(
                    p, &mut app.core.offset, &mut app.core.workbook,
                    &mut active_sheet, &wbo,
                );
                app.core.ops_applied = app.core.ops_applied.saturating_add(1);
            }
            app.core.status = "Cleared selection".into();
            recompute_viewport(state);
            state.canvas.queue_redraw();
            return;
        }
    };
    let addr = CellAddr::Main { row: main_row as u32, col: main_col as u32 };
    app.core.workbook.active_sheet_mut().grid.set(&addr, String::new());
    let sheet_id = app.core.workbook.sheet_id(app.core.workbook.active_sheet);
    let op = Op::SetCell { addr, value: String::new() };
    let wbo = WorkbookOp::SheetOp { sheet_id, op };
    if let Some(ref p) = app.core.path.clone() {
        let mut active_sheet = sheet_id;
        let _ = crate::io::commit_workbook_op(
            p, &mut app.core.offset, &mut app.core.workbook,
            &mut active_sheet, &wbo,
        );
        app.core.ops_applied = app.core.ops_applied.saturating_add(1);
    }
    recompute_viewport(state);
    state.canvas.queue_redraw();
}

fn handle_edit_key(key: u32, state: &NwgCanvasState) -> bool {
    match key {
        VK_RETURN => {
            commit_edit(state);
            move_cursor(state, 1, 0);
            true
        }
        VK_ESCAPE => {
            state.editing.set(false);
            state.edit_buf.borrow_mut().clear();
            state.mode.set(NwgMode::Normal);
            update_formula_bar(state, state.last_row.get(), state.last_col.get());
            state.canvas.queue_redraw();
            true
        }
        VK_TAB => {
            commit_edit(state);
            move_cursor(state, 0, 1);
            true
        }
        VK_BACK => {
            state.edit_buf.borrow_mut().pop();
            state.canvas.queue_redraw();
            true
        }
        VK_DELETE => {
            state.edit_buf.borrow_mut().clear();
            state.canvas.queue_redraw();
            true
        }
        VK_LEFT => {
            commit_edit(state);
            let c = state.last_col.get();
            if c > 0 {
                state.last_col.set(c - 1);
                update_state_cursor(state, state.last_row.get(), state.last_col.get());
            }
            true
        }
        VK_RIGHT => {
            commit_edit(state);
            state.last_col.set(state.last_col.get() + 1);
            update_state_cursor(state, state.last_row.get(), state.last_col.get());
            true
        }
        VK_UP => {
            commit_edit(state);
            let r = state.last_row.get();
            if r > HEADER_ROWS {
                state.last_row.set(r - 1);
                update_state_cursor(state, state.last_row.get(), state.last_col.get());
            }
            true
        }
        VK_DOWN => {
            commit_edit(state);
            state.last_row.set(state.last_row.get() + 1);
            update_state_cursor(state, state.last_row.get(), state.last_col.get());
            true
        }
        _ if (32..=126).contains(&key) => {
            state.edit_buf.borrow_mut().push(char::from_u32(key).unwrap_or('?'));
            state.formula_entry.set_text(&state.edit_buf.borrow());
            state.canvas.queue_redraw();
            true
        }
        _ => false,
    }
}
