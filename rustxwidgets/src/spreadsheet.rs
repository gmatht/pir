//! Cross-platform Spreadsheet model + Canvas renderer.
//!
//! The model is backend-agnostic; rendering is done entirely through the
//! [`crate::core::DrawContext`] 2D API, so the same widget paints identically on
//! every backend that provides a `Canvas` (gtk3, gtk4, wasm, ...). The pancurses
//! backend keeps its own terminal renderer; this module is the pixel-based
//! counterpart used by the GUI backends' `Canvas` draw callback.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::core::DrawContext;

/// Cell display style bits (mirrors corro's `CellDisplayStyle::to_pancurses_style`).
pub mod style {
    pub const DEFAULT: u8 = 0;
    pub const CURSOR: u8 = 1;
    pub const AGGREGATE: u8 = 2;
    pub const FOOTER_AGGREGATE: u8 = 3;
    pub const SELECTED: u8 = 4;
    pub const ACTIVE_HEADER: u8 = 5;
    pub const INACTIVE_HEADER: u8 = 6;
}

pub type CursorMoveCb = Box<dyn FnMut(u32, u32)>;
pub type CommitEditCb = Box<dyn FnMut(u32, u32, String)>;

/// Global (backend-wide) callback registry, used by the free
/// `add_cursor_move_callback` / `add_commit_edit_callback` entry points so a
/// host app can observe navigation/edits without holding a Spreadsheet handle.
thread_local! {
    static GLOBAL_CURSOR_MOVE: RefCell<Vec<CursorMoveCb>> = RefCell::new(Vec::new());
    static GLOBAL_COMMIT_EDIT: RefCell<Vec<CommitEditCb>> = RefCell::new(Vec::new());
}

pub fn add_global_cursor_move_callback(f: CursorMoveCb) {
    GLOBAL_CURSOR_MOVE.with(|c| c.borrow_mut().push(f));
}
pub fn add_global_commit_edit_callback(f: CommitEditCb) {
    GLOBAL_COMMIT_EDIT.with(|c| c.borrow_mut().push(f));
}

fn fire_cursor_move(row: u32, col: u32) {
    let mut cbs = GLOBAL_CURSOR_MOVE.with(|c| std::mem::take(&mut *c.borrow_mut()));
    for cb in cbs.iter_mut() {
        cb(row, col);
    }
    GLOBAL_CURSOR_MOVE.with(|c| *c.borrow_mut() = cbs);
}
fn fire_commit_edit(row: u32, col: u32, text: String) {
    let mut cbs = GLOBAL_COMMIT_EDIT.with(|c| std::mem::take(&mut *c.borrow_mut()));
    for cb in cbs.iter_mut() {
        cb(row, col, text.clone());
    }
    GLOBAL_COMMIT_EDIT.with(|c| *c.borrow_mut() = cbs);
}

/// Backend-agnostic spreadsheet data model.
#[derive(Default)]
pub struct SpreadsheetModel {
    pub cells: HashMap<(u32, u32), String>,
    pub cell_styles: HashMap<(u32, u32), u8>,
    pub raw_cells: HashMap<(u32, u32), String>,
    pub cursor_row: u32,
    pub cursor_col: u32,
    pub anchor: Option<(u32, u32)>,
    /// Number of leading "margin" columns (left label/outlier area).
    pub margin_cols: u32,
    /// Number of main data columns.
    pub main_cols: u32,
    pub header_row_count: u32,
    pub main_row_count: u32,
    /// `(global_col, width_in_chars, title)` for each displayed column.
    pub column_layout: Vec<(u32, u32, String)>,
    /// `(row, label)` for the left margin labels.
    pub row_labels: Vec<(u32, String)>,
    pub menu_text: String,
    pub border_title: String,
    pub status_text: String,
    pub formula_bar_trailing: String,
    pub tab_titles: Vec<String>,
    pub tab_active: usize,
    pub editing: bool,
    pub edit_buf: String,
    pub edit_pos: usize,
    pub formula_bar_address: Option<String>,
    pub formula_bar_entry: Option<String>,
    pub cursor_move_callbacks: Vec<CursorMoveCb>,
    pub commit_edit_callbacks: Vec<CommitEditCb>,
}

impl SpreadsheetModel {
    pub fn new(rows: u32, cols: u32) -> Self {
        SpreadsheetModel {
            main_row_count: rows,
            main_cols: cols,
            margin_cols: 1,
            header_row_count: 1,
            column_layout: (0..cols).map(|c| (c, 12u32, format!("{}", c + 1))).collect(),
            ..Default::default()
        }
    }

