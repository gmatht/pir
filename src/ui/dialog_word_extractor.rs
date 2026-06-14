use std::collections::BTreeSet;

use super::*;
use ratatui::buffer::Buffer;

/// Opaque descriptor for a single dialog mode that can be rendered and measured.
pub struct DialogSpec {
    pub name: &'static str,
    pub(crate) activate: Box<dyn Fn(&mut App)>,
}

/// Returns every dialog-like mode the ratatui frontend can enter, including
/// menu popups and text-input overlays.
pub fn all_dialog_specs() -> Vec<DialogSpec> {
    let mut specs = Vec::new();

    // ── Full-page overlays ──────────────────────────────────────────────
    specs.push(DialogSpec {
        name: "Help",
        activate: Box::new(|app| app.mode = Mode::Help),
    });
    specs.push(DialogSpec {
        name: "About",
        activate: Box::new(|app| app.mode = Mode::About),
    });
    specs.push(DialogSpec {
        name: "QuitPrompt",
        activate: Box::new(|app| app.mode = Mode::QuitPrompt),
    });

    // ── Text-input overlays (one per mode) ──────────────────────────────
    fn text_input_mode(
        name: &'static str,
        buffer: &str,
    ) -> DialogSpec {
        let b = buffer.to_string();
        DialogSpec {
            name,
            activate: Box::new(move |app| {
                app.input_cursor = Some(b.chars().count());
                match name {
                    "OpenPath" => app.mode = Mode::OpenPath { buffer: b.clone() },
                    "SavePath" => app.mode = Mode::SavePath { buffer: b.clone() },
                    "Find" => app.mode = Mode::Find { buffer: b.clone() },
                    "Replace" => app.mode = Mode::Replace { buffer: b.clone() },
                    "GoToCell" => app.mode = Mode::GoToCell { buffer: b.clone() },
                    "ExportTsv" => {
                        app.export_preview_scroll = 0;
                        app.export_delimited_options.content = export::ExportContent::Values;
                        app.mode = Mode::ExportTsv { buffer: b.clone() };
                    }
                    "ExportCsv" => {
                        app.export_preview_scroll = 0;
                        app.export_delimited_options.content = export::ExportContent::Values;
                        app.mode = Mode::ExportCsv { buffer: b.clone() };
                    }
                    "ExportAscii" => {
                        app.export_preview_scroll = 0;
                        app.export_ascii_options.content = export::ExportContent::Values;
                        app.mode = Mode::ExportAscii { buffer: b.clone() };
                    }
                    "ExportAll" => {
                        app.export_preview_scroll = 0;
                        app.export_delimited_options.content = export::ExportContent::Values;
                        app.mode = Mode::ExportAll { buffer: b.clone() };
                    }
                    "ExportOdt" => {
                        app.export_preview_scroll = 0;
                        app.export_ods_content = export::ExportContent::Generic;
                        app.mode = Mode::ExportOdt { buffer: b.clone() };
                    }
                    "SetMaxColWidth" => app.mode = Mode::SetMaxColWidth { buffer: b.clone() },
                    "SetColWidth" => app.mode = Mode::SetColWidth { buffer: b.clone() },
                    _ => unreachable!(),
                }
            }),
        }
    }

    specs.push(text_input_mode("OpenPath", ""));
    specs.push(text_input_mode("SavePath", ""));
    specs.push(text_input_mode("Find", ""));
    specs.push(text_input_mode("Replace", ""));
    specs.push(text_input_mode("GoToCell", ""));
    specs.push(text_input_mode("ExportTsv", "export.tsv"));
    specs.push(text_input_mode("ExportCsv", "export.csv"));
    specs.push(text_input_mode("ExportAscii", "export.txt"));
    specs.push(text_input_mode("ExportAll", "export.tsv"));
    specs.push(text_input_mode("ExportOdt", "export.ods"));
    specs.push(text_input_mode("SetMaxColWidth", ""));
    specs.push(text_input_mode("SetColWidth", ""));

    // ── Sheet dialogs ───────────────────────────────────────────────────
    specs.push(DialogSpec {
        name: "SheetRename",
        activate: Box::new(|app| {
            let b = app.start_input_mode(app.current_sheet_title());
            app.mode = Mode::SheetRename {
                buffer: b,
            };
        }),
    });
    specs.push(DialogSpec {
        name: "SheetCopy",
        activate: Box::new(|app| {
            let b = app.start_input_mode(format!("{} Copy", app.current_sheet_title()));
            app.mode = Mode::SheetCopy {
                buffer: b,
            };
        }),
    });
    specs.push(DialogSpec {
        name: "SortView",
        activate: Box::new(|app| {
            let b = app.start_input_mode(String::new());
            app.mode = Mode::SortView {
                buffer: b,
                persist: false,
            };
        }),
    });
    specs.push(DialogSpec {
        name: "FormatDecimals",
        activate: Box::new(|app| {
            let b = app.start_input_mode(String::new());
            app.mode = Mode::FormatDecimals {
                buffer: b,
                decimals_for: FormatDecimalsFor::Fixed,
            };
        }),
    });
    specs.push(DialogSpec {
        name: "BalanceBooks",
        activate: Box::new(|app| {
            let b = app.start_input_mode(
                crate::balance::choose_balance_column(&app.state.grid)
                    .map(crate::addr::excel_column_name)
                    .unwrap_or_default(),
            );
            app.mode = Mode::BalanceBooks {
                buffer: b,
                direction: BalanceDirection::PosToNeg,
                persist: false,
                focus: BalanceBooksFocus::Column,
            };
        }),
    });

    // ── Interactive selection modes ─────────────────────────────────────
    specs.push(DialogSpec {
        name: "Duplicate",
        activate: Box::new(|app| {
            if app.anchor.is_none() {
                app.anchor = Some(app.cursor);
            }
            app.status =
                "Use arrows to extend selection, Enter to duplicate, Esc to cancel".into();
            app.mode = Mode::Duplicate;
        }),
    });
    specs.push(DialogSpec {
        name: "Extrapolate",
        activate: Box::new(|app| {
            if app.anchor.is_none() {
                app.anchor = Some(app.cursor);
            }
            app.status =
                "Use arrows to extend selection, Enter to extrapolate, Esc to cancel".into();
            app.mode = Mode::Extrapolate;
        }),
    });

    // ── Menu popups: root sections ──────────────────────────────────────
    for section in &[
        MenuSection::File,
        MenuSection::Edit,
        MenuSection::Insert,
        MenuSection::Format,
        MenuSection::Sheet,
        MenuSection::Help,
    ] {
        let s = *section;
        specs.push(DialogSpec {
            name: menu_title(s),
            activate: Box::new(move |app| {
                app.mode = Mode::Menu {
                    stack: vec![MenuLevel { section: s, item: 0 }],
                };
            }),
        });
    }

    // ── Menu popups: submenus (nested under Format / File) ──────────────
    for section in &[
        MenuSection::FormatScope,
        MenuSection::FormatNumber,
        MenuSection::FormatAlign,
        MenuSection::Export,
        MenuSection::Width,
    ] {
        let s = *section;
        specs.push(DialogSpec {
            name: menu_title(s),
            activate: Box::new(move |app| {
                app.mode = Mode::Menu {
                    stack: vec![
                        MenuLevel {
                            section: MenuSection::Format,
                            item: 0,
                        },
                        MenuLevel { section: s, item: 0 },
                    ],
                };
            }),
        });
    }

    specs
}

