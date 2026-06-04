#[test]
fn test_eval_main() {
    let path = std::path::PathBuf::from("docs/tests/subtotal-tiny.corro");
    let mut app = corro::ui::App::new(Some(path));
    app.load_initial().unwrap();
    let sheet = app.workbook.active_sheet().clone();
    let grid = &sheet.grid;

    // Just eval a main cell
    let addr = corro::grid::CellAddr::Main { row: 0, col: 0 };
    let mut v = Vec::new();
    let mut b = 10_000usize;
    println!("A1={:?}", corro::formula::eval_cell(grid, &addr, &mut v, &mut b));
    println!("step1 ok");

    // Now eval the =TOTAL left cell
    let addr2 = corro::grid::CellAddr::Left { col: 0, row: 2 };
    let mut v2 = Vec::new();
    let mut b2 = 10_000usize;
    let r = corro::formula::eval_cell(grid, &addr2, &mut v2, &mut b2);
    println!("Left =TOTAL result={:?}", r);
    println!("step2 ok");
}