    pub fn set_cell(&mut self, row: u32, col: u32, text: &str) {
        self.cells.insert((row, col), text.to_string());
    }
    pub fn set_raw_cell(&mut self, row: u32, col: u32, text: &str) {
        self.raw_cells.insert((row, col), text.to_string());
    }
    pub fn get_cell(&self, row: u32, col: u32) -> Option<String> {
        self.cells.get(&(row, col)).cloned()
    }
    pub fn set_cell_style(&mut self, row: u32, col: u32, s: u8) {
        self.cell_styles.insert((row, col), s);
    }
    pub fn set_cursor(&mut self, row: u32, col: u32) {
        self.cursor_row = row;
        self.cursor_col = col;
        let mut cbs = std::mem::take(&mut self.cursor_move_callbacks);
        for cb in cbs.iter_mut() {
            cb(row, col);
        }
        self.cursor_move_callbacks = cbs;
        fire_cursor_move(row, col);
    }
    pub fn cursor_position(&self) -> Option<(u32, u32)> {
        Some((self.cursor_row, self.cursor_col))
    }
    pub fn set_editing(&mut self, editing: bool, buf: &str, pos: usize) {
        self.editing = editing;
        self.edit_buf = buf.to_string();
        self.edit_pos = pos;
    }
    pub fn set_grid_config(&mut self, margin_cols: u32, main_cols: u32) {
        self.margin_cols = margin_cols;
        self.main_cols = main_cols;
    }
    pub fn set_row_counts(&mut self, header_rows: u32, main_rows: u32) {
        self.header_row_count = header_rows;
        self.main_row_count = main_rows;
    }
    pub fn set_column_layout(&mut self, layout: Vec<(u32, u32, String)>) {
        self.column_layout = layout;
    }
    pub fn set_row_labels(&mut self, labels: Vec<(u32, String)>) {
        self.row_labels = labels;
    }
    pub fn set_menu_text(&mut self, text: &str) {
        self.menu_text = text.to_string();
    }
    pub fn set_border_title(&mut self, text: &str) {
        self.border_title = text.to_string();
    }
    pub fn set_status_text(&mut self, text: &str) {
        self.status_text = text.to_string();
    }
    pub fn set_formula_bar_trailing(&mut self, text: &str) {
        self.formula_bar_trailing = text.to_string();
    }
    pub fn set_tab_data(&mut self, titles: &[String], active: usize) {
        self.tab_titles = titles.to_vec();
        self.tab_active = active;
    }
    pub fn set_formula_bar(&mut self, address: &str, entry: &str) {
        self.formula_bar_address = Some(address.to_string());
        self.formula_bar_entry = Some(entry.to_string());
    }
    pub fn commit_formula_bar(&mut self) {
        if let (Some(addr), Some(text)) = (self.formula_bar_address.clone(), self.formula_bar_entry.clone()) {
            // address like "B3" -> (row, col); fall back to cursor if unparseable
            if let Some((r, c)) = parse_address(&addr) {
                self.cells.insert((r, c), text.clone());
                let mut cbs = std::mem::take(&mut self.commit_edit_callbacks);
                for cb in cbs.iter_mut() {
                    cb(r, c, text.clone());
                }
                self.commit_edit_callbacks = cbs;
                fire_commit_edit(r, c, text);
            }
        }
    }
    pub fn add_cursor_move_callback(&mut self, f: CursorMoveCb) {
        self.cursor_move_callbacks.push(f);
    }
    pub fn add_commit_edit_callback(&mut self, f: CommitEditCb) {
        self.commit_edit_callbacks.push(f);
    }

    /// Pixel layout constants (kept in one place so backends agree).
    pub const CHAR_W: f64 = 8.0;
    pub const ROW_H: f64 = 22.0;
    pub const HEADER_H: f64 = 22.0;
    pub const ROW_LABEL_W: f64 = 64.0;

    /// X pixel offset of a given global column (after the left label area).
    fn col_x(&self, global_col: u32) -> f64 {
        let mut x = Self::ROW_LABEL_W;
        for &(gc, w, _) in &self.column_layout {
            if gc >= global_col {
                break;
            }
            x += w as f64 * Self::CHAR_W;
        }
        x
    }
    fn col_width(&self, global_col: u32) -> f64 {
        for &(gc, w, _) in &self.column_layout {
            if gc == global_col {
                return w as f64 * Self::CHAR_W;
            }
        }
        12.0 * Self::CHAR_W
    }
    fn col_title(&self, global_col: u32) -> String {
        for (gc, _, t) in &self.column_layout {
            if *gc == global_col {
                return t.clone();
            }
        }
        format!("{}", global_col + 1)
    }
}