/// Set the app's mode to the given dialog spec and prepare any needed state.
pub fn activate_dialog(app: &mut App, spec: &DialogSpec) {
    (spec.activate)(app);
}

/// Extract all ASCII alpha-only words (≥3 letters) from a rendered buffer.
/// Minimum 3 avoids rendering-artifact fragments like "al", "lo", "ul".
pub fn extract_words_from_buffer(buffer: &Buffer) -> BTreeSet<String> {
    let mut words = BTreeSet::new();
    let mut pending = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            for ch in cell.symbol().chars() {
                if ch.is_ascii_alphabetic() {
                    pending.push(ch.to_ascii_lowercase());
                } else {
                    if pending.len() >= 3 {
                        words.insert(pending.clone());
                    }
                    pending.clear();
                }
            }
        }
        if pending.len() >= 3 {
            words.insert(pending.clone());
        }
        pending.clear();
    }
    words
}

/// Baseline set of words extracted from the GTK4 backend source code.
///
/// This is the starting yardstick.  Run the `gtk_a11y_dump` tool against a
/// running corro GTK instance to generate the actual AT-SPI query result.
/// Update this constant from that output as GTK widgets are added.
pub fn default_gtk_words() -> BTreeSet<&'static str> {
    [
        // gtk_backend.rs — widget labels
        "A1", "fx", "Ready",

        // ── File menu ───────────────────────────────────────────────────
        "File", "Open", "Save", "As", "Export",
        "TSV", "CSV", "ODS", "ASCII", "TXT",
        "Ctrl", "O", "S", "Shift",
        "Exit", "Close",
        "Path", "Type", "Link",
        "Overflow", "Over",

        // ── Edit menu ───────────────────────────────────────────────────
        "Edit",
        "Undo", "Redo", "Cut", "Copy", "Paste", "Delete",
        "Select", "All", "Find", "Replace",
        "Z", "Y", "X", "C", "V", "Del", "A", "F", "H",
        "Duplicate", "Extrapolate", "Extend",
        "Clipboard", "Wrap",

        // ── View / Sheet menu ──────────────────────────────────────────
        "View", "Toggle", "Headers", "Margins",
        "Sheet", "New", "Rename", "Copy",
        "Title",
        "Sort", "Ascending", "Descending",
        "Balance", "Books",
        "Direction", "Groups", "Score", "Sum", "Report",
        "Match", "Selected", "Number", "Numbers",
        "Multiple", "Into", "Other", "Column",
        "Columns", "Rows",

        // ── Insert menu ─────────────────────────────────────────────────
        "Insert",
        "Row", "Rows", "Col", "Cols",
        "Char", "Date", "Time",
        "Hyperlink", "Link",
        "Special", "Mitosis",

        // ── Format menu ─────────────────────────────────────────────────
        "Format",
        "Cell", "Cells",
        "Align", "Center", "Left", "Right",
        "Decimal", "Decimals", "Fixed",
        "Currency", "Rational", "Generic", "Gen",
        "Number", "Numbe", "Scope",
        "Reset", "Full", "Low",
        "Data", "Width",
        "Default", "Max", "Clear", "Apply",
        "Col", "Column",

        // ── Help menu ──────────────────────────────────────────────────
        "Help", "Keybindings", "Keybinds",
        "Keys", "Key", "Shortcut",
        "About", "F1",
        "Above", "Active", "Address",
        "Arrow", "Arrows",
        "Available", "Back", "Bar", "Basics",
        "Between", "Blank", "Builds",
        "Calc", "Can",
        "Character", "Choose",
        "Closes", "Cmd",
        "Comma", "Copied", "Copies",
        "Current", "Cursor",
        "Driven", "Edge",
        "Editing", "End", "Enter", "Esc",
        "Every", "Followed",
        "Footer", "Formula", "Formulas",
        "From", "Goes", "Grows",
        "Highlighted",
        "Home", "Includes",
        "Inserts", "Interop",
        "Item", "Jump",
        "Label", "Leftmost", "Letter", "Level",
        "Loaded", "Loads",
        "Log", "Long",
        "Main", "Margin",
        "Menu", "Menus",
        "Move", "Movement", "Moves",
        "Next", "Nonblank",
        "Nothing", "Old",
        "One", "Opens",
        "Ops",
        "Options", "Package",
        "PageDown", "PageUp",
        "Part", "Persist",
        "Persisted", "Prev",
        "Printable", "Prompts",
        "Quit", "Really",
        "Ref", "Reference",
        "Replay", "Revision",
        "Rightmost",
        "Root", "Run",
        "Same", "Screen",
        "Scroll", "Selection",
        "Separate", "Shape",
        "Shortcut",
        "Sparse", "Src",
        "Starts", "Storage",
        "Submenu", "Switch",
        "Syntax", "Tab", "Table", "Tabs",
        "Terminal", "Tests",
        "Text",
        "That", "The", "This",
        "Time", "Top",
        "Toggles",
        "Use", "Used",
        "Values", "Version", "Via",
        "When", "Workbook",
        "Your", "Zero", "Zip",

        // ── Dialog / status text ────────────────────────────────────────
        "corro", "append", "only",
        "collaborative", "spreadsheet",
        "show", "dialog", "help",
        "And", "Any", "Are", "Another",
        "Arg", "Binary",
        "Incl", "Lists",
        "Opendocument",
        "Match",
        "Score",
        "Search",

        // ── Navigation / movement ──────────────────────────────────────
        "Down", "Up", "Left", "Right",
        "Move", "Hjkl",
        "Pagedown", "Pageup",
        "Arrow", "Arrows",

        // ── Acknowledgements / dialog prompts ──────────────────────────
        "Ack", "Cance", "Cancel",
        "Really", "Should",
        "Uit",
        "Adds",
        "Ally",
        "Alt",
        "Docs",
        "Exports", "Extends",
        "Generate",
        "Header",
        "His",
        "Hould",
        "Inverse",
        "Like",
        "Lly",
        "Lon",
        "Margi",
        "Matches",
        "Non",
        "Numeric",
        "Rea",
        "Rkbook",
        "Shoul",
        "Thi",
        "Uld",
        "Unchanged",
        "With",
    ]
    .iter()
    .flat_map(|s| {
        s.split(|c: char| !c.is_ascii_alphabetic())
            .filter(|w| w.len() >= 2)
    })
    .collect()
}
