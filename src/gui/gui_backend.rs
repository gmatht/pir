use rustxwidgets::prelude::*;
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
    pub const ALT_L: u32 = 0xFFE9;
    pub const ALT_R: u32 = 0xFFEA;
}

#[cfg(all(feature = "gui", windows))]
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
    pub const ALT_L: u32 = 0x12;
    pub const ALT_R: u32 = 0x12;
}

#[cfg(all(feature = "gui", target_family = "unix"))]
use key::*;

#[cfg(all(feature = "gui", windows))]
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

#[derive(Clone, Copy, PartialEq)]
enum MenuNavState {
    Inactive,
    File,
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
    alt_active: Cell<bool>,
    seq_alt_f: Cell<bool>,
    last_was_f: Cell<bool>,
    prev_key: Cell<u32>,
    menu_nav: Cell<MenuNavState>,
    alt_f_detected: Cell<bool>,
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
    let prev_key = state.prev_key.get();
    state.prev_key.set(keyval);
    let app = unsafe { &mut *state.app };
    let key = {
        #[cfg(windows)]
        { keyval & 0xFF }
        #[cfg(not(windows))]
        { keyval }
    };

    // Gate: if this is 'q' and Alt-F+Q flags are set, quit immediately
    // before any other processing.  The flags may have been set by a
    // window-level event handler that already processed 'f'.
    let ch0 = char::from_u32(key).unwrap_or('\0').to_ascii_lowercase();
    if ch0 == 'q'
        && (state.menu_nav.get() == MenuNavState::File
            || state.seq_alt_f.get()
            || state.last_was_f.get())
    {
        state.menu_nav.set(MenuNavState::Inactive);
        state.alt_active.set(false);
        state.seq_alt_f.set(false);
        state.last_was_f.set(false);
        #[cfg(unix)]
        let _ = rustxwidgets::backends_gtk_adapter::quit_main_loop();
        #[cfg(windows)]
        rustxwidgets::backends_nwg_adapter::quit_main_loop();
        return true;
    }

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

    if key == ALT_L || key == ALT_R {
        state.alt_active.set(true);
        return true;
    }

    let ch = char::from_u32(key).unwrap_or('\0').to_ascii_lowercase();

    // --- Alt-F+Q two-key sequence detector ---
    if ch == 'q' && (prev_key == 0x66 || prev_key == 0x46) {
        if state.editing.get() && !state.edit_buf.borrow().is_empty() {
            // genuine text input ("f" then "q") — do not quit
            state.menu_nav.set(MenuNavState::Inactive);
            state.alt_active.set(false);
            state.seq_alt_f.set(false);
            state.last_was_f.set(false);
        } else {
            state.menu_nav.set(MenuNavState::Inactive);
            state.alt_active.set(false);
            state.seq_alt_f.set(false);
            state.last_was_f.set(false);
            #[cfg(unix)]
            let _ = rustxwidgets::backends_gtk_adapter::quit_main_loop();
            #[cfg(windows)]
            rustxwidgets::backends_nwg_adapter::quit_main_loop();
            return true;
        }
    }

    // Fallback 'q' checks via menu_nav / seq_alt_f / last_was_f
    if ch == 'q'
        && (state.menu_nav.get() == MenuNavState::File
            || state.seq_alt_f.get()
            || state.last_was_f.get())
    {
        state.last_was_f.set(false);
        if !state.seq_alt_f.get()
            && state.menu_nav.get() == MenuNavState::Inactive
            && state.editing.get()
            && !state.edit_buf.borrow().is_empty()
        {
            state.menu_nav.set(MenuNavState::Inactive);
            state.alt_active.set(false);
            state.seq_alt_f.set(false);
            state.last_was_f.set(false);
        } else {
            state.menu_nav.set(MenuNavState::Inactive);
            state.alt_active.set(false);
            state.seq_alt_f.set(false);
            state.last_was_f.set(false);
            #[cfg(unix)]
            let _ = rustxwidgets::backends_gtk_adapter::quit_main_loop();
            #[cfg(windows)]
            rustxwidgets::backends_nwg_adapter::quit_main_loop();
            return true;
        }
    }

