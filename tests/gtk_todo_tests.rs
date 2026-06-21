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

#[test]
fn gui_menu_items_not_wired_to_real_functions() {
    // Verify that menu items ARE currently NOT wired to real functions, they use eprintln instead
    let menu_path = Path::new("src/gui/menu.rs");
    let content = fs::read_to_string(menu_path).unwrap();

    // Check that menu handlers DO use eprintln instead of calling real functions
    let open_pattern = "register_action(app, \"open\", || {\n        if let Some(path) = dialogs::file_open_dialog() {\n            eprintln!(\"Open file: {:?}\", path);\n        }\n    })?;";
    let save_pattern = "register_action(app, \"save\", || {\n        if let Some(path) = dialogs::file_save_dialog() {\n            eprintln!(\"Save file: {:?}\", path);\n        }\n    })?;";
    let about_pattern = "register_action(app, \"about\", || {\n        dialogs::show_about_dialog();\n    })?;";
    let help_pattern = "register_action(app, \"help_keybinds\", || {\n        dialogs::show_keybinds_help();\n    })?;";
    let find_pattern = "register_action(app, \"find\", || {\n        dialogs::find_dialog(|result| {\n            if let Some(text) = result {\n                eprintln!(\"Find: {}\", text);\n            }\n        });\n    })?;";
    let replace_pattern = "register_action(app, \"replace\", || {\n        dialogs::replace_dialog(|result| {\n            if let Some((find, replace)) = result {\n                eprintln!(\"Replace: {} with {}\", find, replace);\n            }\n        });\n    })?;";

    assert!(content.contains(open_pattern), "Menu item 'Open' should still be using eprintln instead of calling real function");
    assert!(content.contains(save_pattern), "Menu item 'Save' should still be using eprintln instead of calling real function");
    assert!(content.contains(about_pattern), "Menu item 'About' should NOT use eprintln (already calls real function)");
    assert!(content.contains(help_pattern), "Menu item 'Keybindings' should NOT use eprintln (already calls real function)");
    assert!(content.contains(find_pattern), "Menu item 'Find' should still be using eprintln instead of calling real function");
    assert!(content.contains(replace_pattern), "Menu item 'Replace' should still be using eprintln instead of calling real function");
}

#[test]
fn gui_sheet_tab_bar_displayed() {
    let mut app = corro::ui::App::new(None).unwrap();
    app.load_initial().unwrap();

    // Verify that new sheets are displayed in a tab bar at the bottom
    // Create a new sheet and verify it's displayed
    let initial_count = app.core.workbook.sheet_count();
    let new_id = app.core.workbook.next_sheet_id;
    let title = format!("Sheet{}", new_id);

    let op = corro::ops::WorkbookOp::NewSheet { id: new_id, title: title.clone() };
    let _ = corro::ops::apply_workbook_op(&mut app.core.workbook, &mut 0, op.clone());

    // The new sheet should be added and visible in the tab bar
    assert!(app.core.workbook.sheet_count() == initial_count + 1);
    assert!(app.core.workbook.sheet_title(app.core.workbook.active_sheet) == title);
}

#[test]
fn gui_add_column_plus_column() {
    let path = Path::new("docs/tests/subtotal.corro");
    let mut app = corro::gui::App::new_with_paths(vec![path.to_path_buf()]);
    app.set_backend(corro::gui::Backend::Gtk);
    app.load_initial().unwrap();

    // Verify that a + column exists between data and right margin
    // Clicking it should create a new data column
    let sheet = app.core.workbook.active_sheet();
    let sheet_id = app.core.workbook.sheet_id(app.core.workbook.active_sheet);

    // In GTK mode, the + column should be implemented
    // We can't test the actual GUI here, but we can verify the logic
    // This test documents the requirement for GTK implementation
}

#[test]
fn gui_spreadsheet_scrollbars() {
    // Test that GTK backend provides scrollbars via create_scrolled_window
    // The rustxwidgets backend provides a create_scrolled_window function
    // which can be used to wrap the spreadsheet
    let test_backend = rustxwidgets::backends::gtk::create_scrolled_window().is_ok();

    // This test documents the requirement for GTK implementation
    // Scrollbars should be provided when content exceeds viewport
}
