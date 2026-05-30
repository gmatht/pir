use crate::error::Error;
use crate::symbols::Symbols;
use libloading::os::unix::Library;
use std::collections::HashMap;
use std::sync::Arc;

pub type RawLib = Library;

/// Which GTK we loaded
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version { None, Gtk3, Gtk4 }

pub struct Loader {
    pub libs: HashMap<String, Arc<RawLib>>,
    pub symbols: Arc<Symbols>,
    pub version: Version,
}

impl Loader {
    pub fn new() -> Result<Arc<Self>, Error> {
        // Candidate lists (env overrides not implemented yet)
        let glib_cands = ["libglib-2.0.so.0", "libglib-2.0.so"];
        let gobject_cands = ["libgobject-2.0.so.0", "libgobject-2.0.so"];
        let gio_cands = ["libgio-2.0.so.0", "libgio-2.0.so"];
        let gdk_cands = ["libgdk-4.so.1", "libgdk-4.so", "libgdk-3.so.0", "libgdk-3.so"];
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

        // gdk (optional)
        let libgdk = open_first(&gdk_cands);
        if let Some(g) = libgdk { libs.insert("libgdk".into(), Arc::new(g)); }

        // pango (optional)
        let libpango = open_first(&pango_cands);
        if let Some(p) = libpango { libs.insert("libpango".into(), Arc::new(p)); }

        // cairo (optional)
        let cairo_cands = ["libcairo.so.2", "libcairo.so"];
        let libcairo = open_first(&cairo_cands);
        if let Some(c) = libcairo { libs.insert("libcairo".into(), Arc::new(c)); }

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

        // Try GTK4 then GTK3 by default. Honor GTK_DLOPEN_PREFER_GTK3=1 to reverse.
        let prefer_gtk3 = match std::env::var_os("GTK_DLOPEN_PREFER_GTK3") {
            Some(v) => v != "0",
            None => false,
        };

        let (libgtk, version) = if prefer_gtk3 {
            if let Some(l) = open_first(&gtk3_cands) { (l, Version::Gtk3) } else if let Some(l) = open_first(&gtk4_cands) { (l, Version::Gtk4) } else { return Err(Error::NoGtkFound); }
        } else {
            if let Some(l) = open_first(&gtk4_cands) { (l, Version::Gtk4) } else if let Some(l) = open_first(&gtk3_cands) { (l, Version::Gtk3) } else { return Err(Error::NoGtkFound); }
        };
        libs.insert("libgtk".into(), Arc::new(libgtk));

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

        Ok(Arc::new(Loader { libs, symbols: Arc::new(symbols), version }))
    }

    pub fn version(&self) -> Version { self.version }
}

fn open_first(cands: &[&str]) -> Option<RawLib> {
    for &name in cands {
        // try to open with RTLD_NOW | RTLD_GLOBAL
        unsafe {
            // If available, add RTLD_NODELETE to avoid unloading libraries while GTK may still use them
            let flags = libc::RTLD_NOW | libc::RTLD_GLOBAL
                | { #[allow(non_upper_case_globals)] { libc::RTLD_NODELETE } };
            match Library::open(Some(name), flags) {
                Ok(lib) => return Some(lib),
                Err(_) => continue,
            }
        }
    }
    None
}
