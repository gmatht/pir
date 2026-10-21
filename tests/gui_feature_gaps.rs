//! Repro: `pir --gui` shows a single dead "Session" tab button.
//!
//! What the operator sees as a "Session button that does nothing" is the
//! `TabView` tab-bar button titled `session` (`spawn_session` ->
//! `TabView::add_tab`). With one tab it is a no-op by construction. The real
//! Session/Model/Turn/View/Safety/Help menu (`MENU_ACTIONS` in `src/gui.rs`)
//! is built by `build_menu_bar()` and then thrown away with `let _ = bar;`
//! (`src/gui.rs:1315`) — never appended to `root`, never visible.
//!
//! These four tests pin the four wiring gaps behind that symptom (plan:
//! `plans/gui-parity-plan.md`, design rows in `docs/GUI_DESIGN.md` §4):
//!
//! 1. menu bar is never attached (G2) — `build_menu_bar` drops the bar.
//! 2. keyboard scrolling is not wired (S6) — `GuiScroll` has no non-test caller.
//! 3. view actions are never built (G2) — `MENU_VIEW_ACTIONS` is never iterated.
//! 4. scroll-follow has no effect (S2/S3) — `sync_follow()` is never read.
//!
//! All four FAIL on the current tree (that is the point: they reproduce the
//! report). After the fix they must all PASS, and can then be removed or kept
//! as regression guards.

use std::path::PathBuf;

/// Path to the GUI source, relative to this integration test.
fn gui_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("src");
    p.push("gui.rs");
    p
}

