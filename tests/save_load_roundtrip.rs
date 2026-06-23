#[test]
fn save_load_roundtrip() {
    let mut wb = corro::ops::WorkbookState::new();
    let addr = corro::grid::CellAddr::Main { row: 0, col: 0 };
    wb.active_sheet_mut().grid.set(&addr, "hello".to_string());

    let snapshot = corro::ops::WorkbookSnapshot::from_workbook(&wb);

    let tmp = std::env::temp_dir().join("test_save_load_roundtrip.corro");
    corro::io::save_workbook(&tmp, &snapshot).unwrap();

    let loaded_snapshot = corro::io::load_workbook_snapshot(&tmp).unwrap();
    let loaded_wb = corro::ops::WorkbookState::from_snapshot(&loaded_snapshot);

    let val = loaded_wb.active_sheet().grid.get(&addr);
    assert_eq!(val, Some("hello".into()));

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn save_load_roundtrip_multiple_cells() {
    let mut wb = corro::ops::WorkbookState::new();
    let cells = [
        (corro::grid::CellAddr::Main { row: 0, col: 0 }, "42"),
        (corro::grid::CellAddr::Main { row: 1, col: 0 }, "hello"),
        (corro::grid::CellAddr::Main { row: 0, col: 1 }, "world"),
        (corro::grid::CellAddr::Main { row: 5, col: 3 }, "=A1+1"),
    ];
    for (addr, val) in &cells {
        wb.active_sheet_mut().grid.set(addr, val.to_string());
    }

    let snapshot = corro::ops::WorkbookSnapshot::from_workbook(&wb);
    let tmp = std::env::temp_dir().join("test_save_load_roundtrip_multi.corro");
    corro::io::save_workbook(&tmp, &snapshot).unwrap();

    let loaded_snapshot = corro::io::load_workbook_snapshot(&tmp).unwrap();
    let loaded_wb = corro::ops::WorkbookState::from_snapshot(&loaded_snapshot);

    for (addr, expected) in &cells {
        let val = loaded_wb.active_sheet().grid.get(addr);
        assert_eq!(val, Some(expected.to_string()), "cell {:?}", addr);
    }

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn save_load_roundtrip_empty_workbook() {
    let wb = corro::ops::WorkbookState::new();
    let snapshot = corro::ops::WorkbookSnapshot::from_workbook(&wb);
    let tmp = std::env::temp_dir().join("test_save_load_roundtrip_empty.corro");
    corro::io::save_workbook(&tmp, &snapshot).unwrap();

    let loaded_snapshot = corro::io::load_workbook_snapshot(&tmp).unwrap();
    let loaded_wb = corro::ops::WorkbookState::from_snapshot(&loaded_snapshot);

    assert_eq!(wb.sheet_count(), loaded_wb.sheet_count());
    assert_eq!(wb.active_sheet().grid.main_rows(), loaded_wb.active_sheet().grid.main_rows());
    assert_eq!(wb.active_sheet().grid.main_cols(), loaded_wb.active_sheet().grid.main_cols());

    let _ = std::fs::remove_file(&tmp);
}
