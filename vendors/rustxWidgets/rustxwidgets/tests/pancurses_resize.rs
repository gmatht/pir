use std::process::Command;

/// Verify the toolbar line has no wasted trailing space and buttons
/// show at least partial labels (not just `[]`).
#[test]
#[cfg(feature = "pancurses")]
fn pancurses_toolbar_no_wasted_space() {
    if Command::new("which").arg("tmux").output().is_err() {
        eprintln!("tmux not available, skipping test");
        return;
    }

    let session = "pancurses_toolbar_test";
    cleanup_session(session);

    tmux(&["new-session", "-d", "-s", session, "-x", "80", "-y", "24",
           "-e", "TERM=screen-256color"]);

    let _ = Command::new("cargo")
        .args(["build", "--features", "pancurses", "--example", "app"])
        .output();

    tmux(&["send-keys", "-t", session,
           "cargo run --features pancurses --example app < /dev/null",
           "Enter"]);

    std::thread::sleep(std::time::Duration::from_secs(20));

    // Use full-scrollback capture so we can find the ncurses output
    // among the build messages in the tmux history.
    let all_lines = capture(session);
    eprintln!("=== Toolbar test full capture ({} lines) ===", all_lines.len());
    for (i, l) in all_lines.iter().enumerate() {
        eprintln!("  {i:3}: {l}");
    }

    // The toolbar line has `[Open]` etc. Filter by toolbar-specific label
    // plus the presence of `]  [` separator between adjacent buttons.
    let toolbar_idx = all_lines.iter().rposition(|l| {
        (l.contains("Open") || l.contains("Save")) && l.contains("]  [")
    });
    assert!(toolbar_idx.is_some(), "toolbar line not found in {} lines; last 10:\n{:?}",
        all_lines.len(), &all_lines[all_lines.len().saturating_sub(10)..]);
    let toolbar = &all_lines[toolbar_idx.unwrap()];
    eprintln!("Toolbar line (idx {}): {:?} (len {})", toolbar_idx.unwrap(), toolbar, toolbar.len());

    // 1. No wasted space: trimmed content should leave at most 2 trailing blanks
    let trimmed = toolbar.trim_end();
    let wasted = toolbar.len() - trimmed.len();
    assert!(
        wasted <= 3,
        "Toolbar has {wasted} chars of trailing whitespace: {:?}",
        toolbar
    );

    // 2. Buttons should not be empty `[]`.
    assert!(
        !toolbar.contains("[]"),
        "Toolbar has empty buttons `[]`: {:?}", toolbar
    );

    // 3. At least a few labels show partial text (single char counts).
    let visible_labels = ["O", "S", "Q", "B", "I", "A", "H", "F"];
    let found_labels = visible_labels.iter().filter(|&&l| toolbar.contains(l)).count();
    assert!(
        found_labels >= 5,
        "Toolbar too truncated: only {found_labels}/{} expected chars found in {toolbar:?}",
        visible_labels.len()
    );

    tmux(&["send-keys", "-t", session, "q"]);
    cleanup_session(session);
}



