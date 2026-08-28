#![cfg(feature = "ratatui")]

#[test]
fn test_load_only() {
    let path = std::path::PathBuf::from("docs/tests/subtotal-tiny.corro");
    let mut app = corro::ui::App::new(Some(path));
    app.load_initial().unwrap();
    // Just loading is fine — now try format_cell_display on a single cell:
    let sheet = app.workbook.active_sheet().clone();
    let grid = &sheet.grid;
    let addr = corro::grid::CellAddr::Main { row: 0, col: 0 };
    let val = grid.get(&addr).unwrap_or_default();
    let _disp = corro::format_cell_display(grid, &addr, val);
    // Try eval_cell on a =TOTAL cell:
    use corro::formula::eval_cell;
    let left_addr = corro::grid::CellAddr::Left { col: 0, row: 2 }; // [A3
    let left_val = grid.get(&left_addr).unwrap_or_default();
    let mut visiting = Vec::new();
    let mut budget = 10_000usize;
    let result = eval_cell(grid, &left_addr, &mut visiting, &mut budget);
    println!("Left cell {:?} value {:?} result {:?}", left_addr, left_val, result);
    
    // Try format_cell_display on left cell:
    let _disp2 = corro::format_cell_display(grid, &left_addr, left_val);
    println!("format done");
}
