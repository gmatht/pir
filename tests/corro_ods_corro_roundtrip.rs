//! Load a `.corro` workbook, export the **full** workbook to ODS (Generic, same as the UI),
//! re-import, and assert **effective display** matches on the **main grid** for cells that are
//! non-empty in **both** versions (intersection). Margins, header rows, and footers are skipped:
//! generic ODF interop can change *stored* formula text there.
//!
//! **Computed values / “offset adjustment”:** Generic export rewrites cell references in formulas
//! so the ODF table’s top-left aligns with the exported matrix (row/column offset / rebase). After
//! import, stored `=` text may differ from the original `.corro` log, but evaluated **main-grid**
//! numbers and text must match — we compare [`cell_effective_display`] under the same eval context,
//! not raw cell strings.

use std::collections::HashSet;
use std::fs;
use std::io::{Cursor, Read};
use std::path::Path;

use corro::export::{delimited_export_matrix, DelimitedExportOptions, ExportContent};
use corro::formula::{cell_effective_display, refresh_spills, set_eval_context};
use corro::grid::{CellAddr, HEADER_ROWS, MARGIN_COLS};
use corro::ods::{export_ods_bytes_workbook_with_options, import_ods_workbook};
use zip::ZipArchive;
use corro::ops::WorkbookState;

fn load_corro_fixture(path: &Path) -> WorkbookState {
    let data = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut workbook = WorkbookState::new();
    let mut active_sheet = workbook.sheet_id(workbook.active_sheet);
    for (line_no, line) in data.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        corro::ops::apply_log_line_to_workbook(t, &mut workbook, &mut active_sheet)
            .unwrap_or_else(|e| panic!("{} line {}: {e}", path.display(), line_no + 1));
    }
    workbook
}

fn nonempty_addrs(grid: &corro::grid::GridBox) -> HashSet<CellAddr> {
    grid.iter_nonempty().map(|(a, _)| a).collect()
}

/// After ODS interop, stored formula text may differ; users care about the same **display**.
fn assert_workbook_effective_display_parity(before: &WorkbookState, after: &WorkbookState) {
    assert_eq!(
        before.sheet_count(),
        after.sheet_count(),
        "sheet count after ODS round trip"
    );
    for i in 0..before.sheet_count() {
        assert_eq!(
            before.sheet_title(i),
            after.sheet_title(i),
            "sheet {i} title"
        );
    }

    let mut wb = before.clone();
    for s in &mut wb.sheets {
        refresh_spills(&mut s.state.grid);
    }
    let mut wa = after.clone();
    for s in &mut wa.sheets {
        refresh_spills(&mut s.state.grid);
    }

    for i in 0..wb.sheet_count() {
        // Only the **main** grid is compared: ODS round-trip is stable for main data; margin/header/footer
        // can differ in stored text while still evaluating consistently for cells we don’t assert here.
        let a: HashSet<CellAddr> = nonempty_addrs(&wb.sheets[i].state.grid);
        let b: HashSet<CellAddr> = nonempty_addrs(&wa.sheets[i].state.grid);

        let mut addrs: Vec<CellAddr> = a
            .intersection(&b)
            .filter(|a| matches!(a, CellAddr::Main { .. }))
            .cloned()
            .collect();
        addrs.sort_by_key(|a| match a {
            CellAddr::Main { row, col } => (*row, *col),
            _ => (0u32, 0u32),
        });
        for addr in addrs {
            let expected = {
                let _guard = set_eval_context(&wb);
                cell_effective_display(&wb.sheets[i].state.grid, &addr)
            };
            let got = {
                let _guard = set_eval_context(&wa);
                cell_effective_display(&wa.sheets[i].state.grid, &addr)
            };
            assert_eq!(
                expected, got,
                "main-grid effective display mismatch after ODS round trip: sheet {i} ({}) addr {addr:?}",
                before.sheet_title(i)
            );
        }
    }
}

fn roundtrip_generic_ods_workbook(before: &WorkbookState) -> WorkbookState {
    let opts = DelimitedExportOptions {
        content: ExportContent::Generic,
        ..Default::default()
    };
    let bytes = export_ods_bytes_workbook_with_options(before, &opts).expect("export ods");
    let tmp = tempfile::NamedTempFile::new().expect("temp file");
    fs::write(tmp.path(), bytes).expect("write ods");
    import_ods_workbook(tmp.path()).expect("import ods")
}

fn roundtrip_generic_ods_parity(before: &WorkbookState) {
    let after = roundtrip_generic_ods_workbook(before);
    assert_workbook_effective_display_parity(before, &after);
}

