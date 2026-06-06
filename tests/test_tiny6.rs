#[test]
fn test_narrow2() {
    let path = std::path::PathBuf::from("docs/tests/subtotal-tiny.corro");
    let mut app = corro::ui::App::new(Some(path));
    println!("before load");
    // Manually do what load_initial does
    let data = std::fs::read_to_string(&path).unwrap();
    let mut workbook = corro::ops::WorkbookState::new();
    let mut active_sheet = workbook.sheet_id(workbook.active_sheet);
    let (_, _replay) = corro::io::load_workbook_revisions_partial(
        &path,
        usize::MAX,
        &mut workbook,
        &mut active_sheet,
    ).unwrap();
    println!("workbook loaded");
    
    // Show the grid size
    println!("main_rows={} main_cols={}", workbook.sheets[0].state.grid.main_rows(), 
             workbook.sheets[0].state.grid.main_cols());
    
    // Now try to evaluate a cell
    let grid = &workbook.sheets[0].state.grid;
    for r in 0..grid.main_rows() {
        for c in 0..grid.main_cols() {
            let addr = corro::grid::CellAddr::Main { row: r as u32, col: c as u32 };
            let raw = grid.get(&addr).unwrap_or_default();
            println!("  cell ({},{}) raw={:?}", r, c, raw);
        }
    }
    println!("all good");
}
