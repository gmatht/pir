use std::fs;
use tempfile::NamedTempFile;

use corro::grid::CellAddr;

#[test]
fn editing_d1_serializes_as_d1_not_gutter() {
    // Create a simple workbook and commit an edit to D1; ensure the log uses D1
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_path_buf();
    // Start with a fresh workbook and set its main size to include D
    let mut workbook = corro::ops::WorkbookState::new();
    workbook.sheets[0].state.grid.set_main_size(1, 4); // A..D
    let mut active = workbook.sheet_id(workbook.active_sheet);
    let mut offset = 0u64;

    let op = corro::ops::WorkbookOp::SheetOp {
        sheet_id: active,
        op: corro::ops::Op::SetCell {
            addr: CellAddr::Main { row: 0, col: 3 }, // D1
            value: "x".into(),
        },
    };

    // Commit the op
    corro::io::commit_workbook_op(&path, &mut offset, &mut workbook, &mut active, &op).unwrap();

    let written = fs::read_to_string(&path).unwrap();
    assert!(written.contains("SET D1 x") || written.contains("SET $1:D1 x"), "written={}", written);
}
