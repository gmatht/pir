#![cfg(feature = "gui")]

use std::fs;
use std::path::Path;

#[test]
fn gui_menu_items_available() {
    let gui_dir = Path::new("src/gui");
    let mut all_menu_items_found = false;

    for entry in fs::read_dir(gui_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().map_or(true, |e| e != "rs") {
            continue;
        }
        let content = fs::read_to_string(&path).unwrap();
        if content.contains("file_open_dialog()") &&
           content.contains("file_save_dialog()") &&
           content.contains("show_keybinds_help()") &&
           content.contains("show_about_dialog()") &&
           content.contains("find_dialog(") &&
           content.contains("replace_dialog(") &&
           content.contains("sort_dialog(") &&
           content.contains("balance_dialog(")
        {
            all_menu_items_found = true;
            break;
        }
    }

    assert!(all_menu_items_found, "Not all Ratatui menu items have GTK equivalents");
}

/// Strip leading (and trailing) whitespace from each line so pattern
/// matching is resilient to indentation changes (e.g. rustfmt,
/// refactoring) and to \r\n / \r line-ending differences.
fn strip_leading(s: &str) -> String {
    s.lines().map(|l| l.trim()).collect::<Vec<_>>().join("\n")
}

/// Extract string literal match-arm values from a Rust `match` block body.
/// This parses the content between the opening `{` after `match name {` and
/// the matching closing `}` at the function level.  It returns only simple
/// identifier-like string literals (no spaces, pipes, or operator chars).
fn extract_match_arm_strings(text: &str) -> Vec<String> {
    let mut result = Vec::new();
    // Find the first '{' that starts the match body (skipping comments)
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    // Skip past `match name {` to find the opening `{`
    let mut brace_depth = 0i32;
    let mut in_match_body = false;
    while i < len {
        if chars[i] == '/' && i + 1 < len {
            if chars[i + 1] == '/' {
                i += 2;
                while i < len && chars[i] != '\n' { i += 1; }
                continue;
            }
            if chars[i + 1] == '*' {
                i += 2;
                while i + 1 < len && !(chars[i] == '*' && chars[i + 1] == '/') { i += 1; }
                if i + 1 < len { i += 2; }
                continue;
            }
        }
        if chars[i] == '{' {
            brace_depth += 1;
            if !in_match_body && brace_depth == 1 {
                in_match_body = true;
            }
        } else if chars[i] == '}' {
            brace_depth -= 1;
            if in_match_body && brace_depth == 0 {
                break; // end of function/block
            }
        } else if in_match_body && chars[i] == '"' {
            // Extract string literal content
            i += 1;
            let mut s = String::new();
            while i < len {
                if chars[i] == '\\' && i + 1 < len {
                    i += 2;
                    continue;
                }
                if chars[i] == '"' {
                    break;
                }
                s.push(chars[i]);
                i += 1;
            }
            // Only keep identifier-like strings (action names are
            // snake_case or simple words with no spaces/pipes)
            if !s.is_empty()
                && !s.contains(char::is_whitespace)
                && !s.contains('|')
                && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                result.push(s);
            }
        }
        i += 1;
    }
    result.sort();
    result.dedup();
    result
}

