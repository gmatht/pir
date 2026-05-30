use crate::widget::sheet::{Sheet, Row, Col};
use crate::widget::sheet::{index_to_col_label};
use std::rc::Rc;
use std::cell::RefCell;

use gtk_compat::{Loader, BoxWidget, Orientation, Entry, Grid, Label, DrawingArea};

/// A simple virtualized spreadsheet-like widget built from a small grid of Entry widgets.
/// It keeps a sparse Sheet model with huge logical coordinates but only allocates widgets for
/// a small visible viewport. Long text is split across adjacent blank visible cells (visual overflow)
/// using pixel measurements.
pub struct SpreadsheetWidget {
    pub sheet: Rc<RefCell<Sheet>>,
    loader: std::sync::Arc<Loader>,
    container: BoxWidget,
    grid: Grid,
    row_headers: Vec<Label>,
    col_headers: Vec<Label>,
    cells: Vec<Vec<Entry>>,
    viewport_rows: usize,
    viewport_cols: usize,
    top_row: Row,
    left_col: Col,
}

impl SpreadsheetWidget {
    pub fn new(loader: std::sync::Arc<Loader>, sheet: Sheet, viewport_rows: usize, viewport_cols: usize) -> Result<Self, Box<dyn std::error::Error>> {
        let container = BoxWidget::new(loader.clone(), Orientation::Vertical, 0)?;
        let grid = Grid::new(loader.clone())?;
        // create drawing area for canvas rendering
        let drawing = DrawingArea::new(loader.clone())?;

        // headers + cells
        let mut col_headers: Vec<Label> = Vec::new();
        let mut row_headers: Vec<Label> = Vec::new();
        let mut cells: Vec<Vec<Entry>> = Vec::new();

        // top-left corner blank
        let blank = Label::new(loader.clone(), "")?;
        grid.attach(&blank, 0, 0, 1, 1);

        // create column headers
        for c in 0..viewport_cols {
            let h = Label::new(loader.clone(), &format!("{}", index_to_col_label(c as Col)))?;
            h.set_markup(&format!("<span weight=bold>{}</span>", index_to_col_label(c as Col)));
            grid.attach(&h, (c + 1) as i32, 0, 1, 1);
            col_headers.push(h);
        }

        for r in 0..viewport_rows {
            // row header
            let rh = Label::new(loader.clone(), &format!("{}", r + 1))?;
            rh.set_markup(&format!("<span foreground=\"#666666\">{}</span>", r + 1));
            grid.attach(&rh, 0, (r + 1) as i32, 1, 1);
            row_headers.push(rh);

            let mut row_entries: Vec<Entry> = Vec::new();
            for c in 0..viewport_cols {
                let e = Entry::new(loader.clone())?;
                e.set_width_chars(12);
                e.set_size_request(120, 28);
                grid.attach(&e, (c + 1) as i32, (r + 1) as i32, 1, 1);
                row_entries.push(e);
            }
            cells.push(row_entries);
        }

        container.append(&grid);
        // put drawing area below the grid (we'll replace the cells with canvas rendering)
        container.append(&drawing);

        let sheet_rc = Rc::new(RefCell::new(sheet));
        let mut w = SpreadsheetWidget {
            sheet: sheet_rc,
            loader: loader.clone(),
            container,
            grid,
            row_headers,
            col_headers,
            cells,
            viewport_rows,
            viewport_cols,
            top_row: 1,
            left_col: 0,
        };

        // initial fill
        w.update_view()?;

        // keep drawing area queued for initial draw
        drawing.queue_draw();

        Ok(w)
    }

    pub fn as_widget(&self) -> &BoxWidget { &self.container }

    pub fn set_cell(&self, row: Row, col: Col, text: String) {
        self.sheet.borrow_mut().set_cell(row, col, text);
    }

    pub fn go_to(&mut self, row: Row, col: Col) -> Result<(), Box<dyn std::error::Error>> {
        // clamp
        let max_row = self.sheet.borrow().total_rows;
        let max_col = self.sheet.borrow().total_cols;
        let r = if row < 1 { 1 } else if row > max_row { max_row } else { row };
        let c = if col > max_col { max_col } else { col };
        self.top_row = r;
        self.left_col = c;
        self.update_view()?;
        Ok(())
    }

    pub fn scroll_by(&mut self, drows: i64, dcols: i64) -> Result<(), Box<dyn std::error::Error>> {
        let mut new_top = self.top_row as i64 + drows;
        if new_top < 1 { new_top = 1; }
        self.top_row = new_top as Row;
        let mut new_left = self.left_col as i64 + dcols;
        if new_left < 0 { new_left = 0; }
        self.left_col = new_left as Col;
        self.update_view()?;
        Ok(())
    }