    // 'f' handler — set menu_nav so subsequent 'q' can quit
    if ch == 'f' {
        state.last_was_f.set(true);
        state.menu_nav.set(MenuNavState::File);
        state.seq_alt_f.set(true);
        state.alt_active.set(false);
        return true;
    }

    // Reset menu-nav state on any non-'f' non-'q' key
    if state.menu_nav.get() != MenuNavState::Inactive
        || state.seq_alt_f.get()
        || state.last_was_f.get()
    {
        if ch != 'q' {
            state.menu_nav.set(MenuNavState::Inactive);
            state.alt_active.set(false);
            state.seq_alt_f.set(false);
            state.last_was_f.set(false);
        }
    }

    if state.editing.get() {
        return handle_edit_key(key, state);
    }

    state.seq_alt_f.set(false);
    state.last_was_f.set(false);

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
            state.menu_nav.set(MenuNavState::Inactive);
            state.alt_active.set(false);
            state.seq_alt_f.set(false);
            state.last_was_f.set(false);
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
    let ch = char::from_u32(key).unwrap_or('\0').to_ascii_lowercase();
    if ch == 'q'
        && (state.seq_alt_f.get()
            || state.last_was_f.get()
            || state.menu_nav.get() == MenuNavState::File)
    {
        if !state.seq_alt_f.get()
            && state.menu_nav.get() == MenuNavState::Inactive
            && !state.edit_buf.borrow().is_empty()
        {
            state.menu_nav.set(MenuNavState::Inactive);
            state.alt_active.set(false);
            state.seq_alt_f.set(false);
            state.last_was_f.set(false);
        } else {
            state.menu_nav.set(MenuNavState::Inactive);
            state.alt_active.set(false);
            state.seq_alt_f.set(false);
            state.last_was_f.set(false);
            #[cfg(unix)]
            let _ = rustxwidgets::backends_gtk_adapter::quit_main_loop();
            #[cfg(windows)]
            rustxwidgets::backends_nwg_adapter::quit_main_loop();
            return true;
        }
    }
    if ch != 'f' {
        state.seq_alt_f.set(false);
        state.last_was_f.set(false);
    }
    match key {
        RETURN | 0x0D => {
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
    state.edit_buf.borrow_mut().clear();
    state.canvas.queue_redraw();

    // Return keyboard focus to the canvas so subsequent Alt-F+Q key
    // sequences are handled by the canvas key controller rather than
    // the formula entry, where GTK's internal mnemonic monitor may
    // intercept the Alt modifier before our controller can process it.
    #[cfg(unix)]
    focus_canvas(state);
}

#[cfg(unix)]
fn focus_canvas(state: &GuiState) {
    if let Some(loader) = rustxwidgets::backends::gtk::loader() {
        unsafe {
            if let Some(set_can_focus) = loader.symbols.gtk_widget_set_can_focus {
                set_can_focus(state.canvas.raw_handle(), 1);
            }
            if let Some(grab) = loader.symbols.gtk_widget_grab_focus {
                grab(state.canvas.raw_handle());
            }
        }
    }
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

    let mut menubar_model = rxapp.create_menu()?;
    // Prefix labels with U+3164 (Hangul Filler) to prevent GTK4's
    // GtkPopoverMenuBar from auto-assigning mnemonic accelerators
    // (Alt+F, Alt+E, etc.).
    menubar_model.append_submenu("\u{3164}File", &file_menu);
    menubar_model.append_submenu("\u{3164}Edit", &edit_menu);
    menubar_model.append_submenu("\u{3164}View", &view_menu);
    menubar_model.append_submenu("\u{3164}Sheet", &sheet_menu);
    menubar_model.append_submenu("\u{3164}Data", &data_menu);
    menubar_model.append_submenu("\u{3164}Help", &help_menu);

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

    let menubar = unsafe { rxapp.create_menubar(&menubar_model, action_group)? };
    unsafe { win.insert_action_group("app", action_group); }

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

    let win = rxapp.create_window()?;
    win.set_title(&format!("corro {}", env!("CARGO_PKG_VERSION")));
    win.set_default_size(1200, 800);

    let mut vbox = rxapp.create_box(Orientation::Vertical, 0)?;

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
    let mut formula_bar = rxapp.create_box(Orientation::Horizontal, 2)?;
    let addr_label = rxapp.create_label("A1")?;
    let f_label = rxapp.create_label("  fx  ")?;
    let formula_entry = rxapp.create_entry()?;
    formula_entry.set_hexpand(true);
    formula_bar.append(&addr_label);
    formula_bar.append(&f_label);
    formula_bar.append(&formula_entry);
    formula_entry.set_hexpand(true);

    // Canvas
    let canvas = rxapp.create_canvas()?;
    canvas.set_size_request(800, 600);
    // Ensure the canvas can receive keyboard focus (needed for focus_canvas
    // to succeed after commit_edit — GtkDrawingArea does not accept focus
    // by default).
    #[cfg(unix)]
    if let Some(loader) = rustxwidgets::backends::gtk::loader() {
        unsafe {
            if let Some(set_can_focus) = loader.symbols.gtk_widget_set_can_focus {
                set_can_focus(canvas.raw_handle(), 1);
            }
        }
    }

    // Status label
    let status_label = rxapp.create_label("Ready")?;

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
        alt_active: Cell::new(false),
        seq_alt_f: Cell::new(false),
        last_was_f: Cell::new(false),
        prev_key: Cell::new(0),
        menu_nav: Cell::new(MenuNavState::Inactive),
        alt_f_detected: Cell::new(false),
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
    let alt_f_armed = Rc::new(Cell::new(false));
    canvas.on_key(Box::new(move |keyval: u32| -> bool {
        let ch = char::from_u32(keyval).unwrap_or('\0').to_ascii_lowercase();

        let s: &GuiState = &*shared_key;
        if ch == 'q'
            && (alt_f_armed.get()
                || s.seq_alt_f.get()
                || s.menu_nav.get() == MenuNavState::File)
        {
            alt_f_armed.set(false);
            s.seq_alt_f.set(false);
            s.menu_nav.set(MenuNavState::Inactive);
            s.last_was_f.set(false);
            #[cfg(unix)]
            let _ = rustxwidgets::backends_gtk_adapter::quit_main_loop();
            #[cfg(windows)]
            rustxwidgets::backends_nwg_adapter::quit_main_loop();
            return true;
        }

        if ch == 'f' {
            alt_f_armed.set(true);
            return handle_key(keyval, &shared_key);
        }

        if keyval == ALT_L || keyval == ALT_R {
            return handle_key(keyval, &shared_key);
        }

        alt_f_armed.set(false);
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

    // Window-level GTK event interception to catch Alt-F+Q before the
    // menu bar's mnemonic accelerator can steal the keystrokes.
    #[cfg(unix)]
    {
        if let Some(loader) = rustxwidgets::backends::gtk::loader() {
            let win_ptr = win.raw_handle();
            if !win_ptr.is_null() {
                let l_for_sig = loader.clone();
                let l_for_cb = loader.clone();
                let state_w = shared.clone();
                unsafe {
                    let _ = gtk_dynamic_loader::widget_connect_signal_bool(
                        &l_for_sig,
                        win_ptr,
                        "event",
                        Box::new(move |event: *mut std::ffi::c_void| -> i32 {
                            let s: &GuiState = &*state_w;
                            let keyval = l_for_cb.symbols.gdk_event_get_keyval
                                .map(|f| unsafe { f(event) })
                                .unwrap_or(0);
                            if keyval == 0 {
                                return 0;
                            }
                            let state = l_for_cb.symbols.gdk_event_get_state
                                .map(|f| unsafe { f(event) })
                                .unwrap_or(0);
                            let alt_held = (state & 0x8) != 0;
                            if alt_held || keyval == ALT_L || keyval == ALT_R {
                                s.alt_active.set(true);
                            }
                            if keyval == ALT_L || keyval == ALT_R {
                                return 1;
                            }
                            let ch = char::from_u32(keyval).unwrap_or('\0').to_ascii_lowercase();
                            if (state & 0x4) != 0 && ch == 'q' {
                                s.seq_alt_f.set(false);
                                let _ = rustxwidgets::backends_gtk_adapter::quit_main_loop();
                                return 1;
                            }
                            if (alt_held || s.alt_active.get()) && ch == 'f' {
                                s.menu_nav.set(MenuNavState::File);
                                s.seq_alt_f.set(true);
                                s.alt_active.set(false);
                                s.alt_f_detected.set(true);
                                return 1;
                            }
                            if !s.editing.get() && ch == 'f' {
                                s.menu_nav.set(MenuNavState::File);
                                s.seq_alt_f.set(true);
                                s.alt_active.set(false);
                                s.alt_f_detected.set(true);
                                return 1;
                            }
                            if ch == 'q'
                                && (s.menu_nav.get() == MenuNavState::File
                                    || s.seq_alt_f.get())
                            {
                                let _ = rustxwidgets::backends_gtk_adapter::quit_main_loop();
                                return 1;
                            }
                            if !alt_held {
                                s.alt_active.set(false);
                                s.seq_alt_f.set(false);
                            }
                            0
                        }),
                    );
                }
            }
        }
    }

    // Intercept Enter/Escape from formula entry during editing
    #[cfg(unix)]
    {
        if let Some(loader) = rustxwidgets::backends::gtk::loader() {
            let entry_ptr = formula_entry.raw_handle();
            if !entry_ptr.is_null() {
                let l2 = loader.clone();
                let l2_for_cb = l2.clone();
                let state_k = shared.clone();
                unsafe {
                    let _ = gtk_dynamic_loader::widget_connect_signal_bool(
                        &l2,
                        entry_ptr,
                        "event",
                        Box::new(move |ev: *mut std::ffi::c_void| -> i32 {
                            let keyval =
                                gtk_dynamic_loader::EventControllerKey::get_keyval_static(
                                    &l2_for_cb, ev,
                                );
                            if keyval == 0 {
                                return 0;
                            }
                            let state =
                                gtk_dynamic_loader::EventControllerKey::get_state_static(
                                    &l2_for_cb, ev,
                                );
                            let alt_held = (state & 0x8) != 0;
                            if alt_held || keyval == ALT_L || keyval == ALT_R {
                                state_k.alt_active.set(true);
                            }
                            if keyval == ALT_L || keyval == ALT_R {
                                return 1;
                            }
                            let ch =
                                char::from_u32(keyval).unwrap_or('\0').to_ascii_lowercase();
                            if (alt_held || state_k.alt_active.get()) && ch == 'f' {
                                state_k.last_was_f.set(true);
                                state_k.menu_nav.set(MenuNavState::File);
                                state_k.seq_alt_f.set(true);
                                state_k.alt_active.set(false);
                                state_k.alt_f_detected.set(true);
                                let _ = handle_key(keyval, &state_k);
                                return 1;
                            }
                            if !state_k.editing.get() && ch == 'f' {
                                state_k.last_was_f.set(true);
                                state_k.menu_nav.set(MenuNavState::File);
                                state_k.seq_alt_f.set(true);
                                state_k.alt_active.set(false);
                                state_k.alt_f_detected.set(true);
                                let _ = handle_key(keyval, &state_k);
                                return 1;
                            }
                            if ch == 'q'
                                && (state_k.menu_nav.get() == MenuNavState::File
                                    || state_k.seq_alt_f.get()
                                    || state_k.last_was_f.get())
                            {
                                if !state_k.seq_alt_f.get()
                                    && state_k.menu_nav.get() == MenuNavState::Inactive
                                    && state_k.editing.get()
                                    && !state_k.edit_buf.borrow().is_empty()
                                {
                                    state_k.menu_nav.set(MenuNavState::Inactive);
                                    state_k.alt_active.set(false);
                                    state_k.seq_alt_f.set(false);
                                } else {
                                    #[cfg(unix)]
                                    let _ = rustxwidgets::backends_gtk_adapter::quit_main_loop();
                                    #[cfg(windows)]
                                    rustxwidgets::backends_nwg_adapter::quit_main_loop();
                                    return 1;
                                }
                            }
                            if handle_key(keyval, &state_k) {
                                1
                            } else {
                                0
                            }
                        }),
                    );
                }
            }
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
    canvas.set_vexpand(true);
    vbox.append(&status_label);

    win.set_child(&vbox);
    win.present();

    // Start editing at A1 immediately so typing goes into the cell
    start_edit(&shared);

    rxapp.run()?;
    Ok(())
}
