#![cfg(feature = "ratatui")]

use corro::ui::App;
use ratatui::backend::TestBackend;
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
}