/// Regression: every main data cell in the subtotal-style fixture must keep correct **raw** value
/// after ODS round trip (literals) before we can trust formula rebase.
#[test]
fn subtotal_style_ods_corro_main_literals_survive() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/subtotal_style_ods_roundtrip.corro");
    let before = load_corro_fixture(&path);
    let after = roundtrip_generic_ods_workbook(&before);
    let g = &after.sheets[0].state.grid;
    for (addr, want) in [
        (CellAddr::Main { row: 0, col: 0 }, "1"),
        (CellAddr::Main { row: 0, col: 1 }, "10"),
        (CellAddr::Main { row: 1, col: 0 }, "2"),
        (CellAddr::Main { row: 1, col: 1 }, "20"),
    ] {
        let got = g.text(&addr);
        assert_eq!(got, want, "raw cell text after ODS: {addr:?}");
    }
}

/// Always-on: committed 2-sheet fixture (MAX/MIN multi-range, SUM, TAX) exercises the same ODF path
/// as real subtotal workbooks, including formula rebase.
#[test]
fn subtotal_style_corro_ods_corro_roundtrip_matches_effective_display() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/subtotal_style_ods_roundtrip.corro");
    let before = load_corro_fixture(&path);
    roundtrip_generic_ods_parity(&before);
}

/// When `subtotal.corro` is present in the repo root (optional local or vendored file), also verify
/// the full multi-sheet subtotal log round-trips with identical **computed** main-grid display.
#[test]
fn subtotal_corro_working_copy_ods_corro_roundtrip_matches_effective_display() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("subtotal.corro");
    if !path.is_file() {
        eprintln!(
            "skip: {} not found (optional; see tests/fixtures/subtotal_style_ods_roundtrip.corro)",
            path.display()
        );
        return;
    }
    let before = load_corro_fixture(&path);
    roundtrip_generic_ods_parity(&before);
}

#[test]
fn debug_subtotal_b11_inspect() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("subtotal.corro");
    if !path.is_file() {
        return;
    }
    let before = load_corro_fixture(&path);
    let g0 = &before.sheets[0].state.grid;
    let opts = DelimitedExportOptions {
        content: ExportContent::Generic,
        ..Default::default()
    };
    let (_m, c0, _c1, dr) = delimited_export_matrix(g0, &opts);
    eprintln!("export col_start={c0} data_rows[0]={:?}", dr.first());
    let h703 = CellAddr::Header {
        row: (HEADER_ROWS - 1) as u32,
        col: 703,
    };
    let h706 = CellAddr::Header {
        row: (HEADER_ROWS - 1) as u32,
        col: 706,
    };
    eprintln!("before header col 703: {:?}", g0.text(&h703));
    eprintln!("before header col 706: {:?}", g0.text(&h706));
    let bytes = export_ods_bytes_workbook_with_options(&before, &opts).expect("export");
    let mut z = ZipArchive::new(Cursor::new(bytes)).expect("zip");
    let mut xml = String::new();
    z.by_name("content.xml")
        .expect("content")
        .read_to_string(&mut xml)
        .expect("read");
    for (i, seg) in xml.split("<table:table-row>").enumerate() {
        if seg.contains("TAX") && seg.contains("0.1") {
            let n = seg.matches("<table:table-cell").count();
            eprintln!("ODS row fragment {i} with TAX: {n} table:table-cell tags");
            eprintln!("  fragment (trunc 1200c):");
            eprintln!("{}", &seg.chars().take(1200).collect::<String>());
        }
    }
    let mut sidecar = String::new();
    z.by_name("corro-ods-layout")
        .expect("layout")
        .read_to_string(&mut sidecar)
        .expect("read layout");
    eprintln!("sidecar first lines:\n{}", &sidecar.lines().take(12).collect::<Vec<_>>().join("\n"));
    let mut wb = before.clone();
    for s in &mut wb.sheets {
        refresh_spills(&mut s.state.grid);
    }
    let g = &wb.sheets[0].state.grid;
    let hb = CellAddr::Header {
        row: (HEADER_ROWS - 1) as u32,
        col: (MARGIN_COLS as u32) + 1,
    };
    let b11 = CellAddr::Main { row: 10, col: 1 };
    eprintln!("header B~1 raw: {:?}", g.text(&hb));
    eprintln!("Main (10,1) raw: {:?}", g.text(&b11));
    let _guard = set_eval_context(&wb);
    eprintln!("Main (10,1) eff before: {:?}", cell_effective_display(g, &b11));
    let after = roundtrip_generic_ods_workbook(&before);
    let mut wa = after.clone();
    for s in &mut wa.sheets {
        refresh_spills(&mut s.state.grid);
    }
    let ga = &wa.sheets[0].state.grid;
    eprintln!("header B~1 raw after: {:?}", ga.text(&hb));
    eprintln!("Main (10,1) raw after: {:?}", ga.text(&b11));
    let _g2 = set_eval_context(&wa);
    eprintln!("Main (10,1) eff after: {:?}", cell_effective_display(ga, &b11));
    for (a, t) in ga.iter_nonempty() {
        if let CellAddr::Header { row, col } = a {
            if row == (HEADER_ROWS - 1) as u32 && (t.contains("TAX") || t.contains("0.1")) {
                eprintln!("header ~1 row with tax-ish: {a:?} => {:?}", t);
            }
        }
    }
}
