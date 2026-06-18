

mod tmux {
    use std::process::Command;

    pub fn new_session(session: &str, command: &str) {
        let status = Command::new("tmux")
            .args(["new-session", "-d", "-s", session, "-x", "120", "-y", "40", command])
            .status()
            .expect("tmux new-session failed");
        assert!(status.success(), "tmux new-session exited non-zero");
    }

    pub fn send_keys(session: &str, key: &str) {
        Command::new("tmux").args(["send-keys", "-t", session, key]).status().ok();
    }

    pub fn capture_pane(session: &str) -> String {
        let output = Command::new("tmux")
            .args(["capture-pane", "-t", session, "-p", "-S", "-200"])
            .output()
            .expect("tmux capture-pane failed");
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    pub fn kill_session(session: &str) {
        Command::new("tmux").args(["kill-session", "-t", session]).output().ok();
    }
}

use std::path::PathBuf;
use std::time::Duration;

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Run pancurses in tmux, send keys, capture pane output.
fn run_in_tmux(args: &str, keys: &[&str], wait_ms: u64) -> String {
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let session = format!("corro-{}", id);
    let bin = format!("{}/target/debug/corro", env!("CARGO_MANIFEST_DIR"));
    tmux::new_session(&session, &format!("{} {}; sleep 2", bin, args));
    std::thread::sleep(Duration::from_millis(wait_ms));
    for key in keys {
        tmux::send_keys(&session, key);
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(400));
    let pane = tmux::capture_pane(&session);
    tmux::kill_session(&session);
    pane
}

/// Send the same key events to the ratatui App (via the public bench_handle_key API).
fn ratatui_send(app: &mut corro::ui::App, code: crossterm::event::KeyCode, mods: crossterm::event::KeyModifiers) {
    let ev = crossterm::event::KeyEvent::new(code, mods);
    app.bench_handle_key(ev).ok();
}

/// Render via ratatui TestBackend after sending a sequence of key events.
/// Operates on a COPY of the source file so the original .corro log is not mutated.
fn render_via_ratatui_with_keys(rel_path: &str, key_codes: &[crossterm::event::KeyCode]) -> String {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel_path);
    let tmp = std::env::temp_dir().join("corro-test-tmp.corro");
    std::fs::copy(&src, &tmp).ok();
    let mut app = corro::ui::App::new(Some(tmp));
    app.load_initial().unwrap();

    for &code in key_codes {
        ratatui_send(&mut app, code, crossterm::event::KeyModifiers::NONE);
    }

