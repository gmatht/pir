#[test]
fn test_clone() {
    let path = std::path::PathBuf::from("docs/tests/subtotal-tiny.corro");
    let mut app = corro::ui::App::new(Some(path));
    app.load_initial().unwrap();
    println!("loaded");
    // just clone sheet
    let sheet = app.workbook.active_sheet().clone();
    println!("cloned, grid id={:?}", sheet.grid.id());
    let grid = &sheet.grid;
    // get cell
    let val = grid.get(&corro::grid::CellAddr::Main { row: 0, col: 0 });
    println!("got A1={:?}", val);
}
