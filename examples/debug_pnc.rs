fn main() {
    let mut app = corro::gui::App::new_with_paths(
        vec![std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/tests/date.corro"))]
    );
    app.load_initial().unwrap();
    
    // Clone what pnc_backend does
    let sheet_rec = app.core.workbook.active_sheet().clone();
    app.core.cursor.clamp(&sheet_rec.grid);
    
    let hr = corro::grid::HEADER_ROWS;
    let mr = sheet_rec.grid.main_rows();
    let mc = sheet_rec.grid.main_cols();
    let lm: usize = 702; // MARGIN_COLS
    
    eprintln!("Cursor: row={} col={}", app.core.cursor.row, app.core.cursor.col);
    eprintln!("hr={} mr={} mc={} lm={}", hr, mr, mc, lm);
    
    // Compute visible rows/cols
    let data_rows = 43usize;
    let data_cols = 21usize;
    let (display_rows, _) = corro::ui_core::visible_row_indices(&sheet_rec, app.core.cursor, data_rows, 0);
    eprintln!("display_rows (first 10): {:?}", &display_rows[..10.min(display_rows.len())]);
    
    let cursor_display_ri = display_rows.iter().position(|&r| r == app.core.cursor.row).unwrap_or(0);
    eprintln!("cursor_display_ri={}", cursor_display_ri);
    
    // Row labels
    for idx in 0..5.min(display_rows.len()) {
        let label = corro::addr::ui_row_label(display_rows[idx], mr);
        eprintln!("  display_rows[{}]={} label='{}'", idx, display_rows[idx], label);
    }
}
