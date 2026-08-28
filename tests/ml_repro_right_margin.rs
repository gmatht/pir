use corro::grid::{CellAddr, ColumnAddr, GridBox as Grid, HEADER_ROWS, MARGIN_COLS};
use corro::formula;

#[test]
fn parse_and_write_right_margin_header_addr() {
    // Construct a grid and write to the right-margin header cell directly
    let mut g = Grid::from(corro::grid::Grid::new(2, 3));
    let mc = g.main_cols();
    let _right_a_global = MARGIN_COLS + mc; // global column index of ]A
    let header_addr = CellAddr::Header {
        row: (HEADER_ROWS - 1) as u32,
        col: ColumnAddr::Right(0),
    };
    g.set(&header_addr, "=B".into());
    assert_eq!(g.get(&header_addr).as_deref(), Some("=B"));

    // templated_formula for the last main column should be derived from that right-margin header
    let main_addr = CellAddr::Main { row: 0, col: (mc - 1) as u32 };
    assert_eq!(formula::export_templated_formula(&g, &main_addr), Some("=B1".into()));
}