    fn update_view(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Fill headers
        for (i, h) in self.col_headers.iter().enumerate() {
            let col_idx = self.left_col + (i as u32);
            h.set_markup(&format!("<span weight=bold>{}</span>", index_to_col_label(col_idx)));
        }
        for (i, h) in self.row_headers.iter().enumerate() {
            let row_idx = self.top_row + (i as u32);
            h.set_markup(&format!("<span foreground=\"#666666\">{}</span>", row_idx));
        }

        // populate visible entries with sheet data or empty
        for r in 0..self.viewport_rows {
            for c in 0..self.viewport_cols {
                let row_idx = self.top_row + (r as u32);
                let col_idx = self.left_col + (c as u32);
                if let Some(cell) = self.sheet.borrow().get_cell(row_idx, col_idx) {
                    self.cells[r][c].set_text(&cell.text);
                } else {
                    self.cells[r][c].set_text("");
                }
            }
        }

        // Apply overflow splitting across visible cells (visual only)
        self.apply_overflow();

        Ok(())
    }

    fn apply_overflow(&mut self) {
        use std::collections::HashMap;
        let mut measure_cache: HashMap<(usize, String), i32> = HashMap::new();
        // approximate per cell width in px by measuring a single cell widget
        let sample_widget_ptr = *self.cells[0][0].as_ref();
        let per_cell_px = gtk_compat::measure_text_px(&self.loader, Some(sample_widget_ptr), "MMMMMMMMMMMM");

        for r in 0..self.viewport_rows {
            // collect original texts
            let mut orig: Vec<String> = (0..self.viewport_cols).map(|c| self.cells[r][c].get_text().unwrap_or_default()).collect();
            let mut i = 0;
            while i < self.viewport_cols {
                let s = orig[i].clone();
                if s.is_empty() { i += 1; continue; }
                let widget_ptr = *self.cells[r][i].as_ref();
                let w = if let Some(&v) = measure_cache.get(&(widget_ptr as usize, s.clone())) { v }
                        else {
                            let m = gtk_compat::measure_text_px(&self.loader, Some(widget_ptr), &s);
                            measure_cache.insert((widget_ptr as usize, s.clone()), m);
                            m
                        };
                if w <= per_cell_px { i += 1; continue; }
                // only allow overflow if next cell exists and is blank
                if i + 1 < self.viewport_cols && !orig[i + 1].is_empty() { i += 1; continue; }

                // Gather target cells until a non-empty cell or end
                let mut targets = Vec::new();
                let mut j = i;
                while j < self.viewport_cols && (j == i || orig[j].is_empty()) {
                    targets.push(j);
                    j += 1;
                }

                // Split remaining text across target cells. Prefer breaking on whitespace when possible.
                let mut remaining = s.clone();
                remaining = remaining.trim_start().to_string();

                for &t in targets.iter() {
                    if remaining.is_empty() { orig[t] = String::new(); continue; }
                    let widget_ptr_t = *self.cells[r][t].as_ref();

                    // If the entire remaining text fits, take it all
                    let full_key = (widget_ptr_t as usize, remaining.clone());
                    if let Some(&cached_full) = measure_cache.get(&full_key) {
                        if cached_full <= per_cell_px { orig[t] = remaining.clone(); remaining.clear(); continue; }
                    } else {
                        let measured_full = gtk_compat::measure_text_px(&self.loader, Some(widget_ptr_t), &remaining);
                        measure_cache.insert(full_key.clone(), measured_full);
                        if measured_full <= per_cell_px { orig[t] = remaining.clone(); remaining.clear(); continue; }
                    }

                    // Binary-search the largest prefix (in characters) that fits
                    let chars: Vec<char> = remaining.chars().collect();
                    let mut low: usize = 0;
                    let mut high: usize = chars.len();
                    while low < high {
                        let mid = (low + high + 1) / 2;
                        let prefix: String = chars.iter().take(mid).collect();
                        let key = (widget_ptr_t as usize, prefix.clone());
                        let measured = if let Some(&cached) = measure_cache.get(&key) { cached }
                                       else { let m = gtk_compat::measure_text_px(&self.loader, Some(widget_ptr_t), &prefix); measure_cache.insert(key.clone(), m); m };
                        if measured <= per_cell_px { low = mid; } else { if mid == 0 { break; } high = mid - 1; }
                    }
                    let max_chars = if low == 0 { 1 } else { low };

                    // Prefer to break at the last whitespace within the prefix
                    let mut break_pos: Option<usize> = None;
                    for k in (0..max_chars).rev() {
                        if chars[k].is_whitespace() { break_pos = Some(k); break; }
                    }
                    let take = match break_pos {
                        Some(bp) if bp > 0 => bp,
                        _ => max_chars,
                    };

                    let mut chunk: String = chars.iter().take(take).collect();
                    chunk = chunk.trim_end().to_string();
                    orig[t] = chunk.clone();
                    let rem: String = chars.iter().skip(take).collect();
                    remaining = rem.trim_start().to_string();
                }

                i = j;
            }

            // apply back
            for (entry, txt) in self.cells[r].iter().zip(orig.iter()) {
                entry.set_text(txt);
            }
        }
    }
}
