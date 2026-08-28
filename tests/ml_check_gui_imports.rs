use std::fs;
use std::path::Path;

#[test]
fn gui_no_raw_backend_crate_imports() {
    let gui_dir = Path::new("src/gui");
    let banned: &[&str] = &["pancurses", "gtk", "crossterm", "ratatui", "gtk_dynamic_loader"];

    let mut violations: Vec<String> = Vec::new();

    for entry in fs::read_dir(gui_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().map_or(true, |e| e != "rs") {
            continue;
        }
        let fname = path.file_name().unwrap().to_str().unwrap().to_string();
        let content = strip_comments_and_strings(&fs::read_to_string(&path).unwrap());

        for (line_no, line) in content.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Check 1: `use banned_crate` or `use banned_crate::...` (first path segment only)
            if let Some(after_use) = trimmed.strip_prefix("use ") {
                for b in banned {
                    if let Some(rest) = after_use.strip_prefix(b) {
                        if rest.is_empty()
                            || rest.starts_with("::")
                            || rest.starts_with(';')
                            || rest.starts_with(' ')
                            || rest.starts_with('{')
                            || rest.starts_with(':')
                            || rest.starts_with(" as ")
                        {
                            violations.push(format!("{}:{}: {}", fname, line_no + 1, trimmed));
                        }
                    }
                }
            }
            // Check 2: `extern crate banned_crate`
            if let Some(after_extern) = trimmed.strip_prefix("extern crate ") {
                for b in banned {
                    if after_extern.starts_with(b) {
                        violations.push(format!("{}:{}: {}", fname, line_no + 1, trimmed));
                    }
                }
            }
            // Check 3: Fully qualified `banned_crate::` — only when it's the top-level crate,
            // not when nested under rustxwidgets::...
            for b in banned {
                let needle = format!("{}::", b);
                let mut search_start = 0;
                while let Some(pos) = trimmed[search_start..].find(&needle) {
                    let abs_pos = search_start + pos;
                    if abs_pos > 0 {
                        let prev = trimmed.as_bytes()[abs_pos - 1];
                        // Skip if preceded by ::, ., or word char (it's a submodule path)
                        if prev == b':' || prev == b'.' || prev.is_ascii_alphanumeric() || prev == b'_' {
                            search_start = abs_pos + needle.len();
                            continue;
                        }
                    }
                    violations.push(format!("{}:{}: {}", fname, line_no + 1, trimmed));
                    break;
                }
            }
        }
    }

    let v: Vec<&str> = violations.iter().map(|s| s.as_str()).collect();
    assert!(
        violations.is_empty(),
        "src/gui/ contains direct backend crate references (use rustxwidgets instead):\n{}",
        v.join("\n")
    );
}

#[test]
fn gui_shared_files_no_concrete_rustxwidgets_types() {
    let gui_dir = Path::new("src/gui");
    let shared: &[&str] = &[
        "clipboard.rs",
        "edit.rs",
        "keymap.rs",
        "mod.rs",
        "sheet.rs",
    ];
    let concrete_types: &[&str] = &["rustxwidgets::App", "rustxwidgets::Window"];

    let mut violations: Vec<String> = Vec::new();

    for entry in fs::read_dir(gui_dir).unwrap() {
        let path = entry.unwrap().path();
        let fname = path.file_name().unwrap().to_str().unwrap().to_string();
        if !shared.contains(&fname.as_str()) {
            continue;
        }
        if path.extension().map_or(true, |e| e != "rs") {
            continue;
        }
        let content = strip_comments_and_strings(&fs::read_to_string(&path).unwrap());

        for (line_no, line) in content.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            for ty in concrete_types {
                if trimmed.contains(ty) {
                    violations.push(format!("{}:{}: {}", fname, line_no + 1, trimmed));
                }
            }
        }
    }

    let v: Vec<&str> = violations.iter().map(|s| s.as_str()).collect();
    assert!(
        violations.is_empty(),
        "Shared src/gui/ files must not reference concrete rustxwidgets types like App/Window;\
         define internal traits instead:\n{}",
        v.join("\n")
    );
}

fn strip_comments_and_strings(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        match chars[i] {
            '/' if i + 1 < len => match chars[i + 1] {
                '/' => {
                    // line comment: skip to end of line
                    i += 2;
                    while i < len && chars[i] != '\n' {
                        i += 1;
                    }
                    if i < len {
                        out.push('\n');
                        i += 1;
                    }
                }
                '*' => {
                    // block comment: skip to */
                    i += 2;
                    while i + 1 < len && !(chars[i] == '*' && chars[i + 1] == '/') {
                        if chars[i] == '\n' {
                            out.push('\n');
                        }
                        i += 1;
                    }
                    i += 2; // skip */
                }
                _ => {
                    out.push(chars[i]);
                    i += 1;
                }
            },
            '"' => {
                // string literal: skip to closing quote (handling escapes)
                out.push('"');
                i += 1;
                while i < len {
                    if chars[i] == '\\' && i + 1 < len {
                        i += 2; // skip escape sequence
                    } else if chars[i] == '"' {
                        out.push('"');
                        i += 1;
                        break;
                    } else {
                        // replace string content with space to avoid false matches
                        out.push(' ');
                        i += 1;
                    }
                }
            }
            ch => {
                out.push(ch);
                i += 1;
            }
        }
    }
    out
}
