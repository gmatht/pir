use std::collections::{HashMap, BTreeMap};

pub type Row = u32;
pub type Col = u32;

#[derive(Clone, Debug)]
pub struct Cell {
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct Sheet {
    pub default_col_width: f32,
    pub default_row_height: f32,
    pub col_widths: BTreeMap<Col, f32>,
    pub row_heights: BTreeMap<Row, f32>,
    pub cells: HashMap<(Row, Col), Cell>,
    pub total_rows: Row,
    pub total_cols: Col,
}

impl Sheet {
    pub fn new(total_rows: Row, total_cols: Col) -> Self {
        Sheet {
            default_col_width: 100.0,
            default_row_height: 24.0,
            col_widths: BTreeMap::new(),
            row_heights: BTreeMap::new(),
            cells: HashMap::new(),
            total_rows,
            total_cols,
        }
    }

    pub fn set_cell(&mut self, row: Row, col: Col, text: String) {
        if text.is_empty() {
            self.cells.remove(&(row, col));
        } else {
            self.cells.insert((row, col), Cell { text });
        }
    }

    pub fn get_cell(&self, row: Row, col: Col) -> Option<&Cell> {
        self.cells.get(&(row, col))
    }

    pub fn col_offset(&self, col: Col) -> f64 {
        // compute offset using default width and sparse corrections
        let base = (col as f64) * (self.default_col_width as f64);
        let mut correction: f64 = 0.0;
        for (&cidx, &w) in self.col_widths.range(..col) {
            correction += (w as f64) - (self.default_col_width as f64);
        }
        base + correction
    }

    pub fn row_offset(&self, row: Row) -> f64 {
        let base = (row as f64 - 1.0) * (self.default_row_height as f64);
        let mut correction: f64 = 0.0;
        for (&ridx, &h) in self.row_heights.range(..row) {
            correction += (h as f64) - (self.default_row_height as f64);
        }
        base + correction
    }
}

// A1 helpers
pub fn col_label_to_index(s: &str) -> Option<Col> {
    let mut v: u64 = 0;
    for b in s.as_bytes() {
        if *b < b'A' || *b > b'Z' { return None; }
        v = v * 26 + ((b - b'A') as u64 + 1);
        if v > (u64::from(u32::MAX) + 1) { return None; }
    }
    // convert to 0-based
    if v == 0 { return None; }
    Some((v - 1) as Col)
}

pub fn index_to_col_label(mut c: Col) -> String {
    // 0-based -> A..Z, AA.. etc.
    let mut out = String::new();
    c += 1; // make 1-based
    while c > 0 {
        let rem = ((c - 1) % 26) as u8;
        out.push((b'A' + rem) as char);
        c = (c - 1) / 26;
    }
    out.chars().rev().collect()
}