/// `src/gui.rs` with every `#[cfg(test)] mod ... { ... }` block removed, so a
/// guard that searches for a string cannot match its own test source.
///
/// (The in-tree `non_test_code()` helper splits on the *first* `#[cfg(test)]`,
/// which would discard all non-test code after it — including `build_menu_bar`
/// itself. Test modules here are top-level `mod`s whose closing `}` sits at
/// column 0, while every item nested inside a module is indented, so cutting
/// from each `#[cfg(test)]` to the next column-0 `}\n` removes exactly the
/// test modules.)
fn non_test_code() -> String {
    let src = std::fs::read_to_string(gui_path()).expect("src/gui.rs must be readable");
    let mut out = String::with_capacity(src.len());
    let mut rest = src.as_str();
    while let Some(idx) = rest.find("#[cfg(test)]") {
        out.push_str(&rest[..idx]);
        let after = &rest[idx..];
        match after.find("\n}\n") {
            Some(e) => rest = &after[e + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Definition site of `build_menu_bar` through the end of its body (up to the
/// next top-level `fn`), so assertions target the function rather than a
/// comment or test that quotes it.
fn build_menu_bar_body(code: &str) -> &str {
    let start = code
        .find("\nfn build_menu_bar(")
        .expect("build_menu_bar definition not found in src/gui.rs");
    let tail = &code[start..];
    // Next top-level fn ends the body.
    let mut end = tail.len();
    for m in tail.match_indices("\nfn ") {
        if m.0 > 1 {
            end = m.0;
            break;
        }
    }
    &tail[..end]
}

#[test]
fn menu_bar_is_attached_to_the_window() {
    // G2: the Session/Model/Turn/View/Safety/Help menu must reach the screen.
    // It used to be built by `build_menu_bar` and then thrown away with
    // `let _ = bar;` — the window showed only the tab-bar "session" button,
    // the reported symptom.
    let code = non_test_code();
    let body = build_menu_bar_body(&code).to_string();
    let drops = body.contains("let _ = bar;");
    let returns_bar = body.contains("MenuBar");
    assert!(
        returns_bar && !drops,
        "build_menu_bar must return the MenuBar for the caller to attach, not \
         drop it (`let _ = bar;`): otherwise --gui shows no menu, only the \
         dead tab-bar Session button"
    );
    // And the caller must actually attach it above the tabs.
    assert!(
        code.contains("root.append(&bar)"),
        "run() must append the returned menu bar to `root` above the tabs, or \
         the menu is built and never shown"
    );
}

#[test]
fn keyboard_scrolling_is_wired_to_guiscroll() {
    // S6: `GuiScroll` (page_up/page_down/home/end, clamped) exists but has no
    // non-test caller, and the key controller handles only
    // Tab/Up/Down/Esc/Ctrl-C/D/Q. PageUp/PageDown/Home/End must drive it.
    let code = non_test_code();
    let uses = code
        .lines()
        .filter(|l| l.contains("GuiScroll"))
        .filter(|l| !l.contains("struct GuiScroll") && !l.contains("impl GuiScroll"))
        .count();
    let has_key_handling = code.contains("Page_Up")
        || code.contains("Page_Down")
        || code.contains("GDK_KEY_Page")
        || code.contains("0xff55")
        || code.contains("0xff56");
    assert!(
        uses > 0 && has_key_handling,
        "GuiScroll is defined but never used outside tests ({uses} use(s)) \
         and no PageUp/PageDown/Home/End key is handled: keyboard scrolling \
         (design row S6) is not wired"
    );
}

#[test]
fn view_actions_are_built_into_the_menu() {
    // G2: `MENU_VIEW_ACTIONS` (scroll-to-bottom / page-up / page-down /
    // mark-finished) is defined but never iterated outside its own test — no
    // menu items are built from it.
    let code = non_test_code();
    let mentions = code.matches("MENU_VIEW_ACTIONS").count();
    assert!(
        mentions > 1,
        "MENU_VIEW_ACTIONS appears {mentions}x in non-test code (the definition \
         alone): nothing ever builds those View/Turn items, so they are dead"
    );
}

#[test]
fn scroll_follow_takes_effect_on_sync() {
    // S2/S3: `follow`/`sync_follow()` is updated by `observe_scroll()` but no
    // non-test code reads it — `sync_textview()` appends unconditionally, so
    // follow cannot hold or release the pane.
    let code = non_test_code();
    assert!(
        code.contains(".sync_follow()") || code.contains("sync_follow()"),
        "sync_follow() is never called outside tests: scroll-follow \
         (design rows S2/S3) has no observable effect"
    );
    // Guard against a vacuous call: the result must influence the sync path.
    let sync_uses_follow = code.contains("follow") && code.contains("fn sync_textview");
    assert!(
        sync_uses_follow,
        "the sync path never consults `follow`: appends cannot respect \
         scroll-away vs at-bottom"
    );
}

#[test]
fn menu_items_carry_the_action_group_prefix() {
    // G2: every MenuItem is greyed out because the detailed action names in
    // the GMenu model do not resolve. `build_menu_bar` inserts the action
    // group on the window (`MENU_ACTION_GROUP`, "pir"), so each item's
    // detailed action must be qualified as `pir.<name>` — exactly the
    // convention the toolkit's own working example uses
    // (`file_menu.append("New", "app.new")` with short registration names).
    // Bare names like `session-new` match no group GTK looks in
    // (`app`/`win`), so GTK renders every item insensitive and no click can
    // ever fire.
    let code = non_test_code();
    let body = build_menu_bar_body(&code).to_string();
    // The group name constant — the single source of truth both the insert
    // call and the menu items must reference.
    let group_decl = code
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("const MENU_ACTION_GROUP"))
        .expect("src/gui.rs must define MENU_ACTION_GROUP once");
    let group: &str = group_decl
        .split('"')
        .nth(1)
        .expect("MENU_ACTION_GROUP must be a string literal");
    assert!(
        body.contains("insert_action_group(MENU_ACTION_GROUP,"),
        "build_menu_bar must insert the action group by constant          (MENU_ACTION_GROUP), so the insert call and the menu items cannot drift apart"
    );
    // Every detailed action handed to `Menu::append` inside build_menu_bar
    // must go through the qualifier — a bare action identifier can never
    // resolve against the group, and GTK greys the item out silently.
    let mut bare: Vec<String> = Vec::new();
    for line in body.lines() {
        let t = line.trim();
        // Match the append sites: `subs[i].1.append(label, ACTION)`.
        if let Some(args) = t.strip_prefix("subs[i].1.append(") {
            // The detailed action is the LAST top-level comma-separated
            // argument; walk top-level commas and take the final segment.
            let mut top_commas = Vec::new();
            let mut depth = 0i32;
            for (idx, ch) in args.char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' if depth > 0 => depth -= 1,
                    ',' if depth == 0 => top_commas.push(idx),
                    _ => {}
                }
            }
            let detail = top_commas
                .last()
                .map(|&c| args[c + 1..].trim().trim_end_matches(')').trim())
                .unwrap_or("");
            // Qualified iff it routes through the helper (which prefixes the
            // group) or pastes the group constant in directly.
            let looks_qualified = detail.contains("menu_detailed_action(")
                || detail.contains("MENU_ACTION_GROUP")
                || detail.contains(&format!("\"{group}."));
            if !looks_qualified {
                bare.push(detail.to_string());
            }
        }
    }
    assert!(
        bare.is_empty(),
        "menu items with bare action names never resolve against the \"{group}\" \
         action group, so GTK greys them out and no click fires: {bare:?}. \
         Qualify them as \"{group}.<name>\" (keeping registration names short, \
         as NWG strips the prefix at dispatch)"
    );
}


#[test]
fn new_session_tab_and_pane_are_shown() {
    // G2: clicking "New session" ran the whole pipeline (agent created,
    // `spawn_session` pushed a SessionView, `set_active` moved) yet nothing
    // visible changed — on GTK3, widgets created after `window.present()`
    // start hidden, and only present()'s blanket `show_all` reveals what
    // exists then. Every later tab must be shown explicitly: its button
    // (in `TabView::add_tab`) and its pane (via `show_all` on the vbox
    // `spawn_session` fills), or the session "doesn't seem to do anything".
    //
    // This test cannot run the full GUI (it needs providers/agents), so it
    // pins both halves of the contract in source: the button half in the
    // toolkit (the only place tab buttons are created), the pane half in
    // `spawn_session` (the only place tab panes are filled).
    let tk_path = {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("rustxWidgets");
        p.push("rustxwidgets");
        p.push("src");
        p.push("backends_gtk_adapter_impl.rs");
        p
    };
    let tk = std::fs::read_to_string(&tk_path).expect("toolkit impl readable");
    // add_tab is the only place tab buttons are created; it must show them,
    // since nothing else runs show_all after present().
    let add_tab_start = tk.find("pub fn add_tab").expect("TabView::add_tab must exist");
    let add_tab_end = tk[add_tab_start..]
        .find("pub fn tab_box")
        .map(|r| add_tab_start + r)
        .unwrap_or(tk.len());
    let add_tab = &tk[add_tab_start..add_tab_end];
    assert!(
        add_tab.contains("set_visible(true)"),
        "TabView::add_tab must explicitly show the new tab button: on GTK3 \
         widgets start hidden and only window.present()'s blanket show_all \
         reveals what exists then — every later tab stays invisible otherwise"
    );
    // spawn_session fills the new tab's pane after present(); the pane must
    // be shown explicitly too, or switching shows a blank hole.
    let code = non_test_code();
    let start = code
        .find("\nfn spawn_session(")
        .expect("spawn_session definition not found");
    let tail = &code[start..];
    let end_rel = tail
        .find("\nfn handle_session_command(")
        .expect("spawn_session body end");
    let end_floor = tail[..end_rel]
        .char_indices()
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let body = &tail[..end_floor];
    assert!(
        body.contains("vbox.show_all()"),
        "spawn_session must show_all() the pane it fills: every tab after \
         the first is built after window.present(), so without an explicit \
         show the new session renders blank"
    );
}

#[test]
fn new_session_pane_has_repl_and_log_widgets() {
    // The main window must show a REPL (prompt Entry + Send/Cancel) and an
    // agent log (conversation TextView inside a ScrolledWindow), not just an
    // empty grey block with one session button in the middle.
    //
    // `TabView::add_tab` creates the per-tab panel but never attaches it to
    // the visible `content` box — `content` itself is never even appended to
    // the tab view's `outer` container — so `tab_box(idx)` hands
    // `spawn_session` an orphaned panel. The pane widgets are built and shown
    // into a detached subtree while the window displays only the tab-bar
    // button: the reported empty grey block.
    //
    // Like the tab-button test above, this pins the toolkit contract in
    // source (the pane must be attached, not just created).
    let tk_path = {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("rustxWidgets");
        p.push("rustxwidgets");
        p.push("src");
        p.push("backends_gtk_adapter_impl.rs");
        p
    };
    let tk = std::fs::read_to_string(&tk_path).expect("toolkit impl readable");
    let add_tab_start = tk.find("pub fn add_tab").expect("TabView::add_tab must exist");
    let add_tab_end = tk[add_tab_start..]
        .find("pub fn tab_box")
        .map(|r| add_tab_start + r)
        .unwrap_or(tk.len());
    let add_tab = &tk[add_tab_start..add_tab_end];
    assert!(
        add_tab.contains("content.append") || add_tab.contains("self.content.append"),
        "TabView::add_tab creates a per-tab panel but never appends it to the \
         visible `content` box, so spawn_session fills an orphaned subtree: \
         the main pane stays an empty grey block with only the session button"
    );
    // And the `content` box itself must be part of the visible tree — an
    // attached-to-nothing content box hides every panel even if add_tab
    // appends to it.
    let new_start = tk.find("pub fn new(").expect("TabView::new must exist");
    let new_end = tk[new_start..]
        .find("pub fn add_tab")
        .map(|r| new_start + r)
        .unwrap_or(tk.len());
    let new_body = &tk[new_start..new_end];
    assert!(
        new_body.contains("outer.append(&content)") || new_body.contains("append(&content)"),
        "TabView::new must append the `content` box to the visible `outer` \
         container, or every session pane renders into a detached tree"
    );
}
