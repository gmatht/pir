use std::path::PathBuf;
use corro::ops;
use corro::export;
use corro::grid::{CellAddr, ColumnAddr, MARGIN_COLS, HEADER_ROWS};

fn main() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/tests/subtotal-tiny.corro");
    let data = std::fs::read_to_string(&path).expect("read fixture");
    let mut workbook = ops::WorkbookState::new();
    // active_sheet is the sheet *id* used by apply_log_line_to_workbook
    let mut active_sheet = workbook.sheet_id(workbook.active_sheet);
    for (idx, raw) in data.lines().enumerate() {
        let t = raw.trim();
        if t.is_empty() {
            continue;
        }
        ops::apply_log_line_to_workbook(t, &mut workbook, &mut active_sheet)
            .unwrap_or_else(|e| panic!("{}:{}: {} => {e}", path.display(), idx + 1, t));
    }
    println!("workbook.next_sheet_id={} active_sheet={} sheets.len={}", workbook.next_sheet_id, workbook.active_sheet, workbook.sheets.len());
    for s in &workbook.sheets {
        println!(" sheet id={} title='{}' main_cols={}", s.id, s.title, s.state.grid.main_cols());
    }
    // active_sheet holds the sheet id after replay; use it to look up the sheet
    let sheet = workbook
        .sheet_mut_by_id(active_sheet)
        .expect("sheet");
    let grid = &sheet.grid;
    println!("main_rows={} main_cols={} total_cols={}", grid.main_rows(), grid.main_cols(), grid.total_cols());
    // Use public helpers where possible: delimited_export_matrix and ascii_col_header_label via matrix
    let (matrix, c0, c1, rows) = export::delimited_export_matrix(grid, &export::DelimitedExportOptions::default());
    println!("delimited matrix cols {}..{} rows={} matrix_len={}", c0, c1, rows.len(), matrix.len());
    println!("nonempty cols:");
    for c in 0..grid.total_cols() {
        if grid.logical_col_has_content(c) {
            println!("  col {}", c);
        }
    }
    // Avoid iterating over the enormous logical row range (HEADER_ROWS + FOOTER_ROWS).
    // Collect logical rows from the sparse non-empty iterator instead.
    let mut rows_set = std::collections::BTreeSet::new();
    for (addr, _val) in grid.iter_nonempty() {
        // grid is a GridBox; convert to Grid via GridBox::inner as Grid reference is needed.
        // We can compute logical row directly from addr and grid methods: the GridBox provides
        // total logical rows but not the helper; instead use grid.main_rows() to compute hr.
        let hr = HEADER_ROWS;
        let main_rows = grid.main_rows();
        let r = match addr {
            CellAddr::Header { row, .. } => row as usize,
            CellAddr::Main { row, .. } => hr + row as usize,
            CellAddr::Left { row, .. } | CellAddr::Right { row, .. } => hr + row as usize,
            CellAddr::Footer { row, .. } => hr + main_rows + row as usize,
        };
        rows_set.insert(r);
    }
    println!("nonempty rows:");
    for r in rows_set {
        println!("  row {}", r);
    }
    println!("nonempty cells:");
    for (addr, val) in grid.iter_nonempty() {
        // Show a human-friendly cell ref and the stored text.
        let ref_text = corro::addr::cell_ref_text(&addr, grid.main_cols());
        println!("  {} => {:?}", ref_text, val);
    }
    println!("matrix first rows:");
    for (i, row) in matrix.iter().take(6).enumerate() {
        println!("{}: {:?}", i, row);
    }

    // Print a full mapping from matrix cells back to logical addresses and stored values.
    // This helps trace which sheet cells produced each exported token.
    {
        let opts = export::DelimitedExportOptions::default();
        let include_headers = opts.include_header_row;
        let _include_margins = opts.include_margins;
        let row_key_col = opts.include_row_label_column;
        let mc = grid.main_cols();
        let mr = grid.main_rows();
        let hr = HEADER_ROWS;

        println!("\nMatrix -> CellAddr mapping (col_start={} col_end={} rows={}):", c0, c1, rows.len());
        for (ri, row) in matrix.iter().enumerate() {
            println!("MATRIX[{}]:", ri);
            if include_headers && ri == 0 {
                for (ci, field) in row.iter().enumerate() {
                    if row_key_col && ci == 0 {
                        println!("  header[{}] (row-label-col) => {:?}", ci, field);
                        continue;
                    }
                    let col_offset = if row_key_col { ci.saturating_sub(1) } else { ci };
                    let global_col = c0 + col_offset;
                    println!("  header[{}] -> global_col={} token={:?}", ci, global_col, field);
                }
                continue;
            }

            // data rows: map back to logical row from `rows` returned by delimited_export_matrix
            let row_index = if include_headers { ri.saturating_sub(1) } else { ri };
            if row_index >= rows.len() {
                println!("  (out of range row_index {} >= rows.len {})", row_index, rows.len());
                continue;
            }
            let logical_row = rows[row_index];
// Compute sheet_row_label inline (same logic as export::sheet_row_label)
        let sheet_row_label = if logical_row < HEADER_ROWS {
            format!("~{}", HEADER_ROWS - logical_row)
        } else if logical_row < HEADER_ROWS + mr {
            format!("{}", logical_row - HEADER_ROWS + 1)
        } else {
            let fr = logical_row - hr - mr;
            format!("_{}", fr + 1)
        };
            println!("  logical_row {} (sheet_row_label={})", logical_row, sheet_row_label);
            for (ci, field) in row.iter().enumerate() {
                if row_key_col && ci == 0 {
                    println!("    col[{}] ROW_LABEL -> {:?}", ci, field);
                    continue;
                }
                let col_offset = if row_key_col { ci.saturating_sub(1) } else { ci };
                let global_col = c0 + col_offset;

                // Build corresponding CellAddr for this logical_row/global_col
                use corro::grid::CellAddr;
                let addr: CellAddr = if logical_row < hr {
                    CellAddr::Header { 
                        row: logical_row as u32, 
                        col: ColumnAddr::from_global(global_col as usize, mc)
                    }
                } else if logical_row < hr + mr {
                    let main_row = logical_row - hr;
                    if global_col < MARGIN_COLS {
                        CellAddr::Left { col: global_col as usize, row: main_row as u32 }
                    } else if global_col < MARGIN_COLS + mc {
                        CellAddr::Main { row: main_row as u32, col: (global_col - MARGIN_COLS) as u32 }
                    } else {
                        CellAddr::Right { col: global_col as usize - MARGIN_COLS - mc, row: main_row as u32 }
                    }
                } else {
                    let fr = logical_row - hr - mr;
                    CellAddr::Footer { 
                        row: fr as u32, 
                        col: ColumnAddr::from_global(global_col as usize, mc)
                    }
                };

                let ref_text = corro::addr::cell_ref_text(&addr, mc);
                let stored = grid.text(&addr);
                println!("    col[{}] -> {} stored={:?} exported={:?}", ci, ref_text, stored, field);
            }
        }
    }

    // Render full matrix as TSV and write to a temp file for diffing against the golden fixture.
    let mut tsv = String::new();
    for row in &matrix {
        tsv.push_str(&row.join("\t"));
        tsv.push('\n');
    }
    let out_path = std::path::Path::new("/tmp/inspected_subtotal_matrix.tsv");
    std::fs::write(out_path, &tsv).expect("write dumped matrix");
    println!("Wrote TSV matrix to {}", out_path.display());
}
