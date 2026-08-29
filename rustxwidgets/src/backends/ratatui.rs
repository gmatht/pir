//! Ratatui backend for rustxWidgets.
//!
//! This makes `rustxwidgets` usable as a real terminal GUI backend: `App::init()`
//! returns a `RatatuiApp` that runs a crossterm event loop, and
//! [`RatatuiDrawContext`] implements [`crate::core::DrawContext`] by mapping the
//! pixel-based paint coordinate space onto a terminal cell grid. Because the
//! shared widgets (`Spreadsheet::paint`, etc.) only use `clear`/`fill_rect`/
//! `draw_text`/`stroke_rect`, a cell-grid backend is enough to render them — the
//! `CHAR_W`/`ROW_H` constants from `SpreadsheetModel` are reused so the mapping
//! agrees with the GTK/cairo backends.

use std::error::Error as StdError;

use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::prelude::*;
use ratatui::Frame;

use crate::backends::BackendApp;
use crate::core::DrawContext;
use crate::spreadsheet::SpreadsheetModel;

/// Pixel width assumed for one terminal column (matches `SpreadsheetModel::CHAR_W`).
const CELL_W: f64 = SpreadsheetModel::CHAR_W;
/// Pixel height assumed for one terminal row (matches `SpreadsheetModel::ROW_H`).
const CELL_H: f64 = SpreadsheetModel::ROW_H;

fn to_color(r: f64, g: f64, b: f64) -> Color {
    let c = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color::Rgb(c(r), c(g), c(b))
}

/// Map a pixel-space rect onto a terminal `Rect`, clamped to `area`.
fn to_area(area: Rect, x: f64, y: f64, w: f64, h: f64) -> Rect {
    let x0 = (x / CELL_W).floor().clamp(0.0, area.width as f64) as u16;
    let y0 = (y / CELL_H).floor().clamp(0.0, area.height as f64) as u16;
    let x1 = ((x + w) / CELL_W).ceil().clamp(0.0, area.width as f64) as u16;
    let y1 = ((y + h) / CELL_H).ceil().clamp(0.0, area.height as f64) as u16;
    let ww = x1.saturating_sub(x0);
    let hh = y1.saturating_sub(y0);
    if ww == 0 || hh == 0 {
        Rect::default()
    } else {
        Rect::new(x0, y0, ww, hh)
    }
}

/// A [`DrawContext`] that paints into a `ratatui::Frame` (terminal cell grid).
pub struct RatatuiDrawContext<'a, 'b> {
    frame: &'b mut Frame<'a>,
}

impl<'a, 'b> RatatuiDrawContext<'a, 'b> {
    pub fn new(frame: &'b mut Frame<'a>) -> Self {
        RatatuiDrawContext { frame }
    }
}

impl<'a, 'b> DrawContext for RatatuiDrawContext<'a, 'b> {
    fn clear(&mut self, r: f64, g: f64, b: f64, _a: f64) {
        let buf = self.frame.buffer_mut();
        buf.reset();
        buf.set_style(*buf.area(), Style::default().bg(to_color(r, g, b)));
    }

    fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, g: f64, b: f64, _a: f64) {
        let area = to_area(self.frame.area(), x, y, w, h);
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.frame.buffer_mut().set_style(area, Style::default().bg(to_color(r, g, b)));
    }

    fn stroke_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, g: f64, b: f64, _a: f64, _lw: f64) {
        let area = to_area(self.frame.area(), x, y, w, h);
        if area.width == 0 || area.height == 0 {
            return;
        }
        // In a terminal each spreadsheet row is only ~1 cell tall, so the border
        // outline would otherwise sit on the same row as the cell text. Only paint
        // a border glyph where the cell is still empty, so the text survives.
        let color = to_color(r, g, b);
        let buf = self.frame.buffer_mut();
        let left = area.left();
        let right = area.right().saturating_sub(1);
        let top = area.top();
        let bottom = area.bottom().saturating_sub(1);
        for cx in left..=right {
            if let Some(cell) = buf.cell_mut((cx, top)) {
                if cell.symbol() == " " {
                    cell.set_char('─');
                    cell.fg = color;
                }
            }
            if bottom != top {
                if let Some(cell) = buf.cell_mut((cx, bottom)) {
                    if cell.symbol() == " " {
                        cell.set_char('─');
                        cell.fg = color;
                    }
                }
            }
        }
        for cy in top..=bottom {
            if let Some(cell) = buf.cell_mut((left, cy)) {
                if cell.symbol() == " " {
                    cell.set_char('│');
                    cell.fg = color;
                }
            }
            if right != left {
                if let Some(cell) = buf.cell_mut((right, cy)) {
                    if cell.symbol() == " " {
                        cell.set_char('│');
                        cell.fg = color;
                    }
                }
            }
        }
        if let Some(cell) = buf.cell_mut((left, top)) {
            if cell.symbol() == " " {
                cell.set_char('┌');
            }
        }
        if right != left {
            if let Some(cell) = buf.cell_mut((right, top)) {
                if cell.symbol() == " " {
                    cell.set_char('┐');
                }
            }
        }
        if bottom != top {
            if let Some(cell) = buf.cell_mut((left, bottom)) {
                if cell.symbol() == " " {
                    cell.set_char('└');
                }
            }
            if right != left {
                if let Some(cell) = buf.cell_mut((right, bottom)) {
                    if cell.symbol() == " " {
                        cell.set_char('┘');
                    }
                }
            }
        }
    }

    fn draw_text_styled(&mut self, x: f64, y: f64, text: &str, _f: &str, _s: f64, r: f64, g: f64, b: f64, _a: f64, _sl: i32, _w: i32) {
        let area = self.frame.area();
        // Skip the 1-cell left border / 2px padding so text lands in the interior.
        let col = ((x / CELL_W).floor() + 1.0).clamp(0.0, area.width as f64) as u16;
        let row = (y / CELL_H).floor().clamp(0.0, area.height as f64) as u16;
        if col >= area.width || row >= area.height {
            return;
        }
        let color = to_color(r, g, b);
        let buf = self.frame.buffer_mut();
        let mut cx = col;
        for ch in text.chars() {
            if cx >= area.width {
                break;
            }
            if let Some(cell) = buf.cell_mut((cx, row)) {
                cell.set_char(ch);
                cell.fg = color;
            }
            cx += 1;
        }
    }

    fn text_extents_styled(&self, text: &str, _f: &str, _s: f64, _sl: i32, _w: i32) -> (f64, f64, f64, f64) {
        let w: f64 = text.chars().map(|_| CELL_W).sum();
        (0.0, 0.0, w, CELL_H)
    }

    fn save(&mut self) {}
    fn restore(&mut self) {}
    fn clip(&mut self, _x: f64, _y: f64, _w: f64, _h: f64) {}
}