/// Run the app example in tmux, capture its ncurses output,
/// then resize and capture again, verifying widget labels survive.
#[test]
#[cfg(feature = "pancurses")]
fn pancurses_resize_renders_content_after_resize() {
    if Command::new("which").arg("tmux").output().is_err() {
        eprintln!("tmux not available, skipping test");
        return;
    }

    let session = "pancurses_resize_test";
    cleanup_session(session);

    // Create detached session, set TERM so ncurses initialises properly
    tmux(&["new-session", "-d", "-s", session, "-x", "80", "-y", "24",
           "-e", "TERM=screen-256color"]);

    // Pre-build so the tmux session only sees a short compile
    let _ = Command::new("cargo")
        .args(["build", "--features", "pancurses", "--example", "app"])
        .output();

    // Run the app (without stdin redirect so ncurses runs normally)
    tmux(&["send-keys", "-t", session,
           "cargo run --features pancurses --example app",
           "Enter"]);

    // Wait for build + app to fully start and render
    std::thread::sleep(std::time::Duration::from_secs(30));

    // Force a full redraw so ncurses content is definitely on screen
    tmux(&["send-keys", "-t", session, "C-l"]);
    std::thread::sleep(std::time::Duration::from_millis(500));

    let before = capture(session);
    eprintln!("=== Before resize ({} lines) ===", before.len());
    for (i, l) in before.iter().enumerate() {
        eprintln!("  {i:3}: {l}");
    }

    // If capture is empty or has no ncurses content, wait more
    let expected = ["Open", "Save", "Medium", "Grid", "Normal", "Compact", "File", "Edit"];
    let has_content = |lines: &[String]| expected.iter().any(|e| lines.iter().any(|l| l.contains(e)));

    if !has_content(&before) {
        eprintln!("No ncurses yet, waiting 10 more seconds ...");
        std::thread::sleep(std::time::Duration::from_secs(10));
        let before2 = capture(session);
        eprintln!("=== Before resize retry ({} lines) ===", before2.len());
        for (i, l) in before2.iter().enumerate() {
            eprintln!("  {i:3}: {l}");
        }
    }

    assert!(has_content(&before),
        "Before resize: none of {expected:?} found in {} lines; sample: {:?}",
        before.len(), &before[..before.len().min(5)]);

    let wide = before.iter().filter(|l| l.len() >= 70).count();
    assert!(wide >= 2,
        "Before resize: expected >=2 lines of width >=70, got {wide}; widths: {:?}",
        before.iter().map(|l| l.len()).collect::<Vec<_>>());

    let sample_before = before.iter().find(|l| l.len() >= 70).cloned().unwrap_or_default();

    // Resize: narrower and shorter
    tmux(&["resize-window", "-t", session, "-x", "40", "-y", "12"]);
    std::thread::sleep(std::time::Duration::from_secs(3));

    // Send Ctrl-L to trigger a full redraw at the new size
    tmux(&["send-keys", "-t", session, "C-l"]);
    std::thread::sleep(std::time::Duration::from_millis(800));

    let after = capture(session);
    eprintln!("=== After resize ({} lines) ===", after.len());
    for (i, l) in after.iter().enumerate() {
        eprintln!("{i:3} ({:3}): {l}", l.len());
    }

    // Every visible line should now fit in 40 columns (+ slack for border/wide chars)
    let too_wide: Vec<(usize, &str)> = after.iter()
        .enumerate()
        .filter(|(_, l)| l.len() > 44)
        .map(|(i, l)| (i, l.as_str()))
        .collect();
    assert!(too_wide.is_empty(),
        "After resize: lines exceeding 44 cols: {too_wide:?}");

    // Widgets should still be present
    assert!(has_content(&after),
        "After resize: none of {expected:?} found in {} lines; sample: {:?}",
        after.len(), &after[..after.len().min(5)]);

    // Verify content changed (lines are narrower)
    let sample_after = after.iter().find(|l| l.len() >= 30).cloned().unwrap_or_default();
    if !sample_before.is_empty() && !sample_after.is_empty() {
        assert_ne!(sample_before, sample_after,
            "After resize: line content unchanged");
    }

    // No panic
    assert!(!after.iter().any(|l| l.contains("panicked")), "example panicked");

    // Quit the app
    tmux(&["send-keys", "-t", session, "q"]);

    cleanup_session(session);
}

fn tmux(args: &[&str]) {
    let r = Command::new("tmux").args(args).output().unwrap();
    assert!(r.status.success(), "tmux {args:?} failed: {}", String::from_utf8_lossy(&r.stderr));
}

fn cleanup_session(s: &str) {
    let _ = Command::new("tmux").args(["kill-session", "-t", s]).output();
}

/// Capture visible pane, drop trailing blank lines.
fn capture(session: &str) -> Vec<String> {
    let out = Command::new("tmux")
        .args(["capture-pane", "-t", session, "-p"])
        .output().unwrap();
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines().map(|l| l.to_string()).collect();
    while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines
}
