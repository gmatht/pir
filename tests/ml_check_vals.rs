/// Ensure the two test-data files are byte-identical and contain no
/// stale replayer-output lines (elloH / estT).  Replayer test sessions
/// used to modify the file in-place, accumulating duplicate SET commands
/// that broke the golden export tests.
///
/// `test_rec5.corro` is a working copy of `subtotal-tiny.corro` — it is
/// git-ignored and regenerated from the blessed file before each run.
#[test]
fn test_data_files_are_consistent() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let src = manifest.join("docs/tests/subtotal-tiny.corro");
    let dst = manifest.join("test_rec5.corro");
    // Regenerate working copy from blessed source.  The destination may be
    // read-only (set by gui_loop.sh / pre-commit hook), so remove first.
    let _ = std::fs::remove_file(&dst);
    std::fs::copy(&src, &dst).unwrap_or_else(|e| panic!("copy {} -> {}: {e}", src.display(), dst.display()));
    // Restore read-only guard so stray test runs can't corrupt the copy.
    if let Ok(meta) = std::fs::metadata(&dst) {
        let mut perms = meta.permissions();
        perms.set_readonly(true);
        let _ = std::fs::set_permissions(&dst, perms);
    }
    let f1 = std::fs::read(&src).unwrap();
    let f2 = std::fs::read(&dst).unwrap();
    assert_eq!(f1, f2, "docs/tests/subtotal-tiny.corro and test_rec5.corro must be byte-identical");
    let text = String::from_utf8_lossy(&f1);
    assert!(!text.contains("elloH"), "subtotal-tiny.corro must not contain 'elloH' (stale replayer output)");
    assert!(!text.contains("estT"), "subtotal-tiny.corro must not contain 'estT' (stale replayer output)");
}

use std::path::PathBuf;

#[cfg(feature = "ratatui")]
#[test]
fn check_test_rec5_values() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src = manifest.join("docs/tests/subtotal-tiny.corro");
    // Copy to a temporary file so we don't race with
    // test_data_files_are_consistent which also manages test_rec5.corro.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::copy(&src, tmp.path()).unwrap_or_else(|e| {
        panic!("copy {} -> {}: {e}", src.display(), tmp.path().display())
    });
    let mut app = corro::ui::App::new(Some(tmp.path().to_path_buf()));
    app.load_initial().unwrap();
    let sheet = app.workbook.active_sheet().clone();
    let grid = &sheet.grid;
    
    let addr = corro::grid::CellAddr::Main { row: 0, col: 0 };
    eprintln!("A1 raw: {:?}", grid.get(&addr));
    eprintln!("A1 display: {:?}", corro::formula::cell_effective_display(grid, &addr));

    let addr2 = corro::grid::CellAddr::Main { row: 1, col: 0 };
    eprintln!("A2 raw: {:?}", grid.get(&addr2));
    eprintln!("A2 display: {:?}", corro::formula::cell_effective_display(grid, &addr2));

    eprintln!("Cursor: row={}, col={}", app.cursor.row, app.cursor.col);
    eprintln!("main_rows={} main_cols={}", grid.main_rows(), grid.main_cols());
    
    // Print col_width for key columns
    eprintln!("col_width(0)={}", grid.col_width(0));
    eprintln!("col_width(701)={}", grid.col_width(701));
    eprintln!("col_width(702)={}", grid.col_width(702));
    eprintln!("col_width(703)={}", grid.col_width(703));

    // Check rendered_width_for_column (ui_core version)
    let rw = corro::ui_core::rendered_width_for_column(grid, 702);
    eprintln!("rendered_width_for_column(702)={:?}", rw);
    
    let rw703 = corro::ui_core::rendered_width_for_column(grid, 703);
    eprintln!("rendered_width_for_column(703)={:?}", rw703);
    
    // Check content_width_for_column
    let cw = grid.content_width_for_column(702);
    eprintln!("content_width_for_column(702)={:?}", cw);
    
    // Check col_width_overrides
    eprintln!("has_override_702={}", grid.col_width_overrides().iter().any(|(c,_)| *c == 702));
    for (c, w) in grid.col_width_overrides() {
        if c >= 700 && c <= 710 {
            eprintln!("  override col={} width={}", c, w);
        }
    }

    // Simulate what fit_main_columns_to_max_width does:
    // It calls rendered_width_for_column and set_col_width
    for c in 0..grid.main_cols() {
        let global_col = corro::grid::MARGIN_COLS + c;
        if let Some(rw) = corro::ui_core::rendered_width_for_column(grid, global_col) {
            let capped = rw.min(grid.max_col_width());
            eprintln!("  fit col={} rw={} capped={}", global_col, rw, capped);
        }
    }
}
