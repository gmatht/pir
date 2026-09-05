use crate::error::Error;
use crate::symbols::Symbols;
use std::collections::HashMap;
use std::sync::Arc;

#[cfg(feature = "gtk4rs")]
use crate::backend_gtk4rs;

#[cfg(unix)]
use libloading::os::unix::Library;
#[cfg(windows)]
use libloading::os::windows::Library;

pub type RawLib = Library;

/// Which GTK we loaded
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version { None, Gtk3, Gtk4 }

pub struct Loader {
    pub libs: HashMap<String, Arc<RawLib>>,
    pub symbols: Arc<Symbols>,
    pub version: Version,
    pub main_loop: std::sync::Mutex<usize>,
}

impl Loader {
    pub fn new() -> Result<Arc<Self>, Error> {
        // Candidate lists (env overrides not implemented yet)
        let glib_cands = ["libglib-2.0.so.0", "libglib-2.0.so"];
        let gobject_cands = ["libgobject-2.0.so.0", "libgobject-2.0.so"];
        let gio_cands = ["libgio-2.0.so.0", "libgio-2.0.so"];
        let pango_cands = ["libpango-1.0.so.0", "libpango-1.0.so"];
        let gtk4_cands = ["libgtk-4.so.1", "libgtk-4.so"];
        let gtk3_cands = ["libgtk-3.so.0", "libgtk-3.so"];

        // Open libglib
        let mut libs: HashMap<String, Arc<RawLib>> = HashMap::new();
        let libglib = open_first(&glib_cands).ok_or(Error::DlOpenFailed { lib: "libglib-2.0".into(), err: "not found".into() })?;
        libs.insert("libglib".into(), Arc::new(libglib));

        // gobject
        let libgobject = open_first(&gobject_cands).ok_or(Error::DlOpenFailed { lib: "libgobject-2.0".into(), err: "not found".into() })?;
        libs.insert("libgobject".into(), Arc::new(libgobject));

        // gio (optional)
        let libgio = open_first(&gio_cands);
        if let Some(g) = libgio { libs.insert("libgio".into(), Arc::new(g)); }

        // pango (optional)
        let libpango = open_first(&pango_cands);
        if let Some(p) = libpango { libs.insert("libpango".into(), Arc::new(p)); }

        // cairo (optional)
        let cairo_cands = ["libcairo.so.2", "libcairo.so"];
        let libcairo = open_first(&cairo_cands);
        if let Some(c) = libcairo { libs.insert("libcairo".into(), Arc::new(c)); }

        // Force X11 backend to avoid Wayland's asynchronous window configure round-trip.
        // On Wayland (including WSLg), gtk_window_present() returns before the compositor
        // acknowledges the configure request, so gtk_widget_get_mapped and
        // gtk_widget_get_allocated_width return 0 even after event pumping.  The X11
        // backend maps windows synchronously, eliminating this race condition entirely.
        // Users who prefer Wayland can override this by setting GDK_BACKEND=wayland.
        if std::env::var_os("GDK_BACKEND").is_none() {
            std::env::set_var("GDK_BACKEND", "x11");
        }

        // Prefer software/cairo rendering to avoid GL/X11 SHM issues on headless or restricted hosts.
        // Set before loading libgtk so the renderer selection observes these env vars early.
        if std::env::var_os("GSK_RENDERER").is_none() {
            std::env::set_var("GSK_RENDERER", "cairo");
        }
        if std::env::var_os("LIBGL_ALWAYS_SOFTWARE").is_none() {
            std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
        }
        // Force software rasterizer in mesa if available
        if std::env::var_os("MESA_LOADER_DRIVER_OVERRIDE").is_none() {
            std::env::set_var("MESA_LOADER_DRIVER_OVERRIDE", "swrast");
        }
        // Disable X11 shared-memory pixbuf use which can fail in some sandboxed environments
        if std::env::var_os("GDK_PIXBUF_USE_SHM").is_none() {
            std::env::set_var("GDK_PIXBUF_USE_SHM", "0");
        }

        // Default to GTK3.  Set GTK_DLOPEN_PREFER_GTK3=0 to try GTK4 first.
        // corro's GTK4 code path has unresolved stability issues (use-after-free
        // in widget lifecycle, layout-recursion crashes).  GTK3 is fully stable.
        let prefer_gtk3 = match std::env::var_os("GTK_DLOPEN_PREFER_GTK3") {
            Some(v) => v != "0",
            None => true,  // default to GTK3
        };

        let (libgtk, version) = if prefer_gtk3 {
            if let Some(l) = open_first(&gtk3_cands) { (l, Version::Gtk3) } else if let Some(l) = open_first(&gtk4_cands) { (l, Version::Gtk4) } else { return Err(Error::NoGtkFound); }
        } else {
            if let Some(l) = open_first(&gtk4_cands) { (l, Version::Gtk4) } else if let Some(l) = open_first(&gtk3_cands) { (l, Version::Gtk3) } else { return Err(Error::NoGtkFound); }
        };

        // GTK4 stability warning
        if version == Version::Gtk4 {
            use std::io::Write;
            let msg = "\n\x1b[1;33mWARNING: corro is using GTK4 which has known stability issues\n\
                       (widget lifecycle crashes).  Set GTK_DLOPEN_PREFER_GTK3=1 or\n\
                       unset the variable to use the stable GTK3 backend.\x1b[0m\n";
            let _ = std::io::stderr().write_all(msg.as_bytes());
            let _ = std::io::stderr().flush();
        }
        libs.insert("libgtk".into(), Arc::new(libgtk));

        // Open libgdk (separate library in GTK3; GTK4 bundles GDK into libgtk-4)
        if version == Version::Gtk3 {
            let gdk_cands = ["libgdk-3.so.0", "libgdk-3.so"];
            if let Some(g) = open_first(&gdk_cands) { libs.insert("libgdk".into(), Arc::new(g)); }
        }

        // Resolve symbols
        let symbols = Symbols::load(&libs).map_err(|e| Error::Other(format!("symbol error: {:?}", e)))?;

        // For GTK4, prefer the cairo renderer to avoid GL/X11 SHM rendering issues on some setups
        if version == Version::Gtk4 {
            if std::env::var_os("GSK_RENDERER").is_none() {
                std::env::set_var("GSK_RENDERER", "cairo");
            }
        }

        // Call gtk_init if available so type system and runtime are prepared
        if let Some(gtk_init) = symbols.gtk_init {
            unsafe { gtk_init(std::ptr::null_mut(), std::ptr::null_mut()); }
        }

        // In GTK4, gtk_init is a no-op, so widget types may not be registered.
        // Call _get_type() for each widget type to ensure type registration.
        unsafe fn ensure_type(lib: &Library, name: &str) {
            if let Ok(sym) = lib.get::<unsafe extern "C" fn() -> usize>(name.as_bytes()) {
                let get_type = *sym;
                get_type();
            }
        }
        if let Some(gtk) = libs.get("libgtk") {
            unsafe {
                ensure_type(gtk, "gtk_window_get_type");
                ensure_type(gtk, "gtk_button_get_type");
                ensure_type(gtk, "gtk_label_get_type");
                ensure_type(gtk, "gtk_box_get_type");
                ensure_type(gtk, "gtk_grid_get_type");
                ensure_type(gtk, "gtk_entry_get_type");
                ensure_type(gtk, "gtk_dialog_get_type");
                ensure_type(gtk, "gtk_combo_box_get_type");
                ensure_type(gtk, "gtk_drop_down_get_type");
                ensure_type(gtk, "gtk_check_button_get_type");
                ensure_type(gtk, "gtk_radio_button_get_type");
                ensure_type(gtk, "gtk_text_view_get_type");
                ensure_type(gtk, "gtk_text_buffer_get_type");
                ensure_type(gtk, "gtk_string_list_get_type");
                ensure_type(gtk, "gtk_toggle_button_get_type");
            }
        }

        Ok(Arc::new(Loader { libs, symbols: Arc::new(symbols), version, main_loop: std::sync::Mutex::new(0) }))
    }

    #[cfg(feature = "gtk4rs")]
    pub fn new_gtk4rs() -> Result<Arc<Self>, Error> {
        let symbols = backend_gtk4rs::try_build().map_err(|e| Error::Other(e))?;
        Ok(Arc::new(Loader {
            libs: HashMap::new(),
            symbols: Arc::new(symbols),
            version: Version::Gtk4,
            main_loop: std::sync::Mutex::new(0),
        }))
    }

    pub fn version(&self) -> Version { self.version }
}

fn open_first(cands: &[&str]) -> Option<RawLib> {
    for &name in cands {
        // try to open with RTLD_NOW | RTLD_GLOBAL
        unsafe {
            #[cfg(unix)]
            let flags = libc::RTLD_NOW | libc::RTLD_GLOBAL
                | { #[allow(non_upper_case_globals)] { libc::RTLD_NODELETE } };
            #[cfg(windows)]
            let flags = libloading::os::windows::DEFAULT;

            match Library::open(Some(name), flags) {
                Ok(lib) => return Some(lib),
                Err(_) => continue,
            }
        }
    }
    None
}
