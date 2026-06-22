use rustxwidgets::common::*;
use rustxwidgets::core::DrawContext;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use crate::grid::{CellAddr, SheetCursor, HEADER_ROWS, MARGIN_COLS};
use crate::ops::{Op, WorkbookOp};
use crate::ui_core;

use super::compute::{self, CellDisplayStyle};
use super::dialogs;
use super::render::{self, CellSink};

// ---------------------------------------------------------------------------
// Platform key constants
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod key {
    pub const RETURN: u32 = 0xFF0D;
    pub const ESCAPE: u32 = 0xFF1B;
    pub const BACKSPACE: u32 = 0xFF08;
    pub const DELETE: u32 = 0xFFFF;
    pub const LEFT: u32 = 0xFF51;
    pub const UP: u32 = 0xFF52;
    pub const RIGHT: u32 = 0xFF53;
    pub const DOWN: u32 = 0xFF54;
    pub const TAB: u32 = 0xFF09;
    pub const HOME: u32 = 0xFF50;
    pub const END: u32 = 0xFF57;
    pub const PAGE_UP: u32 = 0xFF55;
    pub const PAGE_DOWN: u32 = 0xFF56;
    pub const F1: u32 = 0xFFBE;
    pub const F2: u32 = 0xFFBF;
}

#[cfg(windows)]
mod key {
    pub const RETURN: u32 = 0x0D;
    pub const ESCAPE: u32 = 0x1B;
    pub const BACKSPACE: u32 = 0x08;
    pub const DELETE: u32 = 0x2E;
    pub const LEFT: u32 = 0x25;
    pub const UP: u32 = 0x26;
    pub const RIGHT: u32 = 0x27;
    pub const DOWN: u32 = 0x28;
    pub const TAB: u32 = 0x09;
    pub const HOME: u32 = 0x24;
    pub const END: u32 = 0x23;
    pub const PAGE_UP: u32 = 0x21;
    pub const PAGE_DOWN: u32 = 0x22;
    pub const F1: u32 = 0x70;
    pub const F2: u32 = 0x71;
}

use key::*;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const FONT_SIZE: f64 = 12.0;
const ROW_H: f64 = 20.0;
const HEADER_H: f64 = 24.0;
const ROW_LABEL_W: f64 = 50.0;
const MAX_RENDER_ROWS: usize = 500;
const MAX_RENDER_COLS: usize = 50;
const CHAR_W: f64 = 7.2;

// ---------------------------------------------------------------------------
// Mode
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum GuiMode {
    Normal,
    Help,
}

// ---------------------------------------------------------------------------
// CanvasSink
// ---------------------------------------------------------------------------

struct GuiCanvasSink {
    cells: RefCell<HashMap<(u32, u32), String>>,
    styles: RefCell<HashMap<(u32, u32), CellDisplayStyle>>,
    raw_values: RefCell<HashMap<(u32, u32), String>>,
    cursor_pos: Cell<Option<(u32, u32)>>,
}

