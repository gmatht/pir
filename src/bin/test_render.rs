fn main() {
    let path = std::path::PathBuf::from("docs/tests/subtotal-tiny.corro");
    let mut app = corro::ui::App::new(Some(path));
    eprintln!("before load");
    app.load_initial().unwrap();
    eprintln!("after load");
    let sheet = app.workbook.active_sheet().clone();
    eprintln!("after clone");
    
    // Just render via format_cell_display on a single cell
    let addr = corro::grid::CellAddr::Main { row: 0, col: 0 };
    let val = sheet.grid.get(&addr).unwrap_or_default();
    eprintln!("A1 raw={:?}", val);
    let disp = corro::format_cell_display(&sheet.grid, &addr, val);
    eprintln!("A1 display={:?}", disp);
    
    // Try a left margin cell with =TOTAL
    let left_addr = corro::grid::CellAddr::Left { col: 0, row: 2 };
    let left_val = sheet.grid.get(&left_addr).unwrap_or_default();
    eprintln!("[A3 raw={:?}", left_val);
    let disp2 = corro::format_cell_display(&sheet.grid, &left_addr, left_val);
    eprintln!("[A3 display={:?}", disp2);
}
