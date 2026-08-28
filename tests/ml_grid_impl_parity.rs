use corro::grid::{CellAddr, ColumnAddr, Grid, GridBox};

#[test]
fn grid_impl_parity_get_set_and_size() {
    let mut g = Grid::new(2, 2);
    g.set(&CellAddr::Main { row: 0, col: 0 }, "a".into());
    g.set(&CellAddr::Header { row: 0, col: ColumnAddr::Left(0) }, "h".into());

    let mut boxg = GridBox::new(g.clone());

    assert_eq!(boxg.main_rows(), g.main_rows());
    assert_eq!(boxg.main_cols(), g.main_cols());

    assert_eq!(
        boxg.get_owned(&CellAddr::Main { row: 0, col: 0 })
            .as_deref(),
        Some("a")
    );
    assert_eq!(
        boxg.get_owned(&CellAddr::Header { row: 0, col: ColumnAddr::Left(0) })
            .as_deref(),
        Some("h")
    );

    boxg.set_owned(&CellAddr::Main { row: 1, col: 1 }, "x".into());
    assert_eq!(
        boxg.get_owned(&CellAddr::Main { row: 1, col: 1 })
            .as_deref(),
        Some("x")
    );

    boxg.set_main_size(3, 4);
    assert_eq!(boxg.main_rows(), 3);
    assert_eq!(boxg.main_cols(), 4);
}
