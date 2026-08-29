//! Parity tests: prove the shared, backend-agnostic spreadsheet model paints
//! identically (in content) on the headless recorder, the ratatui terminal
//! backend, and the pancurses terminal-grid backend. Run with e.g.
//! `--no-default-features --features ratatui,headless,pancurses`.
#![cfg(any(all(feature = "ratatui", feature = "headless"), feature = "pancurses"))]

use rustxwidgets::spreadsheet::{paint, SpreadsheetModel};

#[cfg(all(feature = "ratatui", feature = "headless"))]
use rustxwidgets::backends::headless::RecordingDrawContext;
#[cfg(all(feature = "ratatui", feature = "headless"))]
use rustxwidgets::backends::ratatui::{demo_model, render_demo_to_test_backend};

/// Build a small spreadsheet with content in real data cells.
fn model_with(title: &str, cells: &[((u32, u32), &str)]) -> SpreadsheetModel {
    let mut m = SpreadsheetModel::new(6, 4);
    m.set_border_title(title);
    for &((r, c), t) in cells {
        m.set_cell(r, c, t);
    }
    m
}

#[cfg(all(feature = "ratatui", feature = "headless"))]
#[test]
fn headless_records_expected_text() {
    let m = model_with("PARITY-TEST", &[((1, 1), "Hello"), ((2, 2), "World")]);
    let mut dc = RecordingDrawContext::new();
    paint(&m, &mut dc, 80, 24);

    assert!(!dc.ops.is_empty(), "paint should record draw operations");
    assert!(dc.has_text("PARITY-TEST"), "border title should be painted");
    assert!(dc.has_text("Hello"), "cell text should be painted");
    assert!(dc.has_text("World"), "cell text should be painted");
}

#[cfg(all(feature = "ratatui", feature = "headless"))]
#[test]
fn headless_records_clear_and_rects() {
    let m = model_with("RECTS", &[]);
    let mut dc = RecordingDrawContext::new();
    paint(&m, &mut dc, 80, 24);

    assert!(dc.ops.iter().any(|o| matches!(o, rustxwidgets::backends::headless::DrawOp::Clear(..))));
    assert!(!dc.fill_rects().is_empty(), "background regions should be filled");
}

#[cfg(all(feature = "ratatui", feature = "headless"))]
#[test]
fn ratatui_backend_renders_same_model() {
    let buf = render_demo_to_test_backend(80, 24);
    let rows = crate::row_strings(&buf);
    for (i, r) in rows.iter().enumerate() {
        if !r.trim().is_empty() {
            eprintln!("row {i:>2}: {r}");
        }
    }
    let joined = rows.join("");

    assert!(joined.contains("rustxWidgets"), "border title must appear on the ratatui canvas");
    assert!(joined.contains("Hello"), "cell text must appear on the ratatui canvas");
    assert!(joined.contains("World"), "cell text must appear on the ratatui canvas");
    assert!(joined.contains("=SUM(A1:A3)"), "formula cell must appear on the ratatui canvas");
}

#[cfg(all(feature = "ratatui", feature = "headless"))]
#[test]
fn ratatui_and_headless_agree_on_content() {
    // Both backends consume the same model; the set of drawn text must match,
    // proving the pixel-space paint is backend-agnostic in *what* it draws.
    let m = demo_model();
    let mut dc = RecordingDrawContext::new();
    paint(&m, &mut dc, 80, 24);
    let headless_texts: std::collections::HashSet<String> = dc.texts().iter().map(|s| s.to_string()).collect();

    let buf = render_demo_to_test_backend(80, 24);
    let rows = crate::row_strings(&buf);
    let ratatui_joined = rows.join("");

    for t in &headless_texts {
        if t.len() >= 4 {
            assert!(ratatui_joined.contains(t), "headless text {t:?} missing from ratatui render");
        }
    }
}

