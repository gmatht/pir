#![cfg(feature = "ratatui")]

#[test]
fn test_tiny_render() {
    let path = std::path::PathBuf::from("docs/tests/subtotal-tiny.corro");
    let mut app = corro::ui::App::new(Some(path));
    app.load_initial().unwrap();
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    // Tiny viewport — just 10×10
    let backend = TestBackend::new(10, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| app.bench_draw(f)).unwrap();

    // Try larger
    let backend = TestBackend::new(50, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| app.bench_draw(f)).unwrap();

    let backend = TestBackend::new(120, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| app.bench_draw(f)).unwrap();
}
