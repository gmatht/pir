use corro::ui::App;
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;
use std::path::Path;

#[test]
fn render_overflow_sample() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/tests/overflow.corro");
    assert!(path.exists(), "overflow sample missing: {}", path.display());
    let mut app = App::new(Some(path));
    app.load_initial().unwrap();

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| app.bench_draw(f)).unwrap();
    let buffer = terminal.backend().buffer();
    let mut whole = String::new();
    for y in 0..buffer.area.height {
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect();
        whole.push_str(&row);
        whole.push('\n');
    }

    eprintln!("--- overflow.corro render ---\n{}--- end render ---", whole);
    // Ensure at least one of the long texts appears somewhere in the render output.
    assert!(whole.contains("This Text is really long and should overflow."), "render does not contain expected text");

    // Count occurrences of the long phrase to ensure each seeded cell rendered.
    let occurrences = whole.matches("This Text is really long and should overflow.").count();
    assert!(occurrences >= 3, "expected at least 3 occurrences of the long text, found {}", occurrences);

    // ── fg/bg color assertions ──────────────────────────────────────────

    // Menu bar (row 0): fg=Black, bg=Cyan
    let menu_style = buffer[(0, 0)].style();
    assert_eq!(menu_style.fg, Some(Color::Black), "menu fg should be Black");
    assert_eq!(menu_style.bg, Some(Color::Cyan), "menu bg should be Cyan");

    // Formula bar (row 1, Normal mode): address text has fg=Cyan
    let prompt_style = buffer[(0, 1)].style();
    assert_eq!(prompt_style.fg, Some(Color::Cyan), "formula bar fg should be Cyan (Normal mode)");

    // Hints line (last row for single-sheet workbook): fg=DarkGray
    let hints_style = buffer[(0, 39)].style();
    assert_eq!(hints_style.fg, Some(Color::DarkGray), "hints fg should be DarkGray");

    // Active column header (cursor starts at col A): fg=Black, bg=Yellow, Bold
    let has_active_header = (0..buffer.area.height).any(|y| {
        (0..buffer.area.width).any(|x| {
            let cell = &buffer[(x, y)];
            cell.style().fg == Some(Color::Black)
                && cell.style().bg == Some(Color::Yellow)
                && cell.style().add_modifier.contains(Modifier::BOLD)
        })
    });
    assert!(has_active_header, "expected active column header with fg=Black, bg=Yellow, Bold");

    // Cursor cell: bg=DarkGray with visible text content
    let has_cursor_cell = (0..buffer.area.height).any(|y| {
        (0..buffer.area.width).any(|x| {
            let cell = &buffer[(x, y)];
            cell.style().bg == Some(Color::DarkGray)
                && cell.style().fg != Some(Color::White)
                && !cell.symbol().trim().is_empty()
        })
    });
    assert!(has_cursor_cell, "expected cursor cell with bg=DarkGray");
}
