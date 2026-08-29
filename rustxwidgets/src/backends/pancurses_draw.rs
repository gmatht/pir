//! Pancurses (terminal cell-grid) `DrawContext` for the shared spreadsheet model.
//!
//! The pancurses backend drives a real terminal by emitting SGR/ANSI escapes,
//! i.e. it is conceptually a character cell grid. This module provides a
//! `DrawContext` that paints into exactly such a grid, so `Spreadsheet::paint`
//! (the same pixel-based paint used by GTK and ratatui) renders identically here.
//! The pixel→cell mapping reuses `SpreadsheetModel::CHAR_W`/`ROW_H`, matching
//! [`crate::backends::ratatui`], so backend parity holds by construction.
//!
//! It is gated behind the `pancurses` feature purely for organisational
//! clarity; it has no hard dependency on the `pancurses` crate (it renders into
//! an in-memory [`CellGrid`] that a host can blit to a real `pancurses::Window`
//! or emit as SGR).

use crate::core::DrawContext;
use crate::spreadsheet::SpreadsheetModel;

const CELL_W: f64 = SpreadsheetModel::CHAR_W;
const CELL_H: f64 = SpreadsheetModel::ROW_H;

/// One terminal cell: a glyph plus resolved foreground/background RGB.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GridCell {
    pub ch: char,
    pub fg: (u8, u8, u8),
    pub bg: (u8, u8, u8),
}

/// A rectangular character cell grid (the pancurses "window").
pub struct CellGrid {
    pub w: u16,
    pub h: u16,
    pub cells: Vec<Vec<GridCell>>,
}

impl CellGrid {
    pub fn new(w: u16, h: u16) -> Self {
        CellGrid {
            w,
            h,
            cells: vec![vec![GridCell::default(); w as usize]; h as usize],
        }
    }

    /// Join each row into a string (for parity assertions).
    pub fn row_strings(&self) -> Vec<String> {
        self.cells.iter().map(|r| r.iter().map(|c| c.ch).collect()).collect()
    }

    fn cell_mut(&mut self, x: u16, y: u16) -> Option<&mut GridCell> {
        self.cells.get_mut(y as usize).and_then(|row| row.get_mut(x as usize))
    }

    fn set_bg(&mut self, area: ratatui_like_rect::Rect, bg: (u8, u8, u8)) {
        for y in area.top..area.bottom {
            for x in area.left..area.right {
                if let Some(c) = self.cell_mut(x, y) {
                    c.bg = bg;
                }
            }
        }
    }
}

mod ratatui_like_rect {
    #[derive(Clone, Copy)]
    pub struct Rect {
        pub left: u16,
        pub top: u16,
        pub right: u16,
        pub bottom: u16,
    }
    impl Rect {
        pub fn new(left: u16, top: u16, right: u16, bottom: u16) -> Self {
            Rect { left, top, right, bottom }
        }
    }
}

fn to_rgb(r: f64, g: f64, b: f64) -> (u8, u8, u8) {
    let c = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    (c(r), c(g), c(b))
}

fn to_area(grid: &CellGrid, x: f64, y: f64, w: f64, h: f64) -> ratatui_like_rect::Rect {
    let x0 = (x / CELL_W).floor().clamp(0.0, grid.w as f64) as u16;
    let y0 = (y / CELL_H).floor().clamp(0.0, grid.h as f64) as u16;
    let x1 = ((x + w) / CELL_W).ceil().clamp(0.0, grid.w as f64) as u16;
    let y1 = ((y + h) / CELL_H).ceil().clamp(0.0, grid.h as f64) as u16;
    let ww = x1.saturating_sub(x0);
    let hh = y1.saturating_sub(y0);
    if ww == 0 || hh == 0 {
        ratatui_like_rect::Rect::new(0, 0, 0, 0)
    } else {
        ratatui_like_rect::Rect::new(x0, y0, x1, y1)
    }
}

/// A [`DrawContext`] that paints into a [`CellGrid`] (pancurses terminal model).
pub struct PancursesDrawContext<'a> {
    grid: &'a mut CellGrid,
}

