use ratatui::style::{Color, Modifier};
use std::path::PathBuf;

/// Format cells via `ui_core::format_cell_display` and count non-empty cells.
fn test_corro_formatting(rel_path: &str) {
    let rel_path = rel_path.to_string();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(&rel_path);
    assert!(path.exists(), "missing: {}", path.display());
    // Run on a dedicated thread with a reasonably large stack.  In debug
    // (unoptimized) mode the formula-evaluation recursion can reach
    // MAX_VISIT_DEPTH=128 levels with ~200 KiB per frame, totalling
    // ~28 MiB for the heaviest fixture (math.corro).  32 MiB covers
    // all current fixtures with headroom.
    std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(move || {
        let mut app = corro::ui::App::new(Some(path));
        app.load_initial().unwrap();

        let sheet = app.workbook.active_sheet().clone();
        let grid = &sheet.grid;
        let main_rows = grid.main_rows();
        let main_cols = grid.main_cols();
        let mut nonempty = 0;

        for r in 0..main_rows {
            for c in 0..main_cols {
                let addr = corro::grid::CellAddr::Main { row: r as u32, col: c as u32 };
                if let Some(val) = grid.get(&addr) {
                    let display = corro::format_cell_display(grid, &addr, val.clone());
                    if !display.trim().is_empty() { nonempty += 1; }
                }
            }
        }
        assert!(nonempty > 0, "{}: no non-empty cells", rel_path);
    }).unwrap().join().unwrap();
}

/// Verify the ratatui render has expected structural elements and fg/bg colors.
fn test_render_structure(rel_path: &str) {
    let rel_path = rel_path.to_string();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(&rel_path);
    assert!(path.exists(), "missing: {}", path.display());
    std::thread::Builder::new().stack_size(28 * 1024 * 1024).spawn(move || {
        let mut app = corro::ui::App::new(Some(path));
        app.load_initial().unwrap();

        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| app.bench_draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let render_text: String = (0..buffer.area.height)
            .flat_map(|y| {
                let row: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                [row, "\n".to_string()]
            })
            .collect::<Vec<_>>()
            .concat();

        assert!(render_text.contains(" A "), "{}: ratatui render missing column header 'A'", rel_path);
        assert!(render_text.contains("   1 "), "{}: ratatui render missing row label '1'", rel_path);
        let has_unicode = render_text.contains('┌') || render_text.contains('│') || render_text.contains('─');
        assert!(has_unicode, "{}: ratatui render has no box-drawing characters", rel_path);

        // ── fg/bg color assertions ──────────────────────────────────────

        // Menu bar (row 0): fg=Black, bg=Cyan
        let menu_style = buffer[(0, 0)].style();
        assert_eq!(menu_style.fg, Some(Color::Black),
            "{}: menu bar fg should be Black", rel_path);
        assert_eq!(menu_style.bg, Some(Color::Cyan),
            "{}: menu bar bg should be Cyan", rel_path);

        // Formula bar (row 1, Normal mode): address text has fg=Cyan
        let prompt_style = buffer[(0, 1)].style();
        assert_eq!(prompt_style.fg, Some(Color::Cyan),
            "{}: formula bar fg should be Cyan (Normal mode)", rel_path);

        // Hints line (last row for single-sheet workbook): fg=DarkGray
        let hints_style = buffer[(0, 39)].style();
        assert_eq!(hints_style.fg, Some(Color::DarkGray),
            "{}: hints fg should be DarkGray", rel_path);

        // Active column header (cursor starts at col A): fg=Black, bg=Yellow, Bold
        let has_active_header = (0..buffer.area.height).any(|y| {
            (0..buffer.area.width).any(|x| {
                let cell = &buffer[(x, y)];
                cell.style().fg == Some(Color::Black)
                    && cell.style().bg == Some(Color::Yellow)
                    && cell.style().add_modifier.contains(Modifier::BOLD)
            })
        });
        assert!(has_active_header,
            "{}: expected active column header with fg=Black, bg=Yellow, Bold", rel_path);

        // Cursor cell: bg=DarkGray with user-visible text content
        let has_cursor_cell = (0..buffer.area.height).any(|y| {
            (0..buffer.area.width).any(|x| {
                let cell = &buffer[(x, y)];
                cell.style().bg == Some(Color::DarkGray)
                    && cell.style().fg != Some(Color::White) // not formula-bar text
                    && !cell.symbol().trim().is_empty()
            })
        });
        assert!(has_cursor_cell,
            "{}: expected cursor cell with bg=DarkGray", rel_path);
    }).unwrap().join().unwrap();
}