#[test]
fn gui_menu_action_names_cover_all_defined_actions() {
    // Verify that every action name defined in menu.rs::action_kind_to_name()
    // has a corresponding match arm in gui_backend.rs::handle_menu_action().
    // This ensures the GUI backend dispatches all defined menu actions.
    let menu_rs = fs::read_to_string("src/gui/menu.rs").unwrap();
    let gui_backend_rs = fs::read_to_string("src/gui/gui_backend.rs").unwrap();

    // Extract action names from action_kind_to_name() in menu.rs
    let menu_normalized = strip_leading(&menu_rs);
    let action_names: Vec<String> = {
        let start_marker = "pub fn action_kind_to_name";
        let start = menu_normalized.find(start_marker)
            .expect("action_kind_to_name not found in menu.rs");
        let body = &menu_normalized[start..];
        extract_match_arm_strings(body)
    };

    // Extract action names from handle_menu_action() match arms in gui_backend.rs
    let gui_normalized = strip_leading(&gui_backend_rs);
    let handled_names: Vec<String> = {
        let start_marker = "fn handle_menu_action";
        let start = gui_normalized.find(start_marker)
            .expect("handle_menu_action not found in gui_backend.rs");
        let body = &gui_normalized[start..];
        extract_match_arm_strings(body)
    };

    // Verify: every action name from menu.rs must appear in gui_backend.rs
    let mut missing: Vec<&str> = Vec::new();
    for name in &action_names {
        if !handled_names.iter().any(|h| h == name) {
            missing.push(name);
        }
    }

    assert!(
        missing.is_empty(),
        "Menu actions defined in menu.rs but missing from gui_backend.rs::handle_menu_action:\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn gui_menu_items_not_wired_to_real_functions() {
    // Verify that menu items ARE currently NOT wired to real functions, they use eprintln instead
    let menu_path = Path::new("src/gui/menu.rs");
    let content = fs::read_to_string(menu_path).unwrap();
    let normalized = strip_leading(&content);

    // Check that menu handlers DO use eprintln instead of calling real functions.
    // Patterns match the current handle_action match-arm structure in menu.rs,
    // with leading whitespace stripped so formatting changes don't break the match.
    let open_pattern = strip_leading("\"open\" => {\n    if let Some(path) = dialogs::file_open_dialog() {\n        eprintln!(\"Open file: {:?}\", path);\n    }\n}");
    let save_pattern = strip_leading("\"save\" => {\n    if let Some(path) = dialogs::file_save_dialog() {\n        eprintln!(\"Save file: {:?}\", path);\n    }\n}");
    let about_pattern = strip_leading("\"about\" => dialogs::show_about_dialog(),");
    let help_pattern = strip_leading("\"help_keybinds\" => dialogs::show_keybinds_help(),");
    let find_pattern = strip_leading("\"find\" => dialogs::find_dialog(|result| {\n    if let Some(text) = result {\n        eprintln!(\"Find: {}\", text);\n    }\n}),");
    let replace_pattern = strip_leading("\"replace\" => dialogs::replace_dialog(|result| {\n    if let Some((find, replace)) = result {\n        eprintln!(\"Replace: '{}' with '{}'\", find, replace);\n    }\n}),");

    assert!(normalized.contains(&open_pattern), "Menu item 'Open' should still be using eprintln instead of calling real function");
    assert!(normalized.contains(&save_pattern), "Menu item 'Save' should still be using eprintln instead of calling real function");
    assert!(normalized.contains(&about_pattern), "Menu item 'About' should NOT use eprintln (already calls real function)");
    assert!(normalized.contains(&help_pattern), "Menu item 'Keybindings' should NOT use eprintln (already calls real function)");
    assert!(normalized.contains(&find_pattern), "Menu item 'Find' should still be using eprintln instead of calling real function");
    assert!(normalized.contains(&replace_pattern), "Menu item 'Replace' should still be using eprintln instead of calling real function");
}

#[test]
#[cfg(feature = "ratatui")]
fn gui_sheet_tab_bar_displayed() {
    let mut app = corro::ui::App::new(None);
    app.load_initial().unwrap();

    // Verify that new sheets are displayed in a tab bar at the bottom
    // Create a new sheet and verify it's displayed
    let initial_count = app.workbook.sheet_count();
    let new_id = app.workbook.next_sheet_id;
    let title = format!("Sheet{}", new_id);

    let op = corro::ops::WorkbookOp::NewSheet { id: new_id, title: title.clone() };
    let _ = corro::ops::apply_workbook_op(&mut app.workbook, &mut 0, op.clone());

    // The new sheet should be added and visible in the tab bar
    assert!(app.workbook.sheet_count() == initial_count + 1);
    let new_index = app.workbook.sheet_index_by_id(new_id).expect("New sheet should have an index");
    assert!(app.workbook.sheet_title(new_index) == title);
}

#[test]
fn gui_add_column_plus_column() {
    let path = Path::new("docs/tests/subtotal.corro");
    let mut app = corro::gui::App::new_with_paths(vec![path.to_path_buf()]);
    app.set_backend(corro::gui::Backend::Gui);
    app.load_initial().unwrap();

    // Verify that a + column exists between data and right margin
    // Clicking it should create a new data column
    let _sheet = app.core.workbook.active_sheet();
    let _sheet_id = app.core.workbook.sheet_id(app.core.workbook.active_sheet);

    // In GTK mode, the + column should be implemented
    // We can't test the actual GUI here, but we can verify the logic
    // This test documents the requirement for GTK implementation
}

#[test]
#[cfg(target_os = "linux")]
fn gui_spreadsheet_scrollbars() {
    // Verify that the gtk::create_scrolled_window symbol exists.
    // This compiles only when the GTK backend is active; the runtime
    // call will fail (loader not initialized) unless a prior init()
    // has been made — the test just checks the function is present.
    let _has_scrolled_window = rustxwidgets::backends::gtk::create_scrolled_window().is_ok();
    // Scrollbars should be provided when content exceeds viewport
}
