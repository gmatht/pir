//! Test that Alt+F → Q (Quit from File menu) causes the app to quit on Windows.
//! Before the fix, `handle_menu_action("quit")` was a no-op on Windows.
//! The fix adds `quit_main_loop()` to the NWG backend which posts `WM_QUIT`.

use rustxwidgets::backends::BackendApp;

#[test]
#[cfg(windows)]
fn quit_main_loop_sets_flag_and_posts_wm_quit() {
    // Verify default state: quit is NOT requested.
    assert!(
        !rustxwidgets::backends::nwg::is_quit_requested(),
        "quit should not be requested before calling quit_main_loop"
    );

    // Call the fix: quit_main_loop sets the flag and posts WM_QUIT.
    rustxwidgets::backends_nwg_adapter::quit_main_loop();

    // Verify the flag is set after calling quit_main_loop.
    assert!(
        rustxwidgets::backends::nwg::is_quit_requested(),
        "quit should be requested after quit_main_loop()"
    );

    // Also verify that a newly created NwgApp::run() returns Ok(())
    // because WM_QUIT was posted to the thread's message queue.
    let app = rustxwidgets::backends::nwg::NwgApp::new().unwrap();
    let result = Box::new(app).run();
    assert!(
        result.is_ok(),
        "NwgApp::run should return Ok(()) when WM_QUIT is queued"
    );
}
