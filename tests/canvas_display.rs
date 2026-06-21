#[cfg(feature = "gui")]
mod canvas_tests {
    use std::cell::Cell;
    use std::ffi::c_void;
    use std::sync::Arc;

    // ---- API-level test ----
    #[test]
    fn canvas_set_draw_callback_api() {
        let loader = gtk_dynamic_loader::Loader::new().expect("Loader::new failed");
        let da = gtk_dynamic_loader::DrawingArea::new(loader.clone())
            .expect("DrawingArea::new failed");

        // set_draw_func should accept and store a closure without crashing
        da.set_draw_func(Box::new(|_cr: *mut c_void, _w: i32, _h: i32| {
            // no-op; just verifying the plumbing
        }))
        .expect("set_draw_func failed");

        // queue_draw should not crash (even without a window)
        da.queue_draw();

        // set_size_request should not crash
        da.set_size_request(400, 300);
    }

    // ---- Full rendering test (requires DISPLAY) ----
    #[test]
    fn canvas_draw_callback_fires() {
        let loader = gtk_dynamic_loader::Loader::new().expect("Loader::new failed");

        // Shared flag to verify the draw callback was invoked
        let drew = Arc::new(Cell::new(false));

        let win = gtk_dynamic_loader::Window::new(loader.clone()).expect("Window::new failed");

        let da = gtk_dynamic_loader::DrawingArea::new(loader.clone())
            .expect("DrawingArea::new failed");
        let drew_clone = drew.clone();
        da.set_draw_func(Box::new(move |_cr: *mut c_void, _w: i32, _h: i32| {
            drew_clone.set(true);
        }))
        .expect("set_draw_func failed");
        da.set_size_request(200, 100);
        da.queue_draw();

        win.set_child(&da);
        win.present();

        // Leak win and da to prevent Drop from running (which would unref GObjects
        // while the main loop or GLib still references them)
        let _win = Box::into_raw(Box::new(win));
        let _da = Box::into_raw(Box::new(da));

        // Run the GLib main loop with an idle callback that checks the flag
        let loop_new = loader.symbols.g_main_loop_new.expect("g_main_loop_new");
        let loop_quit = loader.symbols.g_main_loop_quit.expect("g_main_loop_quit");
        let idle_add = loader.symbols.g_idle_add.expect("g_idle_add");
        let loop_ptr = unsafe { loop_new(std::ptr::null_mut(), 0) };

        type IdleFn = unsafe extern "C" fn(*mut c_void) -> i32;
        type LoopQuit = unsafe extern "C" fn(*mut c_void);

        struct IdleData {
            drew: Arc<Cell<bool>>,
            loop_ptr: *mut c_void,
            loop_quit: LoopQuit,
        }

        unsafe extern "C" fn idle_cb(data: *mut c_void) -> i32 {
            let idle_data = &*(data as *mut IdleData);
            assert!(
                idle_data.drew.get(),
                "Draw callback was NOT invoked during the main loop iteration"
            );
            (idle_data.loop_quit)(idle_data.loop_ptr);
            let _ = Box::from_raw(data as *mut IdleData);
            0
        }

        let idle_data = Box::into_raw(Box::new(IdleData {
            drew: drew.clone(),
            loop_ptr,
            loop_quit,
        }));

        let idle_fn: Option<IdleFn> = Some(idle_cb);
        unsafe { idle_add(idle_fn, idle_data as *mut c_void) };

        unsafe {
            let loop_run = loader.symbols.g_main_loop_run.expect("g_main_loop_run");
            loop_run(loop_ptr);
        }
    }
}