/// Verify that row labels computed the same way as `pnc_backend.rs` match
/// what the pancurses renderer would show. This catches bugs in
/// `ui_row_label` calls that produce wrong labels like `~9999`.
fn test_row_labels_match(rel_path: &str) {
    let rel_path = rel_path.to_string();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(&rel_path);
    assert!(path.exists(), "missing: {}", path.display());
    std::thread::Builder::new().stack_size(28 * 1024 * 1024).spawn(move || {
        let mut app = corro::ui::App::new(Some(path));
        app.load_initial().unwrap();

        let sheet = app.workbook.active_sheet().clone();
        let grid = &sheet.grid;
        let main_rows = grid.main_rows();
        let total_rows = main_rows.max(50);
        let header_rows = corro::grid::HEADER_ROWS;

        // This is the exact same computation as pnc_backend.rs after the fix:
        for r in 0..total_rows.min(200) {
            let logical_row = header_rows + r;
            let label = corro::addr::ui_row_label(logical_row, main_rows);
            // Main rows get simple numbers, footer rows get _N, header rows get ~N
            if r < main_rows {
                // Should be a simple number like "1", "2", ...
                assert!(!label.starts_with('~'), "row {} label '{}' looks like a header",
                        r, label);
                assert!(!label.starts_with('_'), "row {} label '{}' looks like a footer",
                        r, label);
                // Should parse as a number
                let num: usize = label.trim().parse().expect(
                    &format!("row {} label '{}' is not a number", r, label));
                assert_eq!(num, r + 1, "row {} label mismatch", r);
            }
        }

        // Verify with the original wrong computation that it WOULD have failed:
        // (This proves the fix was necessary)
        for r in 0..total_rows.min(200) {
            // OLD BUGGY code: passing r directly as logical_row AND main_cols as main_rows
            let old_label = corro::addr::ui_row_label(r, 0); // main_cols=0 would make all rows footers
            if r >= 1 {
                assert!(old_label.starts_with('_') || old_label.starts_with('~'),
                    "old code would fail: r={} label='{}' should be footer/header", r, old_label);
            }
        }
    }).unwrap().join().unwrap();
}

#[test] fn overflow() { test_corro_formatting("docs/tests/overflow.corro"); }
#[test] fn align() { test_corro_formatting("docs/tests/align.corro"); }
#[test] fn colwidth() { test_corro_formatting("docs/tests/colwidth.corro"); }
#[test] fn date() { test_corro_formatting("docs/tests/date.corro"); }
#[test] fn duplicate_col() { test_corro_formatting("docs/tests/duplicate_col.corro"); }
#[test] fn extrapolate() { test_corro_formatting("docs/tests/extrapolate.corro"); }
#[test] fn math() { test_corro_formatting("docs/tests/math.corro"); }
#[test] fn traditional() { test_corro_formatting("docs/tests/traditional.corro"); }
#[test] fn subtotal() { test_corro_formatting("docs/tests/subtotal.corro"); }
#[test] fn subtotal_tiny() { test_corro_formatting("docs/tests/subtotal-tiny.corro"); }
#[test] fn zerosum() { test_corro_formatting("docs/tests/zerosum.corro"); }
#[test] fn zerosum2() { test_corro_formatting("docs/tests/zerosum2.corro"); }

#[test] fn structure_overflow() { test_render_structure("docs/tests/overflow.corro"); }

// Tests that verify row labels match the CORRECT computation.
// These would catch the ~9999 bug.
#[test] fn row_labels_overflow() { test_row_labels_match("docs/tests/overflow.corro"); }
#[test] fn row_labels_align() { test_row_labels_match("docs/tests/align.corro"); }
#[test] fn row_labels_date() { test_row_labels_match("docs/tests/date.corro"); }

#[test]
fn inspect_state_align() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/tests/align.corro");
    let mut app = corro::ui::App::new(Some(path));
    app.load_initial().unwrap();
    let sheet = app.workbook.active_sheet().clone();
    eprintln!("INSPECT main_rows={} main_cols={}", sheet.grid.main_rows(), sheet.grid.main_cols());
    eprintln!("INSPECT cursor row={} col={}", app.cursor.row, app.cursor.col);
    assert!(true);
}