impl<'a> PancursesDrawContext<'a> {
    pub fn new(grid: &'a mut CellGrid) -> Self {
        PancursesDrawContext { grid }
    }
}

impl<'a> DrawContext for PancursesDrawContext<'a> {
    fn clear(&mut self, r: f64, g: f64, b: f64, _a: f64) {
        let bg = to_rgb(r, g, b);
        for row in self.grid.cells.iter_mut() {
            for cell in row.iter_mut() {
                *cell = GridCell { ch: ' ', fg: (0, 0, 0), bg };
            }
        }
    }

    fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, g: f64, b: f64, _a: f64) {
        let area = to_area(self.grid, x, y, w, h);
        self.grid.set_bg(area, to_rgb(r, g, b));
    }

    fn stroke_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, g: f64, b: f64, _a: f64, _lw: f64) {
        let area = to_area(self.grid, x, y, w, h);
        let color = to_rgb(r, g, b);
        let left = area.left;
        let right = area.right.saturating_sub(1);
        let top = area.top;
        let bottom = area.bottom.saturating_sub(1);
        for cx in left..=right {
            if let Some(c) = self.grid.cell_mut(cx, top) {
                if c.ch == ' ' {
                    c.ch = '─';
                    c.fg = color;
                }
            }
            if bottom != top {
                if let Some(c) = self.grid.cell_mut(cx, bottom) {
                    if c.ch == ' ' {
                        c.ch = '─';
                        c.fg = color;
                    }
                }
            }
        }
        for cy in top..=bottom {
            if let Some(c) = self.grid.cell_mut(left, cy) {
                if c.ch == ' ' {
                    c.ch = '│';
                    c.fg = color;
                }
            }
            if right != left {
                if let Some(c) = self.grid.cell_mut(right, cy) {
                    if c.ch == ' ' {
                        c.ch = '│';
                        c.fg = color;
                    }
                }
            }
        }
        if let Some(c) = self.grid.cell_mut(left, top) {
            if c.ch == ' ' {
                c.ch = '┌';
                c.fg = color;
            }
        }
        if right != left {
            if let Some(c) = self.grid.cell_mut(right, top) {
                if c.ch == ' ' {
                    c.ch = '┐';
                    c.fg = color;
                }
            }
        }
        if bottom != top {
            if let Some(c) = self.grid.cell_mut(left, bottom) {
                if c.ch == ' ' {
                    c.ch = '└';
                    c.fg = color;
                }
            }
            if right != left {
                if let Some(c) = self.grid.cell_mut(right, bottom) {
                    if c.ch == ' ' {
                        c.ch = '┘';
                        c.fg = color;
                    }
                }
            }
        }
    }

    fn draw_text_styled(&mut self, x: f64, y: f64, text: &str, _f: &str, _s: f64, r: f64, g: f64, b: f64, _a: f64, _sl: i32, _w: i32) {
        let col = ((x / CELL_W).floor() + 1.0).clamp(0.0, self.grid.w as f64) as u16;
        let row = (y / CELL_H).floor().clamp(0.0, self.grid.h as f64) as u16;
        let color = to_rgb(r, g, b);
        let mut cx = col;
        for ch in text.chars() {
            if cx >= self.grid.w {
                break;
            }
            if let Some(cell) = self.grid.cell_mut(cx, row) {
                cell.ch = ch;
                cell.fg = color;
            }
            cx += 1;
        }
    }

    fn text_extents_styled(&self, text: &str, _f: &str, _s: f64, _sl: i32, _w: i32) -> (f64, f64, f64, f64) {
        (0.0, 0.0, text.chars().count() as f64 * CELL_W, CELL_H)
    }

    fn save(&mut self) {}
    fn restore(&mut self) {}
    fn clip(&mut self, _x: f64, _y: f64, _w: f64, _h: f64) {}
}

/// Render `model` through the shared `paint` into a fresh [`CellGrid`] (pancurses model).
pub fn render_model_to_grid(model: &SpreadsheetModel, w: u16, h: u16) -> CellGrid {
    let mut grid = CellGrid::new(w, h);
    let mut dc = PancursesDrawContext::new(&mut grid);
    crate::spreadsheet::paint(model, &mut dc, w as i32, h as i32);
    grid
}