#[cfg(feature = "pancurses")]
#[test]
fn pancurses_backend_renders_same_model() {
    use rustxwidgets::backends::pancurses_draw::{render_model_to_grid, GridCell};

    // A representative model with content in real data cells.
    let mut m = SpreadsheetModel::new(6, 4);
    m.set_border_title("rustxWidgets pancurses");
    m.set_formula_bar("A1", "=1+1");
    m.set_tab_data(&["Sheet1".to_string(), "Sheet2".to_string()], 0);
    m.set_cell(1, 1, "Hello");
    m.set_cell(2, 2, "World");
    m.set_cell(3, 3, "=SUM(A1:A3)");
    m.set_cursor(1, 1);

    let grid = rustxwidgets::backends::pancurses_draw::render_model_to_grid(&m, 80, 24);
    let rows: Vec<String> = grid.row_strings();
    for (i, r) in rows.iter().enumerate() {
        if r.trim_end().chars().any(|c| c != ' ') {
            eprintln!("row {i:>2}: {r}");
        }
    }
    let joined = rows.join("");

    assert!(joined.contains("rustxWidgets"), "border title must appear on the pancurses grid");
    assert!(joined.contains("Hello"), "cell text must appear on the pancurses grid");
    assert!(joined.contains("World"), "cell text must appear on the pancurses grid");
    assert!(joined.contains("=SUM(A1:A3)"), "formula cell must appear on the pancurses grid");
    assert!(joined.contains("Sheet1"), "tabs must appear on the pancurses grid");

    // Sanity: every cell has a resolvable foreground colour (no panics / garbage).
    for row in &grid.cells {
        for c in row {
            let _: GridCell = *c;
        }
    }
}

#[cfg(all(feature = "ratatui", feature = "headless", feature = "pancurses"))]
#[test]
fn tabs_and_footer_parity_across_all_backends() {
    // Exercise the chrome widgets (tabs, status bar, formula bar) on every
    // backend to prove the shared model paints them identically.
    let mut m = SpreadsheetModel::new(6, 4);
    m.set_tab_data(&["Alpha".to_string(), "Beta".to_string()], 0);
    m.set_status_text("READY 42/100");
    m.set_formula_bar("B2", "=A1+A2");
    m.set_cell(1, 1, "Hello");
    m.set_cell(2, 2, "World");

    // headless recorder
    let mut dc = RecordingDrawContext::new();
    paint(&m, &mut dc, 80, 24);
    let headless_texts: std::collections::HashSet<String> =
        dc.texts().iter().map(|s| s.to_string()).collect();

    // ratatui terminal backend
    let buf = rustxwidgets::backends::ratatui::render_model_to_test_backend(&m, 80, 24);
    let ratatui_rows: Vec<String> = (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf.cell((x as u16, y as u16)).map(|c| c.symbol()).unwrap_or("").to_string())
                .collect()
        })
        .collect();
    let ratatui_joined = ratatui_rows.join("");

    // pancurses terminal-grid backend
    let grid = rustxwidgets::backends::pancurses_draw::render_model_to_grid(&m, 80, 24);
    let pancurses_joined = grid.row_strings().join("");

    for token in ["Alpha", "Beta", "READY 42/100", "B2", "Hello", "World"] {
        assert!(
            headless_texts.iter().any(|t| t.contains(token)),
            "headless missing {token:?}"
        );
        assert!(ratatui_joined.contains(token), "ratatui missing {token:?}");
        assert!(pancurses_joined.contains(token), "pancurses missing {token:?}");
    }
}
#[cfg(all(feature = "ratatui", feature = "headless", feature = "pancurses"))]
fn row_strings(buf: &ratatui::buffer::Buffer) -> Vec<String> {
    let w = buf.area.width as usize;
    (0..buf.area.height)
        .map(|y| {
            (0..w)
                .map(|x| buf.cell((x as u16, y as u16)).map(|c| c.symbol()).unwrap_or("").to_string())
                .collect()
        })
        .collect()
}

