fn main() {
    let src = std::path::PathBuf::from("/root/src/corro/docs/tests/colwidth.corro");
    let tmp = std::env::temp_dir().join("corro-dump.corro");
    std::fs::copy(&src, &tmp).ok();
    let mut app = corro::ui::App::new(Some(tmp));
    app.load_initial().unwrap();

    let backend = ratatui::backend::TestBackend::new(120, 40);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|f| app.bench_draw(f)).unwrap();

    let buffer = terminal.backend().buffer();
    let visible: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    println!("{}", visible.join("\n"));
}