fn bx<E: std::fmt::Display>(e: E) -> Box<dyn StdError + Send + Sync> {
    Box::new(std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
}

/// Render an arbitrary spreadsheet model to a ratatui `Buffer` using a
/// `TestBackend`. Pure/test-only helper used by parity tests; it exercises the
/// exact same `paint` path the real terminal app uses.
pub fn render_model_to_test_backend(model: &SpreadsheetModel, w: u16, h: u16) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            let mut dc = RatatuiDrawContext::new(f);
            crate::spreadsheet::paint(model, &mut dc, w as i32, h as i32);
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

/// A small, representative spreadsheet model used by the demo loop and tests.
pub fn demo_model() -> SpreadsheetModel {
    let mut m = SpreadsheetModel::new(6, 4);
    m.set_border_title("rustxWidgets ⚡ ratatui");
    m.set_menu_text("File  Edit  View");
    m.set_status_text("demo");
    m.set_formula_bar("A1", "=1+1");
    m.set_tab_data(&["Sheet1".to_string(), "Sheet2".to_string()], 0);
    m.set_cell(1, 1, "Hello");
    m.set_cell(2, 2, "World");
    m.set_cell(3, 3, "=SUM(A1:A3)");
    m.set_cell_style(1, 1, crate::spreadsheet::style::CURSOR);
    m.set_cursor(1, 1);
    m
}

/// The ratatui terminal backend application.
pub struct RatatuiApp {
    model: Option<SpreadsheetModel>,
}

impl RatatuiApp {
    /// Initialize the ratatui backend with a specific model (instead of the demo).
    pub fn init_with_model(
        model: SpreadsheetModel,
    ) -> Result<Box<dyn BackendApp>, Box<dyn StdError + Send + Sync>> {
        Ok(Box::new(RatatuiApp { model: Some(model) }))
    }
}

impl BackendApp for RatatuiApp {
    fn run(self: Box<Self>) -> Result<(), Box<dyn StdError + Send + Sync>> {
        use crossterm::event::{self, Event, KeyCode};
        use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
        use crossterm::ExecutableCommand;
        use std::io::stdout;

        terminal::enable_raw_mode().map_err(bx)?;
        stdout().execute(EnterAlternateScreen).map_err(bx)?;
        let mut terminal = Terminal::new(CrosstermBackend::new(stdout())).map_err(bx)?;
        let mut model = self.model.unwrap_or_else(demo_model);

        let result = (|| -> Result<(), Box<dyn StdError + Send + Sync>> {
            loop {
                terminal
                    .draw(|f| {
                        let (w, h) = (f.area().width, f.area().height);
                        let mut dc = RatatuiDrawContext::new(f);
                        crate::spreadsheet::paint(&model, &mut dc, w as i32, h as i32);
                    })
                    .map_err(bx)?;
                if event::poll(std::time::Duration::from_millis(100)).map_err(bx)? {
                    if let Event::Key(key) = event::read().map_err(bx)? {
                        match key.code {
                            KeyCode::Char('q') => break,
                            KeyCode::Char('j') => model.set_cursor(model.cursor_row + 1, model.cursor_col),
                            KeyCode::Char('k') => model.set_cursor(model.cursor_row.saturating_sub(1), model.cursor_col),
                            KeyCode::Char('l') => model.set_cursor(model.cursor_row, model.cursor_col + 1),
                            KeyCode::Char('h') => model.set_cursor(model.cursor_row, model.cursor_col.saturating_sub(1)),
                            _ => {}
                        }
                    }
                }
            }
            Ok(())
        })();

        let _ = terminal::disable_raw_mode();
        let _ = stdout().execute(LeaveAlternateScreen);
        result
    }
}

/// Render the demo model to a ratatui `Buffer` (pure/test helper).
pub fn render_demo_to_test_backend(w: u16, h: u16) -> ratatui::buffer::Buffer {
    render_model_to_test_backend(&demo_model(), w, h)
}

#[cfg(all(feature = "ratatui", not(any(feature = "gtk", feature = "gtk4-rs", target_os = "windows", target_arch = "wasm32", target_os = "android", feature = "pancurses", feature = "zork"))))]
pub fn init() -> Result<Box<dyn BackendApp>, Box<dyn StdError + Send + Sync>> {
    Ok(Box::new(RatatuiApp { model: None }))
}
