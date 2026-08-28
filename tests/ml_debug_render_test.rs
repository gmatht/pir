#![cfg(feature = "ratatui")]

use std::path::PathBuf;
use corro::grid::{CellAddr, MARGIN_COLS, HEADER_ROWS};

fn main() {
    let path = PathBuf::from("test_rec5.corro");
    let mut app = corro::ui::App::new(Some(path));
    app.load_initial().unwrap();
    
    // Simulate cursor at A2 after some keypress
    // In the real scenario, cursor would be at main row 1, col A (global index MARGIN_COLS)
    let sheet = app.workbook.active_sheet().clone();
    let grid = &sheet.grid;
    let hr = HEADER_ROWS;
    let mr = grid.main_rows();
    let mc = grid.main_cols();
    let lm = MARGIN_COLS;
    
    // Simulate cursor at A2 (main row 1, main col 0)
    let cursor_row = hr + 1; // A2 = header_rows + main_row 1
    let cursor_col = lm + 0; // column A = margin_cols + 0
    
    // Build visible cols (simplified - just include a few)
    let data_width = 113usize;
    let data_cols = 56usize;
    let cursor = corro::grid::SheetCursor { row: cursor_row, col: cursor_col };
    let (mut col_ixs, _) = corro::ui_core::visible_col_indices(&sheet, cursor, data_cols, 0);
    corro::ui_core::trim_visible_cols_to_width(grid, &mut col_ixs, cursor_col, data_width);
    
    eprintln!("col_ixs: {:?}", col_ixs);
    eprintln!("cursor_row={} cursor_col={}", cursor_row, cursor_col);
    eprintln!("contains cursor_col: {}", col_ixs.contains(&cursor_col));
    
    // check width
    eprintln!("col_width(cursor_col)={}", grid.col_width(cursor_col));
    
    // compute the display text
    let cursor_main_row = cursor_row.saturating_sub(hr);
    let cursor_col_addr = corro::grid::ColumnAddr::from_global(cursor_col, mc);
    let cursor_addr = if cursor_row < hr {
        CellAddr::Header { row: cursor_row as u32, col: cursor_col_addr }
    } else if cursor_row < hr + mr {
        if cursor_col < lm {
            CellAddr::Left { row: cursor_main_row as u32, col: cursor_col }
        } else if cursor_col < lm + mc {
            CellAddr::Main { row: cursor_main_row as u32, col: (cursor_col - lm) as u32 }
        } else {
            CellAddr::Right { row: cursor_main_row as u32, col: cursor_col - lm - mc }
        }
    } else {
        CellAddr::Footer { row: (cursor_row - hr - mr) as u32, col: cursor_col_addr }
    };
    
    let effective = corro::formula::cell_effective_display(grid, &cursor_addr);
    let formatted = corro::ui_core::format_cell_display(grid, &cursor_addr, effective.clone());
    let cw = grid.col_width(cursor_col).max(1);
    let align = corro::ui_core::effective_cell_align(grid, &cursor_addr, &formatted);
    
    eprintln!("addr={:?} raw={:?} effective={:?} formatted={:?} cw={} align={:?}", 
        cursor_addr, grid.get(&cursor_addr), effective, formatted, cw, align);
    
    let display_text = corro::ui_core::align_cell_display(formatted.to_string(), cw, align);
    eprintln!("display_text='{}' len={} chars={} chars_count={}",
        display_text, display_text.len(), display_text.chars().collect::<Vec<_>>().len(), display_text.chars().count());
    for (i, c) in display_text.chars().enumerate() {
        eprintln!("  char {}: '{:?}' u+{:04X}", i, c, c as u32);
    }
}