impl GuiCanvasSink {
    fn new() -> Self {
        GuiCanvasSink {
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
}

impl CellSink for GuiCanvasSink {
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

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

struct GuiState {
    app: *mut super::App,
    #[allow(dead_code)]
    rxapp: rustxwidgets::App,
    canvas: Canvas,
    formula_entry: Entry,
    addr_label: Label,
    status_label: Label,
    editing: Cell<bool>,
    edit_buf: RefCell<String>,
    mode: Cell<GuiMode>,
    last_row: Cell<usize>,
    last_col: Cell<usize>,
    data_rows: Cell<usize>,
    data_cols: Cell<usize>,
    last_key: Cell<u32>,
    key_counter: Cell<u64>,
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render_to(
    sink: &GuiCanvasSink,
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
    let cells = sink.cells.borrow();
    let styles = sink.styles.borrow();

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
            let cx = ROW_LABEL_W + col_ixs.iter().take(ci).map(|&pc| *col_widths.get(&pc).unwrap_or(&8) as f64 * CHAR_W).sum::<f64>();

            let key = (ri as u32, c as u32);
            let raw_text = cells.get(&key).map(|s| s.as_str()).unwrap_or("");
            let style_key = (ri as u32, c as u32);
            let style = styles.get(&style_key).copied().unwrap_or(CellDisplayStyle::Default);
            let is_current = logical_row == cursor_row && c == cursor_col;

            let bg = if is_current {
                if is_editing { (1.0, 1.0, 0.8, 1.0) } else { (0.8, 0.9, 1.0, 1.0) }
            } else if is_sel_row && selection_anchor.is_some() {
                (0.9, 0.95, 1.0, 1.0)
            } else {
                (1.0, 1.0, 1.0, 1.0)
            };

            dc.fill_rect(cx, ry, cw, ROW_H, bg.0, bg.1, bg.2, bg.3);

            // Selection highlight
            if is_current && !is_editing {
                dc.stroke_rect(cx, ry, cw, ROW_H, 0.0, 0.4, 0.8, 1.0, 2.0);
            }

            // Grid lines
            dc.stroke_rect(cx, ry, cw, ROW_H, 0.8, 0.8, 0.8, 1.0, 0.5);

            if !raw_text.is_empty() {
                match style {
                    CellDisplayStyle::Default => {
                        dc.draw_text(cx + 2.0, ry + 2.0, raw_text, "monospace", FONT_SIZE, 0.0, 0.0, 0.0, 1.0);
                    }
                    CellDisplayStyle::Cursor | CellDisplayStyle::Selected => {
                        dc.draw_text(cx + 2.0, ry + 2.0, raw_text, "monospace", FONT_SIZE, 0.0, 0.0, 0.0, 1.0);
                    }
                    CellDisplayStyle::Aggregate | CellDisplayStyle::FooterAggregate => {
                        dc.draw_text(cx + 2.0, ry + 2.0, raw_text, "monospace", FONT_SIZE, 0.5, 0.5, 0.5, 1.0);
                    }
                    CellDisplayStyle::ActiveHeader | CellDisplayStyle::InactiveHeader => {
                        dc.draw_text(cx + 2.0, ry + 2.0, raw_text, "monospace", FONT_SIZE, 0.3, 0.3, 0.3, 1.0);
                    }
                }
            }
        }
    }

    // Edit overlay on cursor cell
    if is_editing && !edit_text.is_empty() {
        if let Some(pos) = col_ixs.iter().position(|&c| c == cursor_col) {
            let cw = *col_widths.get(&cursor_col).unwrap_or(&8) as f64 * CHAR_W;
            let cx = ROW_LABEL_W + col_ixs.iter().take(pos).map(|&pc| *col_widths.get(&pc).unwrap_or(&8) as f64 * CHAR_W).sum::<f64>();
            if let Some(pos_r) = display_rows.iter().position(|&r| r == cursor_row) {
                let ry = HEADER_H + pos_r as f64 * ROW_H;
                dc.fill_rect(cx, ry, cw, ROW_H, 1.0, 1.0, 0.8, 1.0);
                dc.draw_text(cx + 2.0, ry + 2.0, edit_text, "monospace", FONT_SIZE, 0.0, 0.0, 0.0, 1.0);
            }
        }
    }
}

fn render_grid(dc: &mut dyn DrawContext, state: &GuiState, w: i32, h: i32) {
    dc.clear(0.94, 0.94, 0.94, 1.0);
    dc.clip(0.0, 0.0, w as f64, h as f64);

    let app = unsafe { &*state.app };
    let hr = HEADER_ROWS;
    let lm = MARGIN_COLS;
    let cursor_row = state.last_row.get();
    let cursor_col = state.last_col.get();

    let display_rows: Vec<usize> = {
        let sheet = app.core.workbook.active_sheet();
        ui_core::visible_row_indices(sheet, app.core.cursor, state.data_rows.get(), 0).0
    };
    let col_ixs: Vec<usize> = {
        let sheet = app.core.workbook.active_sheet();
        ui_core::visible_col_indices(sheet, app.core.cursor, state.data_cols.get(), 0).0
    };
    let mr = app.core.workbook.active_sheet().grid.main_rows();
    let mc = app.core.workbook.active_sheet().grid.main_cols();

    // Row headers
    for (ri, &logical_row) in display_rows.iter().enumerate().take(MAX_RENDER_ROWS) {
        let ry = HEADER_H + ri as f64 * ROW_H;
        let label = crate::addr::ui_row_label(logical_row, mr);
        let (_, _, tw, _) = dc.text_extents(&label, "monospace", FONT_SIZE);
        dc.fill_rect(0.0, ry, ROW_LABEL_W, ROW_H, 0.9, 0.9, 0.9, 1.0);
        dc.draw_text(ROW_LABEL_W - tw - 4.0, ry + 2.0, &label, "monospace", FONT_SIZE, 0.3, 0.3, 0.3, 1.0);
    }

    // Column headers
    for (ci, &c) in col_ixs.iter().enumerate().take(MAX_RENDER_COLS) {
        let cw = sheet_rec_col_width(&app.core.workbook.active_sheet(), c) as f64 * CHAR_W;
        let cx = ROW_LABEL_W + col_ixs.iter().take(ci).map(|&pc| sheet_rec_col_width(&app.core.workbook.active_sheet(), pc) as f64 * CHAR_W).sum::<f64>();
        let col_name = crate::addr::ui_column_fragment(c, mc);
        dc.fill_rect(cx, 0.0, cw, HEADER_H, 0.9, 0.9, 0.9, 1.0);
        let (_, _, tw, _) = dc.text_extents(&col_name, "monospace", FONT_SIZE);
        dc.draw_text(cx + (cw - tw) / 2.0, (HEADER_H - FONT_SIZE * 1.2) / 2.0, &col_name, "monospace", FONT_SIZE, 0.3, 0.3, 0.3, 1.0);
    }

    let col_widths: HashMap<usize, usize> = col_ixs.iter()
        .map(|&c| (c, sheet_rec_col_width(&app.core.workbook.active_sheet(), c)))
        .collect();

    let row_agg_func = compute::compute_row_agg_func(
        &app.core.workbook.active_sheet().grid,
        &display_rows, hr, mr,
    );

    // Fill cells via the shared render pipeline
    let sink_snapshot: HashMap<(u32, u32), String>;
    {
        let mut sink = GuiCanvasSink::new();
        render::fill_cells(
            &mut sink, &display_rows, &col_ixs, &col_widths,
            &app.core.workbook.active_sheet().grid,
            hr, mr, mc,
            lm, state.data_cols.get(),
            cursor_row, cursor_col,
            &row_agg_func,
        );
        sink_snapshot = sink.cells.borrow().clone();
        render_to(
            &sink, dc, &col_ixs, &col_widths, &display_rows,
            mr, mc,
            cursor_row, cursor_col,
            state.editing.get(),
            &state.edit_buf.borrow(),
            app.core.anchor.map(|a| (a.row, a.col)),
        );
    }

    // Status line at bottom
    if h as f64 > HEADER_H + 20.0 {
        dc.fill_rect(0.0, h as f64 - 20.0, w as f64, 20.0, 0.9, 0.9, 0.9, 1.0);
    }

    // Diagnostic: read first data cell from grid + sink
    let first_row = hr;
    let first_col = lm;
    let main_row = first_row.saturating_sub(hr);
    let main_col = first_col.saturating_sub(lm);
    let addr = crate::grid::CellAddr::Main { row: main_row as u32, col: main_col as u32 };
    let cell_val = app.core.workbook.active_sheet().grid.get(&addr).unwrap_or_default();
    let is_editing = if state.editing.get() { "EDIT" } else { "NORM" };
    let eb = state.edit_buf.borrow().clone();
    let buf_display = if eb.is_empty() { "(empty)" } else { &eb };
    let sink_key_col = MARGIN_COLS as u32;
    let sink_first = sink_snapshot.get(&(0u32, sink_key_col)).cloned().unwrap_or_default();
    let cur = (state.last_row.get(), state.last_col.get());
    let lk = state.last_key.get();
    let kc = state.key_counter.get();
    dc.draw_text(100.0, h as f64 - 140.0, &format!("Grid(0,0)='{cell_val}' Sink(0,{sink_key_col})='{sink_first}' Cur=({},{})", cur.0, cur.1), "monospace", 12.0, 0.0, 0.5, 0.0, 1.0);
    dc.draw_text(100.0, h as f64 - 120.0, &format!("lastKey=0x{lk:04x} cnt={kc}", ), "monospace", 12.0, 1.0, 0.0, 0.0, 1.0);
    dc.draw_text(100.0, h as f64 - 100.0, &format!("Mode:{is_editing} Buf:'{buf_display}'"), "monospace", 14.0, 0.5, 0.0, 0.5, 1.0);
}

fn sheet_rec_col_width(sheet: &crate::ops::SheetState, col: usize) -> usize {
    sheet.grid.col_width(col).max(1)
}

// ---------------------------------------------------------------------------
// Keyboard handling
// ---------------------------------------------------------------------------

fn handle_key(keyval: u32, state_rc: &Rc<GuiState>) -> bool {
    let state: &GuiState = &**state_rc;
    state.last_key.set(keyval);
    let app = unsafe { &mut *state.app };
    let key = {
        #[cfg(windows)]
        { keyval & 0xFF }
        #[cfg(not(windows))]
        { keyval }
    };

    match state.mode.get() {
        GuiMode::Help => {
            if key == ESCAPE {
                state.mode.set(GuiMode::Normal);
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
        F1 => {
            state.mode.set(GuiMode::Help);
            state.canvas.queue_redraw();
            true
        }
        F2 => {
            start_edit(state);
            true
        }
        RETURN => {
            move_cursor(state, 1, 0);
            true
        }
        TAB => {
            move_cursor(state, 0, 1);
            true
        }
        ESCAPE => {
            app.core.anchor = Some(SheetCursor {
                row: state.last_row.get(),
                col: state.last_col.get(),
            });
            state.canvas.queue_redraw();
            true
        }
        LEFT => {
            move_cursor(state, 0, -1);
            true
        }
        RIGHT => {
            move_cursor(state, 0, 1);
            true
        }
        UP => {
            if state.last_row.get() > HEADER_ROWS {
                move_cursor(state, -1, 0);
            }
            true
        }
        DOWN => {
            move_cursor(state, 1, 0);
            true
        }
        HOME => {
            state.last_col.set(MARGIN_COLS);
            update_state_cursor(state, state.last_row.get(), MARGIN_COLS);
            true
        }
        END => {
            state.last_col.set(state.last_col.get() + 10);
            update_state_cursor(state, state.last_row.get(), state.last_col.get());
            true
        }
        PAGE_UP => {
            let dr = state.data_rows.get();
            let new_row = state.last_row.get().saturating_sub(dr);
            state.last_row.set(new_row.max(HEADER_ROWS));
            update_state_cursor(state, state.last_row.get(), state.last_col.get());
            true
        }
        PAGE_DOWN => {
            let dr = state.data_rows.get();
            state.last_row.set(state.last_row.get() + dr);
            update_state_cursor(state, state.last_row.get(), state.last_col.get());
            true
        }
        DELETE => {
            handle_delete(state);
            true
        }
        BACKSPACE => {
            handle_delete(state);
            true
        }
        _ if (32..=126).contains(&key) => {
            let ch = char::from_u32(key).unwrap_or('?');
            start_edit_with(state, ch);
            true
        }
        _ => false,
    }
}

fn handle_edit_key(key: u32, state: &GuiState) -> bool {
    match key {
        RETURN => {
            commit_edit(state);
            move_cursor(state, 1, 0);
            true
        }
        ESCAPE => {
            state.editing.set(false);
            state.edit_buf.borrow_mut().clear();
            state.mode.set(GuiMode::Normal);
            update_formula_bar(state, state.last_row.get(), state.last_col.get());
            state.canvas.queue_redraw();
            true
        }
        TAB => {
            commit_edit(state);
            move_cursor(state, 0, 1);
            true
        }
        BACKSPACE => {
            state.edit_buf.borrow_mut().pop();
            state.canvas.queue_redraw();
            true
        }
        DELETE => {
            state.edit_buf.borrow_mut().clear();
            state.canvas.queue_redraw();
            true
        }
        LEFT => {
            commit_edit(state);
            let c = state.last_col.get();
            if c > 0 {
                state.last_col.set(c - 1);
                update_state_cursor(state, state.last_row.get(), state.last_col.get());
            }
            true
        }
        RIGHT => {
            commit_edit(state);
            state.last_col.set(state.last_col.get() + 1);
            update_state_cursor(state, state.last_row.get(), state.last_col.get());
            true
        }
        UP => {
            commit_edit(state);
            if state.last_row.get() > HEADER_ROWS {
                state.last_row.set(state.last_row.get() - 1);
                update_state_cursor(state, state.last_row.get(), state.last_col.get());
            }
            true
        }
        DOWN => {
            commit_edit(state);
            state.last_row.set(state.last_row.get() + 1);
            update_state_cursor(state, state.last_row.get(), state.last_col.get());
            true
        }
        _ if (32..=126).contains(&key) => {
            let ch = char::from_u32(key).unwrap_or('?');
            state.edit_buf.borrow_mut().push(ch);
            state.canvas.queue_redraw();
            true
        }
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// Edit operations
// ---------------------------------------------------------------------------

fn start_edit(state: &GuiState) {
    state.editing.set(true);
    state.edit_buf.borrow_mut().clear();
    state.formula_entry.set_text("");
    state.formula_entry.grab_focus();
    state.canvas.queue_redraw();
}

fn start_edit_with(state: &GuiState, ch: char) {
    state.editing.set(true);
    state.edit_buf.borrow_mut().clear();
    let s = ch.to_string();
    state.edit_buf.borrow_mut().push_str(&s);
    state.formula_entry.set_text(&s);
    state.formula_entry.grab_focus();
    state.canvas.queue_redraw();
}

fn commit_edit(state: &GuiState) {
    state.editing.set(false);
    state.mode.set(GuiMode::Normal);
    let val = state.edit_buf.borrow().clone();
    if !val.is_empty() {
        let app = unsafe { &mut *state.app };
        let row = state.last_row.get();
        let col = state.last_col.get();
        let main_row = row.saturating_sub(HEADER_ROWS);
        let main_col = col.saturating_sub(MARGIN_COLS);
        let addr = CellAddr::Main { row: main_row as u32, col: main_col as u32 };
        app.core.workbook.active_sheet_mut().grid.set(&addr, val.clone());
        let sheet_id = app.core.workbook.sheet_id(app.core.workbook.active_sheet);
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
        let main_rows = crate::addr::MainRows(app.core.workbook.active_sheet().grid.main_rows());
        let main_cols = crate::addr::MainCols(app.core.workbook.active_sheet().grid.main_cols());
        app.core.status = format!("Set cell {}", crate::addr::sheet_cursor_to_addr(
            crate::addr::LogicalRow(row),
            crate::addr::GlobalCol(col),
            main_rows,
            main_cols,
        ));
        recompute_viewport(state);
    }
    state.canvas.queue_redraw();
}

fn handle_delete(state: &GuiState) {
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
            app.core.status = "Cleared selection".into();
            recompute_viewport(state);
            state.canvas.queue_redraw();
            return;
        }
    }
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

fn recompute_viewport(state: &GuiState) {
    let app = unsafe { &*state.app };
    let hr = HEADER_ROWS;
    let cursor_row = state.last_row.get();
    let cursor_col = state.last_col.get();
    let sheet = app.core.workbook.active_sheet();
    let (display_rows, _) = ui_core::visible_row_indices(sheet, app.core.cursor, state.data_rows.get(), 0);
    let (col_ixs, _) = ui_core::visible_col_indices(sheet, app.core.cursor, state.data_cols.get(), 0);
    if !display_rows.contains(&cursor_row) || !col_ixs.contains(&cursor_col) {
        let new_row = if cursor_row > display_rows.last().copied().unwrap_or(hr) {
            display_rows.first().copied().unwrap_or(hr)
        } else {
            cursor_row
        };
        let new_col = if cursor_col > col_ixs.last().copied().unwrap_or(MARGIN_COLS) {
            col_ixs.first().copied().unwrap_or(MARGIN_COLS)
        } else {
            cursor_col
        };
        update_state_cursor(state, new_row, new_col);
    }
}

fn move_cursor(state: &GuiState, dr: isize, dc: isize) {
    let row = state.last_row.get();
    let col = state.last_col.get();
    let app = unsafe { &mut *state.app };
    let mr = app.core.workbook.active_sheet().grid.main_rows();
    let mc = app.core.workbook.active_sheet().grid.main_cols() + MARGIN_COLS;
    let new_row = (row as isize + dr).max(HEADER_ROWS as isize).min((HEADER_ROWS + mr).max(HEADER_ROWS) as isize) as usize;
    let new_col = (col as isize + dc).max(MARGIN_COLS as isize).min(mc as isize - 1).max(MARGIN_COLS as isize) as usize;
    update_state_cursor(state, new_row, new_col);
}

fn update_state_cursor(state: &GuiState, row: usize, col: usize) {
    state.last_row.set(row);
    state.last_col.set(col);
    let app = unsafe { &mut *state.app };
    app.core.cursor.row = row;
    app.core.cursor.col = col;
    update_formula_bar(state, row, col);
    state.canvas.queue_redraw();
}

fn update_formula_bar(state: &GuiState, row: usize, col: usize) {
    let app = unsafe { &*state.app };
    let main_row = row.saturating_sub(HEADER_ROWS);
    let main_col = col.saturating_sub(MARGIN_COLS);
    let addr_str = crate::addr::sheet_cursor_to_addr(
        crate::addr::LogicalRow(row),
        crate::addr::GlobalCol(col),
        crate::addr::MainRows(app.core.workbook.active_sheet().grid.main_rows()),
        crate::addr::MainCols(app.core.workbook.active_sheet().grid.main_cols()),
    );
    state.addr_label.set_text(&addr_str.to_string());
    let addr = crate::grid::CellAddr::Main { row: main_row as u32, col: main_col as u32 };
    let val = app.core.workbook.active_sheet().grid.get(&addr).unwrap_or_default();
    state.formula_entry.set_text(&val);
    state.status_label.set_text(&app.core.status);
    if state.status_label.raw_handle().is_null() {
        // status label will be updated; no-op
    }
}

// ---------------------------------------------------------------------------
// Click handling
// ---------------------------------------------------------------------------

fn handle_click(x: f64, y: f64, state_rc: &Rc<GuiState>) {
    let state: &GuiState = &**state_rc;
    let app = unsafe { &mut *state.app };
    if x < ROW_LABEL_W || y < HEADER_H {
        return;
    }
    let col_ixs: Vec<usize> = {
        let sheet = app.core.workbook.active_sheet();
        ui_core::visible_col_indices(sheet, app.core.cursor, state.data_cols.get(), 0).0
    };
    let mut cx = ROW_LABEL_W;
    for &c in &col_ixs {
        let cw = sheet_rec_col_width(&app.core.workbook.active_sheet(), c) as f64 * CHAR_W;
        if x >= cx && x < cx + cw {
            let ri = ((y - HEADER_H) / ROW_H) as usize;
            let display_rows: Vec<usize> = {
                let sheet = app.core.workbook.active_sheet();
                ui_core::visible_row_indices(sheet, app.core.cursor, state.data_rows.get(), 0).0
            };
            if ri < display_rows.len() {
                let logical_row = display_rows[ri];
                state.last_row.set(logical_row);
                state.last_col.set(c);
                app.core.cursor.row = logical_row;
                app.core.cursor.col = c;
                if app.core.anchor.is_none() {
                    app.core.anchor = Some(SheetCursor { row: logical_row, col: c });
                }
                update_formula_bar(state, logical_row, c);
                start_edit(state);
                state.canvas.queue_redraw();
            }
            return;
        }
        cx += cw;
    }
}

// ---------------------------------------------------------------------------
// Menu building
// ---------------------------------------------------------------------------

fn build_menu(rxapp: &rustxwidgets::App, win: &Window, state: &Rc<GuiState>) -> Result<MenuBar, Box<dyn std::error::Error>> {
    use crate::gui::menu;

    let action_group = rxapp.ensure_action_group()?;

    let file_menu = menu::build_submenu(rxapp, menu::FILE_MENU, "app")?;
    let edit_menu = menu::build_submenu(rxapp, menu::EDIT_MENU, "app")?;
    let view_menu = menu::build_submenu(rxapp, menu::VIEW_MENU, "app")?;
    let sheet_menu = menu::build_submenu(rxapp, menu::SHEET_MENU, "app")?;
    let data_menu = menu::build_submenu(rxapp, menu::DATA_MENU, "app")?;
    let help_menu = menu::build_submenu(rxapp, menu::HELP_MENU, "app")?;

    let mut menubar_model = rxapp.new_menu()?;
    menubar_model.append_submenu("File", &file_menu);
    menubar_model.append_submenu("Edit", &edit_menu);
    menubar_model.append_submenu("View", &view_menu);
    menubar_model.append_submenu("Sheet", &sheet_menu);
    menubar_model.append_submenu("Data", &data_menu);
    menubar_model.append_submenu("Help", &help_menu);

    // Register action callbacks with state access
    let s = state.clone();
    for &items in &[menu::FILE_MENU, menu::EDIT_MENU, menu::VIEW_MENU, menu::SHEET_MENU, menu::DATA_MENU, menu::HELP_MENU] {
        for item in items {
            let name = menu::action_kind_to_name(item.action);
            let name_owned = name.to_string();
            let state_cb = s.clone();
            menu::register_action(rxapp, name, move || handle_menu_action(&name_owned, &state_cb))?;
        }
    }

    let menubar = rxapp.new_menubar(&menubar_model, action_group)?;
    win.insert_action_group("app", action_group);

    Ok(menubar)
}

fn handle_menu_action(name: &str, state: &GuiState) {
    let app = unsafe { &mut *state.app };
    match name {
        "open" => {
            if let Some(path) = dialogs::file_open_dialog() {
                match crate::io::load_workbook_snapshot(&path) {
                    Ok(snapshot) => {
                        app.core.workbook = crate::ops::WorkbookState::from_snapshot(&snapshot);
                        app.core.offset = 0;
                        app.core.ops_applied = 0;
                        app.core.path = Some(path);
                        app.core.status = "Opened file".into();
                        recompute_viewport(state);
                        state.canvas.queue_redraw();
                    }
                    Err(e) => app.core.status = format!("Open error: {e}"),
                }
            }
        }
        "save" => {
            if let Some(ref p) = app.core.path.clone() {
                let snapshot = crate::ops::WorkbookSnapshot::from_workbook(&app.core.workbook);
                match crate::io::save_workbook(p, &snapshot) {
                    Ok(()) => app.core.status = "Saved".into(),
                    Err(e) => app.core.status = format!("Save error: {e}"),
                }
            }
        }
        "save_as" => {
            if let Some(path) = dialogs::file_save_dialog() {
                app.core.path = Some(path.clone());
                let snapshot = crate::ops::WorkbookSnapshot::from_workbook(&app.core.workbook);
                match crate::io::save_workbook(&path, &snapshot) {
                    Ok(()) => app.core.status = format!("Saved to {}", path.display()),
                    Err(e) => app.core.status = format!("Save error: {e}"),
                }
            }
        }
        "quit" => {
            #[cfg(unix)]
            let _ = rustxwidgets::backends_gtk_adapter::quit_main_loop();
            #[cfg(windows)]
            rustxwidgets::backends_nwg_adapter::quit_main_loop();
        }
        "find" => dialogs::find_dialog(|result| {
            if let Some(text) = result {
                app.core.status = format!("Find: {text}");
            }
        }),
        "replace" => dialogs::replace_dialog(|result| {
            if let Some((find, replace)) = result {
                app.core.status = format!("Replace: '{find}' with '{replace}'");
            }
        }),
        "sort_asc" => {
            let wb = crate::ops::WorkbookState::default();
            dialogs::sort_dialog(&wb, |result| {
                if let Some((col, asc)) = result {
                    app.core.status = format!("Sort col {col} asc: {asc}");
                }
            });
        }
        "sort_desc" => {
            let wb = crate::ops::WorkbookState::default();
            dialogs::sort_dialog(&wb, |result| {
                if let Some((col, asc)) = result {
                    app.core.status = format!("Sort col {col} desc: {}", !asc);
                }
            });
        }
        "balance_books" => dialogs::balance_dialog(|result| {
            if let Some(col) = result {
                app.core.status = format!("Balance col: {col}");
            }
        }),
        "about" => dialogs::show_about_dialog(),
        "help_keybinds" => dialogs::show_keybinds_help(),
        "rename_sheet" => dialogs::find_dialog(|result| {
            if let Some(name) = result {
                app.core.status = format!("Rename sheet to: {name}");
            }
        }),
        "undo" => {
            app.core.status = "Undo not yet implemented".into();
            state.canvas.queue_redraw();
        }
        "redo" => {
            app.core.status = "Redo not yet implemented".into();
            state.canvas.queue_redraw();
        }
        "cut" => {
            app.core.status = "Cut not yet implemented".into();
        }
        "copy" => {
            app.core.status = "Copy not yet implemented".into();
        }
        "paste" => {
            app.core.status = "Paste not yet implemented".into();
        }
        "delete_cell" => {
            handle_delete(state);
        }
        "select_all" => {
            app.core.status = "Select All".into();
            app.core.anchor = None;
            state.canvas.queue_redraw();
        }
        "toggle_headers" => {
            app.core.status = "Toggle headers not yet implemented".into();
        }
        "toggle_margins" => {
            app.core.status = "Toggle margins not yet implemented".into();
        }
        "new_sheet" => {
            app.core.status = "New sheet not yet implemented".into();
        }
        "delete_sheet" => {
            app.core.status = "Delete sheet not yet implemented".into();
        }
        "export_tsv" => {
            if let Some(path) = dialogs::file_save_dialog() {
                app.core.status = format!("Exporting TSV to {}", path.display());
            }
        }
        "export_csv" => {
            if let Some(path) = dialogs::file_save_dialog() {
                app.core.status = format!("Exporting CSV to {}", path.display());
            }
        }
        "export_ods" => {
            if let Some(path) = dialogs::file_save_dialog() {
                app.core.status = format!("Exporting ODS to {}", path.display());
            }
        }
        "export_ascii" => {
            if let Some(path) = dialogs::file_save_dialog() {
                app.core.status = format!("Exporting ASCII to {}", path.display());
            }
        }
        _ => {
            app.core.status = format!("Menu action: {name}");
        }
    }
    update_formula_bar(state, state.last_row.get(), state.last_col.get());
}

// ---------------------------------------------------------------------------
// Formula entry change callback
// ---------------------------------------------------------------------------

fn on_formula_entry_changed(state: &GuiState) {
    if !state.editing.get() {
        return;
    }
    if let Some(text) = state.formula_entry.get_text() {
        *state.edit_buf.borrow_mut() = text;
        state.canvas.queue_redraw();
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run_gui(corro_app: &mut super::App) -> Result<(), Box<dyn std::error::Error>> {
    let rxapp = rustxwidgets::App::init()
        .map_err(|e| format!("GUI init failed: {e}"))?;

    let win = rxapp.new_window()?;
    win.set_title(&format!("corro {}", env!("CARGO_PKG_VERSION")));
    win.set_default_size(1200, 800);

    let mut vbox = rxapp.new_box(Orientation::Vertical, 0)?;

    // Fit column widths to rendered content
    corro_app.fit_main_columns_to_max_width();

    let hr = HEADER_ROWS;
    let lm = MARGIN_COLS;
    let cursor_row = hr;
    let cursor_col = lm;
    corro_app.core.cursor.row = cursor_row;
    corro_app.core.cursor.col = cursor_col;
    corro_app.core.anchor = Some(SheetCursor { row: hr, col: lm });

    let _data_width = 200usize;
    let data_rows = 30usize;
    let data_cols = 12usize;

    // Formula bar
    let mut formula_bar = rxapp.new_box(Orientation::Horizontal, 2)?;
    let addr_label = rxapp.new_label("A1")?;
    let f_label = rxapp.new_label("  fx  ")?;
    let formula_entry = rxapp.new_entry()?;
    formula_entry.set_hexpand(true);
    formula_bar.append(&addr_label);
    formula_bar.append(&f_label);
    formula_bar.append(&formula_entry);
    formula_bar.set_child_hexpand(&formula_entry, true);

    // Canvas
    let canvas = rxapp.new_canvas()?;
    canvas.set_size_request(800, 600);

    // Status label
    let status_label = rxapp.new_label("Ready")?;

    let shared = Rc::new(GuiState {
        app: corro_app as *mut super::App,
        rxapp: rxapp.clone(),
        canvas: canvas.clone(),
        formula_entry: formula_entry.clone(),
        addr_label: addr_label.clone(),
        status_label: status_label.clone(),
        editing: Cell::new(false),
        edit_buf: RefCell::new(String::new()),
        mode: Cell::new(GuiMode::Normal),
        last_row: Cell::new(cursor_row),
        last_col: Cell::new(cursor_col),
        data_rows: Cell::new(data_rows),
        data_cols: Cell::new(data_cols),
        last_key: Cell::new(0),
        key_counter: Cell::new(0),
    });

    // Build menu
    let menubar = build_menu(&rxapp, &win, &shared)?;
    vbox.append(&menubar);

    // Draw callback
    let shared_draw = shared.clone();
    canvas.set_draw_callback(Box::new(move |dc: &mut dyn DrawContext, w: i32, h: i32| {
        render_grid(dc, &shared_draw, w, h);
    }));

    // Keyboard
    let shared_key = shared.clone();
    canvas.on_key(Box::new(move |keyval: u32| -> bool {
        handle_key(keyval, &shared_key)
    }));

    // Click
    let shared_click = shared.clone();
    canvas.on_click(Box::new(move |x: f64, y: f64| {
        handle_click(x, y, &shared_click);
    }));

    // Formula entry change
    let shared_entry = shared.clone();
    formula_entry.connect_changed(move || {
        on_formula_entry_changed(&shared_entry);
    })?;

    // Intercept Enter/Escape from formula entry during editing
    #[cfg(unix)]
    {
        use gtk_dynamic_loader::prelude::*;
        let entry_hwnd = formula_entry.raw_handle() as *mut gtk_dynamic_loader::GtkWidget;
        if !entry_hwnd.is_null() {
            let ctrl = gtk_dynamic_loader::EventControllerKey::new();
            ctrl.set_propagation_phase_capture();
            let state_k = shared.clone();
            ctrl.connect_key_pressed(move |_ctrl, keyval, _code, _state| {
                if handle_key(keyval, &state_k) { 1 } else { 0 }
            });
            ctrl.add_to_widget(unsafe { &*entry_hwnd });
        }
    }
    #[cfg(windows)]
    {
        let shared_k = shared.clone();
        let shared_k2 = shared.clone();
        formula_entry.on_key(Box::new(move |keyval: u32| -> bool {
            shared_k2.key_counter.set(shared_k2.key_counter.get() + 1);
            shared_k2.last_key.set(keyval);
            let masked = keyval & 0xFF;
            if masked == RETURN || masked == ESCAPE {
                handle_key(keyval, &shared_k);
                true
            } else {
                false
            }
        }));

        // WM_CHAR for Enter/Escape is consumed in the NWG adapter's _key_handler
        // to prevent the Edit control from beeping. The on_key callback above handles
        // WM_KEYDOWN for Enter/Escape by calling handle_key and returning true (consumed).
    }

    // Assemble layout
    vbox.append(&formula_bar);
    vbox.append(&canvas);
    vbox.set_child_vexpand(&canvas, true);
    vbox.append(&status_label);

    win.set_child_box(&vbox);
    win.present();

    // Start editing at A1 immediately so typing goes into the cell
    start_edit(&shared);

    rxapp.run()?;
    Ok(())
}