fn parse_address(a: &str) -> Option<(u32, u32)> {
    let a = a.trim();
    let mut col_chars = String::new();
    let mut rest = a;
    let mut chars = a.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_alphabetic() {
            col_chars.push(c);
            rest = &a[col_chars.len()..];
            chars.next();
        } else {
            break;
        }
    }
    let col: u32 = col_chars
        .chars()
        .rev()
        .enumerate()
        .map(|(i, c)| {
            let v = (c.to_ascii_uppercase() as u32) - b'A' as u32 + 1;
            v * 26u32.pow(i as u32)
        })
        .sum::<u32>()
        .saturating_sub(1);
    let row: u32 = rest.trim().parse().ok()?;
    if row == 0 {
        return None;
    }
    Some((row - 1, col))
}

/// Paint the whole spreadsheet into `dc` using the cross-platform 2D API.
pub fn paint(model: &SpreadsheetModel, dc: &mut dyn DrawContext, _w: i32, _h: i32) {
    dc.clear(0.96, 0.96, 0.96, 1.0);

    let header_h = model.header_row_count as f64 * SpreadsheetModel::HEADER_H;
    let row_label_w = SpreadsheetModel::ROW_LABEL_W;

    // Total columns to draw = margin + main.
    let total_cols = model.margin_cols + model.main_cols;
    let total_rows = model.header_row_count + model.main_row_count;

    // ---- grid cells ----
    for r in 0..total_rows {
        let ry = header_h + r as f64 * SpreadsheetModel::ROW_H;
        let is_header_row = r < model.header_row_count;
        for c in 0..total_cols {
            let cx = if c < model.margin_cols {
                // margin / label column
                c as f64 * (row_label_w / model.margin_cols.max(1) as f64)
            } else {
                model.col_x(c)
            };
            let cw = if c < model.margin_cols {
                row_label_w / model.margin_cols.max(1) as f64
            } else {
                model.col_width(c)
            };

            let style = model.cell_styles.get(&(r, c)).copied().unwrap_or(style::DEFAULT);
            let is_cursor = r == model.cursor_row && c == model.cursor_col;

            // background
            let bg = bg_for(style, is_cursor, model.editing);
            dc.fill_rect(cx, ry, cw, SpreadsheetModel::ROW_H, bg.0, bg.1, bg.2, bg.3);

            // text
            let text = if is_header_row {
                model.col_title(c)
            } else if c < model.margin_cols {
                row_label_for(model, r)
            } else {
                model.cells.get(&(r, c)).cloned().unwrap_or_default()
            };
            if !text.is_empty() {
                let (fr, fg, fb) = fg_for(style);
                if style_bold(style) {
                    // Draw twice with a 1px offset to fake bold (no font weight API yet).
                    dc.draw_text(cx + 2.0, ry + 3.0, &text, "monospace", 13.0, fr, fg, fb, 1.0);
                    dc.draw_text(cx + 3.0, ry + 3.0, &text, "monospace", 13.0, fr, fg, fb, 1.0);
                } else {
                    dc.draw_text(cx + 2.0, ry + 3.0, &text, "monospace", 13.0, fr, fg, fb, 1.0);
                }
            }

            // cursor outline
            if is_cursor && !model.editing {
                dc.stroke_rect(cx, ry, cw, SpreadsheetModel::ROW_H, 0.0, 0.4, 0.85, 1.0, 2.0);
            }

            // grid line
            dc.stroke_rect(cx, ry, cw, SpreadsheetModel::ROW_H, 0.8, 0.8, 0.8, 1.0, 0.5);
        }
    }

    // ---- formula bar (top strip above the grid) ----
    let fb_y = 0.0;
    dc.fill_rect(0.0, fb_y, 4096.0, SpreadsheetModel::HEADER_H, 0.9, 0.92, 0.95, 1.0);
    let addr = model
        .formula_bar_address
        .clone()
        .or_else(|| Some(cell_addr(model.cursor_row, model.cursor_col)))
        .unwrap_or_default();
    let entry = if model.editing {
        model.edit_buf.clone()
    } else {
        model.formula_bar_entry.clone().unwrap_or_default()
    };
    let fb_text = format!("{}  {}", addr, entry);
    dc.draw_text(4.0, 4.0, &fb_text, "monospace", 13.0, 0.1, 0.1, 0.2, 1.0);
    if !model.formula_bar_trailing.is_empty() {
        dc.draw_text(
            row_label_w,
            4.0,
            &model.formula_bar_trailing,
            "monospace",
            13.0,
            0.3,
            0.3,
            0.3,
            1.0,
        );
    }

    // ---- tabs (bottom strip) ----
    if !model.tab_titles.is_empty() {
        let tab_h = SpreadsheetModel::HEADER_H;
        let ty = header_h + total_rows as f64 * SpreadsheetModel::ROW_H;
        dc.fill_rect(0.0, ty, 4096.0, tab_h, 0.92, 0.92, 0.92, 1.0);
        let mut tx = 4.0;
        for (i, t) in model.tab_titles.iter().enumerate() {
            let active = i == model.tab_active;
            let (r, g, b) = if active { (0.6, 0.75, 0.95) } else { (0.85, 0.85, 0.85) };
            let w = 80.0;
            dc.fill_rect(tx, ty + 2.0, w, tab_h - 4.0, r, g, b, 1.0);
            dc.draw_text(tx + 4.0, ty + 5.0, t, "monospace", 12.0, 0.0, 0.0, 0.0, 1.0);
            tx += w + 4.0;
        }
    }

    // ---- border title / status (left gutter bottom) ----
    if !model.border_title.is_empty() {
        dc.draw_text(4.0, header_h + 2.0, &model.border_title, "monospace", 12.0, 0.2, 0.2, 0.2, 1.0);
    }
    if !model.status_text.is_empty() {
        dc.draw_text(
            4.0,
            header_h + (total_rows as f64 + 1.0) * SpreadsheetModel::ROW_H,
            &model.status_text,
            "monospace",
            11.0,
            0.3,
            0.3,
            0.3,
            1.0,
        );
    }
}

