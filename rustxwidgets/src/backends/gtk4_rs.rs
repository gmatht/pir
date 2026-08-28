use crate::backends::{BackendApp, BackendError};
use std::sync::LazyLock;

static MAIN_LOOP: LazyLock<gtk4::glib::MainLoop> = LazyLock::new(|| {
    gtk4::glib::MainLoop::new(None, false)
});

fn ensure_dlopen() {
    gtk4_sys::__dlopen_ensure_loaded();
    glib_sys::__dlopen_ensure_loaded();
    gio_sys::__dlopen_ensure_loaded();
}

pub struct GtkApp;

impl BackendApp for GtkApp {
    fn run(self: Box<Self>) -> Result<(), BackendError> {
        MAIN_LOOP.run();
        Ok(())
    }
}

pub fn quit_main_loop() {
    MAIN_LOOP.quit();
}

pub fn init() -> Result<Box<dyn BackendApp>, BackendError> {
    ensure_dlopen();
    // Match the old gtk_dynamic_loader env vars: force X11 + software Cairo
    // rendering so that GSK compositing doesn't discard cairo draw_func output.
    if std::env::var("GDK_BACKEND").is_err() {
        std::env::set_var("GDK_BACKEND", "x11");
    }
    if std::env::var("GSK_RENDERER").is_err() {
        std::env::set_var("GSK_RENDERER", "cairo");
    }
    if !gtk4::is_initialized() {
        gtk4::init().map_err(|e| format!("gtk4 init failed: {e}"))?;
    }
    Ok(Box::new(GtkApp))
}