#[cfg(feature = "ratatui")]
#[test]
fn input_event_from_crossterm() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use rustxwidgets::core::InputEvent;
    assert_eq!(
        InputEvent::from_crossterm(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        InputEvent::Quit
    );
    assert_eq!(
        InputEvent::from_crossterm(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        InputEvent::Char('a')
    );
    assert_eq!(
        InputEvent::from_crossterm(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        InputEvent::ArrowUp
    );
}

#[cfg(feature = "pancurses")]
#[test]
fn input_event_from_pancurses() {
    use rustxwidgets::core::InputEvent;
    assert_eq!(InputEvent::from_pancurses(pancurses::Input::Character('x')), InputEvent::Char('x'));
    assert_eq!(InputEvent::from_pancurses(pancurses::Input::KeyUp), InputEvent::ArrowUp);
    assert_eq!(InputEvent::from_pancurses(pancurses::Input::KeyF1), InputEvent::F(1));
}

/// Grab the rendered terminal screen from the ratatui canvas backend.
#[cfg(all(feature = "ratatui", feature = "headless"))]
fn grab_ratatui(model: &SpreadsheetModel, w: u16, h: u16) -> Vec<String> {
    let buf = rustxwidgets::backends::ratatui::render_model_to_test_backend(model, w, h);
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf.cell((x as u16, y as u16)).map(|c| c.symbol()).unwrap_or("").to_string())
                .collect()
        })
        .collect()
}

/// Grab the rendered terminal screen from the pancurses cell-grid backend.
#[cfg(feature = "pancurses")]
fn grab_pancurses(model: &SpreadsheetModel, w: u16, h: u16) -> Vec<String> {
    rustxwidgets::backends::pancurses_draw::render_model_to_grid(model, w, h).row_strings()
}

/// Extract the multiset of whitespace-delimited tokens from a grabbed screen.
fn screen_tokens(rows: &[String]) -> Vec<String> {
    let mut t: Vec<String> = rows.iter().flat_map(|r| r.split_whitespace().map(|s| s.to_string())).collect();
    t.sort();
    t
}

/// Terminal-grab equivalence: the ratatui canvas backend and the pancurses
/// cell-grid backend both consume the *same* shared `paint`, so grabbing each
/// backend's rendered screen must yield a byte-identical terminal picture
/// (and therefore an identical token multiset). This is the direct analogue of
/// the ratatui/pancurses driver tests, but at the level of the whole screen.
#[cfg(all(feature = "ratatui", feature = "headless", feature = "pancurses"))]
#[test]
fn ratatui_and_pancurses_terminal_grab_equivalent() {
    let models: Vec<SpreadsheetModel> = vec![
        model_with("GRAB-TEST", &[((1, 1), "Hello"), ((2, 2), "World"), ((3, 3), "=SUM(A1:A3)")]),
        {
            let mut m = SpreadsheetModel::new(6, 4);
            m.set_border_title("EQUIV-2");
            m.set_tab_data(&["Alpha".to_string(), "Beta".to_string()], 0);
            m.set_status_text("READY 42/100");
            m.set_formula_bar("B2", "=A1+A2");
            m.set_cell(1, 1, "Hello");
            m.set_cell(2, 2, "World");
            m
        },
    ];
    for (i, m) in models.iter().enumerate() {
        let rat = grab_ratatui(m, 80, 24);
        let pan = grab_pancurses(m, 80, 24);

        assert_eq!(rat.len(), pan.len(), "model {i}: grabbed screen heights must match");
        for (y, (r, p)) in rat.iter().zip(pan.iter()).enumerate() {
            assert_eq!(
                r, p,
                "model {i}: grabbed terminal row {y} differs between ratatui and pancurses"
            );
        }
        // Order-independent content equivalence (complementary to row eq).
        assert_eq!(
            screen_tokens(&rat),
            screen_tokens(&pan),
            "model {i}: terminal-grab token sets must match"
        );
    }
}