fn row_label_for(model: &SpreadsheetModel, r: u32) -> String {
    for (rr, label) in &model.row_labels {
        if *rr == r {
            return label.clone();
        }
    }
    format!("{}", r + 1)
}

fn cell_addr(r: u32, c: u32) -> String {
    let mut s = String::new();
    let mut c = c;
    loop {
        let ch = (b'A' + (c % 26) as u8) as char;
        s.insert(0, ch);
        if c < 26 {
            break;
        }
        c = c / 26 - 1;
    }
    s + &format!("{}", r + 1)
}

fn style_bold(s: u8) -> bool {
    matches!(s, style::ACTIVE_HEADER | style::INACTIVE_HEADER | style::AGGREGATE | style::FOOTER_AGGREGATE)
}

fn bg_for(s: u8, is_cursor: bool, editing: bool) -> (f64, f64, f64, f64) {
    if is_cursor {
        if editing {
            (1.0, 1.0, 0.8, 1.0)
        } else {
            (0.8, 0.9, 1.0, 1.0)
        }
    } else if matches!(s, style::ACTIVE_HEADER | style::INACTIVE_HEADER) {
        (0.88, 0.9, 0.93, 1.0)
    } else {
        (1.0, 1.0, 1.0, 1.0)
    }
}

fn fg_for(s: u8) -> (f64, f64, f64) {
    match s {
        style::AGGREGATE | style::FOOTER_AGGREGATE => (0.4, 0.4, 0.4),
        _ => (0.05, 0.05, 0.1),
    }
}

/// Map a pixel coordinate (from a click) back to a cell, if any.
pub fn cell_at(model: &SpreadsheetModel, x: f64, y: f64) -> Option<(u32, u32)> {
    let header_h = model.header_row_count as f64 * SpreadsheetModel::HEADER_H;
    if y < header_h {
        return None;
    }
    let r = ((y - header_h) / SpreadsheetModel::ROW_H).floor() as u32;
    if r >= model.header_row_count + model.main_row_count {
        return None;
    }
    let total_cols = model.margin_cols + model.main_cols;
    let mut cx = SpreadsheetModel::ROW_LABEL_W;
    for c in 0..total_cols {
        let cw = if c < model.margin_cols {
            SpreadsheetModel::ROW_LABEL_W / model.margin_cols.max(1) as f64
        } else {
            model.col_width(c)
        };
        if x >= cx && x < cx + cw {
            return Some((r, c));
        }
        cx += cw;
    }
    None
}

/// Build a shareable `SpreadsheetModel` handle for a backend `Spreadsheet`.
pub type SharedModel = Rc<RefCell<SpreadsheetModel>>;

/// Construct a fresh shared model handle.
pub fn new_shared_model(rows: u32, cols: u32) -> SharedModel {
    Rc::new(RefCell::new(SpreadsheetModel::new(rows, cols)))
}