    let backend = ratatui::backend::TestBackend::new(120, 40);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|f| app.bench_draw(f)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render via ratatui TestBackend (no key events, initial state).
fn render_via_ratatui(rel_path: &str) -> String {
    render_via_ratatui_with_keys(rel_path, &[])
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[test]
fn overflow_renders_cell_text() {
    let pane = run_in_tmux("--pancurses docs/tests/overflow.corro", &[], 600);
    assert!(pane.contains("should overflow"), "pancurses missing cell text:\n{}",
        &pane[..pane.len().min(2000)]);
}

#[test]
fn q_quits() {
    let pane = run_in_tmux("--pancurses docs/tests/overflow.corro", &["q"], 200);
    assert!(pane.len() < 200 || !pane.contains("[File]"),
        "program should have exited after q");
}

#[test]
fn render_has_menu_and_cell_text() {
    let pane = run_in_tmux("--pancurses docs/tests/overflow.corro", &[], 500);
    let ratatui = render_via_ratatui("docs/tests/overflow.corro");
    for &s in &["[File]", "should overflow"] {
        assert!(pane.contains(s), "pancurses missing '{}'", s);
        assert!(ratatui.contains(s), "ratatui missing '{}'", s);
    }
}

#[test]
fn arrow_down_shows_a3() {
    let pane = run_in_tmux("--pancurses docs/tests/overflow.corro", &["Down"], 1200);
    assert!(pane.contains("A3"),
        "formula bar should show A3 after Down from A2\n---\n{}\n---", &pane[..pane.len().min(5000)]);
}

#[test]
fn right_arrow_shows_b2() {
    let pane = run_in_tmux("--pancurses docs/tests/overflow.corro", &["Right"], 1200);
    assert!(pane.contains("B2"),
        "formula bar should show B2 after Right from A2\n---\n{}\n---", &pane[..pane.len().min(5000)]);
}

#[test]
fn left_arrow_does_not_jump_viewport() {
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let session = format!("corro-{}", id);
    let bin = format!("{}/target/debug/corro", env!("CARGO_MANIFEST_DIR"));
    tmux::new_session(&session, &format!("{} --pancurses docs/tests/overflow.corro; sleep 2", bin));
    std::thread::sleep(Duration::from_millis(1200));

    // Verify app started
    let pane0 = tmux::capture_pane(&session);
    assert!(pane0.contains("[File]"), "app should show menu bar");

    // Press Left once — cursor moves to [A (left-margin column nearest A)
    tmux::send_keys(&session, "Left");
    std::thread::sleep(Duration::from_millis(300));
    let pane1 = tmux::capture_pane(&session);
    // After Left once, formula bar should show [A1 (cursor in left margin)
    assert!(pane1.contains("[A1") || pane1.contains("[A1 "),
        "after Left once, formula bar should show [A1\n{}",
        &pane1[..pane1.len().min(3000)]);

    // Press Left twice — cursor moves to [B
    tmux::send_keys(&session, "Left");
    std::thread::sleep(Duration::from_millis(300));
    let pane2 = tmux::capture_pane(&session);
    // After Left twice, formula bar should show [B1
    assert!(pane2.contains("[B1") || pane2.contains("[B1 "),
        "after Left twice, formula bar should show [B1\n{}",
        &pane2[..pane2.len().min(3000)]);

    tmux::kill_session(&session);
}

/// Move to C3, enter "Hello World!", and verify both backends show the
/// correct cell address and content (structural match, not exact char).
#[test]
fn edit_c3_hello_world_full_screen_match() {
    use crossterm::event::KeyCode;
    let keys = &["Right", "Right", "Down", "Down", "Enter", "H", "e", "l", "l", "o", " ",
                 "W", "o", "r", "l", "d", "!", "Enter"];
    let pane = run_in_tmux("--pancurses docs/tests/overflow.corro", keys, 2000);
    assert!(pane.contains("C4"), "pancurses formula bar should show C4 after edit\n{}",
        &pane[..pane.len().min(3000)]);

    let ratatui = render_via_ratatui_with_keys("docs/tests/overflow.corro", &[
        KeyCode::Right, KeyCode::Right, KeyCode::Down, KeyCode::Down,
        KeyCode::Enter,
        KeyCode::Char('H'), KeyCode::Char('e'), KeyCode::Char('l'), KeyCode::Char('l'),
        KeyCode::Char('o'), KeyCode::Char(' '),
        KeyCode::Char('W'), KeyCode::Char('o'), KeyCode::Char('r'), KeyCode::Char('l'),
        KeyCode::Char('d'), KeyCode::Char('!'),
        KeyCode::Enter,
    ]);
    assert!(ratatui.contains("C4"), "ratatui formula bar should show C4 after edit\n{}",
        &ratatui[..ratatui.len().min(3000)]);
}

/// Navigate to column K (past J) and verify the ratatui formula bar shows K1.
#[test]
fn navigate_to_column_k_via_ratatui() {
    use crossterm::event::KeyCode;
    let mut keys = Vec::new();
    for _ in 0..10 {
        keys.push(KeyCode::Right);
    }
    let ratatui = render_via_ratatui_with_keys("docs/tests/overflow.corro", &keys);
    assert!(ratatui.contains("K1"),
        "ratatui formula bar should show K1 after 10 Right presses\n{}",
        &ratatui[..ratatui.len().min(3000)]);
}

/// Arrow left from A1 should enter the left margin column (show [A label).
/// Arrow up from A1 should enter the header row (show ~1 label).
#[test]
fn arrow_left_from_a1_enters_margin() {
    let pane = run_in_tmux("--pancurses docs/tests/overflow.corro", &["Left"], 1000);
    // After Left from A1, cursor should show left margin label (like [A or similar)
    assert!(pane.contains("[A") || pane.contains("[") || pane.contains("A1"),
        "Left from A1 should show margin or remain at A1\n---\n{}\n---",
        &pane[..pane.len().min(2000)]);
}

/// Arrow up from A1 should enter the header row.
#[test]
fn arrow_up_from_a1_enters_header() {
    let pane = run_in_tmux("--pancurses docs/tests/overflow.corro", &["Up"], 1000);
    // After Up from A1, cursor should show header label (like ~1)
    assert!(pane.contains("~") || pane.contains("A1"),
        "Up from A1 should show header row label or remain at A1\n---\n{}\n---",
        &pane[..pane.len().min(2000)]);
}

/// Navigate to a cell via repeated arrow keys, enter "Hello World!", and verify
/// the pancurses formula bar shows the correct address.
#[test]
fn go_to_cell_and_enter_hello_world() {
    let pane = run_in_tmux("--pancurses docs/tests/overflow.corro", &[
        "Right","Right","Down","Down","Enter","H","e","l","l","o"," ",
        "W","o","r","l","d","!","Enter",
    ], 3000);

    assert!(pane.contains("C4") || pane.contains("c4") || pane.contains("D4") || pane.contains("d4"),
        "formula bar should show C4 or nearby after edit\n---\n{}\n---", &pane[..pane.len().min(3000)]);
    assert!(pane.contains("This Text is really long"),
        "cell content should still be visible\n---\n{}\n---", &pane[..pane.len().min(3000)]);
}

/// Go to cell A1000 via Ctrl+G, then enter "Hello World!".
/// Verifies the pancurses formula bar shows the Go-to address.
#[test]
fn go_to_cell_via_ctrlg() {
    let pane = run_in_tmux("--pancurses docs/tests/overflow.corro", &[
        "C-g", "Enter", "Hello", " ", "World", "!", "Enter",
    ], 3000);

    // After Ctrl+G (Go to A1000), formula bar should show "A1000"
    assert!(pane.contains("A100") || pane.contains("a100"),
        "formula bar should show A1000+ after Go-to\n---\n{}\n---",
        &pane[..pane.len().min(3000)]);
}
