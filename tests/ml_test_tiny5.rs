#![cfg(feature = "ratatui")]

#[test]
fn test_narrow() {
    let path = std::path::PathBuf::from("docs/tests/subtotal-tiny.corro");
    let mut app = corro::ui::App::new(Some(path));
    println!("before load");
    app.load_initial().unwrap();
    println!("after load, main_rows={} main_cols={}", app.state.grid.main_rows(), app.state.grid.main_cols());
    let grid = &app.state.grid;
    // Try cell_effective_display on each cell:
    for r in 0..grid.main_rows() {
        for c in 0..grid.main_cols() {
            let addr = corro::grid::CellAddr::Main { row: r as u32, col: c as u32 };
            let _ = corro::formula::cell_effective_display(grid, &addr);
            println!("  cell ({},{}) ok", r, c);
        }
    }
    println!("all done");
}
