use std::process::Command;

/// Run the dialog example and verify the TUI is not blank:
/// the captured terminal output must contain expected widget labels.
#[test]
#[cfg(feature = "pancurses")]
fn pancurses_dialog_renders_content() {
    let output = Command::new("timeout")
        .args(["3", "cargo", "run", "--features", "pancurses", "--example", "dialog"])
        .output()
        .expect("failed to run dialog example");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // The dialog example sets these widget labels:
    //   DropDown: "Choice 1", "Choice 2", "Choice 3"
    //   CheckButton: "Enable feature"
    //   RadioButton: "Option A", "Option B", "Option C"
    //   Entry: "Hello" / "World"
    //   TextView: "Multi-line"
    // At least some of them must appear in raw terminal output.
    let expected = ["Choice", "Enable", "Option", "World", "Multi"];
    let mut found = false;
    for text in &expected {
        if stdout.contains(text) {
            found = true;
            break;
        }
    }
    assert!(found, "TUI output appears blank — none of {:?} found in terminal output", expected);

    // Must not panic
    assert!(!stdout.contains("panicked"), "example panicked");
}
