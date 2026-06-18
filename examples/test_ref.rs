fn main() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let path = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/tests/date.corro"));
    let mut app = corro::ui::App::new(Some(path));
    app.load_initial().unwrap();
    eprintln!("Cursor row={} col={}", app.cursor.row, app.cursor.col);

    let backend = TestBackend::new(200, 50);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| app.bench_draw(f)).unwrap();
    let buffer = terminal.backend().buffer();

    let rows: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();

    for row in &rows {
        println!("{}", row);
    }
}
