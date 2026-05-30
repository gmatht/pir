use crate::loader::Loader;
use crate::error::Error;
use std::ffi::CString;
use std::marker::PhantomData;
use std::os::raw::c_void;
use std::rc::Rc;
use std::sync::Arc;

pub struct BoxWidget {
    inner: *mut c_void,
    loader: Arc<Loader>,
    _not_send: PhantomData<Rc<()>>,
}

pub enum Orientation { Horizontal = 0, Vertical = 1 }

impl BoxWidget {
    pub fn new(loader: Arc<Loader>, orientation: Orientation, spacing: i32) -> Result<Self, Error> {
        let symbols = &loader.symbols;
        let gtk_box_new = symbols.gtk_box_new.ok_or(Error::MissingSymbol("gtk_box_new".into()))?;
        let inner = unsafe { gtk_box_new(orientation as i32, spacing) };
        if let Some(ref_sink) = symbols.g_object_ref_sink { unsafe { ref_sink(inner); } }
        else if let Some(gref) = symbols.g_object_ref { unsafe { gref(inner); } }
        Ok(BoxWidget { inner, loader, _not_send: PhantomData })
    }

    pub fn append(&self, child: &impl AsRef<*mut c_void>) {
        let symbols = &self.loader.symbols;
        let child_ptr = *child.as_ref();
        if let Some(box_append) = symbols.gtk_box_append {
            unsafe { box_append(self.inner, child_ptr); }
        } else if let Some(pack) = symbols.gtk_box_pack_start {
            unsafe { pack(self.inner, child_ptr, 1, 1, 0); }
        }
    }
}

impl AsRef<*mut c_void> for BoxWidget { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for BoxWidget {
    fn drop(&mut self) {
        if let Some(unref) = self.loader.symbols.g_object_unref { unsafe { unref(self.inner); } }
    }
}

pub struct Window {
    inner: *mut c_void,
    loader: Arc<Loader>,
    _not_send: PhantomData<Rc<()>>,
}

impl Window {
    pub fn new(loader: Arc<Loader>) -> Result<Self, Error> {
        let symbols = &loader.symbols;
        let gtk_window_new = symbols.gtk_window_new.ok_or(Error::MissingSymbol("gtk_window_new".into()))?;
        let inner = unsafe { gtk_window_new(0) };
        if let Some(ref_sink) = symbols.g_object_ref_sink { unsafe { ref_sink(inner); } }
        else if let Some(gref) = symbols.g_object_ref { unsafe { gref(inner); } }
        Ok(Window { inner, loader, _not_send: PhantomData })
    }

    pub fn set_title(&self, title: &str) {
        if let Some(set_title) = self.loader.symbols.gtk_window_set_title {
            let c = CString::new(title).unwrap();
            unsafe { set_title(self.inner, c.as_ptr()); }
        }
    }

    pub fn set_child(&self, child: &impl AsRef<*mut c_void>) {
        let symbols = &self.loader.symbols;
        let child_ptr = *child.as_ref();
        if let Some(set_child) = symbols.gtk_window_set_child {
            unsafe { set_child(self.inner, child_ptr); }
        } else if let Some(container_add) = symbols.gtk_container_add {
            unsafe { container_add(self.inner, child_ptr); }
        }
    }

    pub fn present(&self) {
        // Show children first (GTK3) then present
        if let Some(show_all) = self.loader.symbols.gtk_widget_show_all { unsafe { show_all(self.inner); } }
        if let Some(present) = self.loader.symbols.gtk_window_present { unsafe { present(self.inner); return; } }
    }

    pub fn set_application(&self, app_ptr: *mut c_void) {
        if let Some(set_app) = self.loader.symbols.gtk_window_set_application { unsafe { set_app(self.inner, app_ptr); } }
    }
}

impl AsRef<*mut c_void> for Window { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for Window {
    fn drop(&mut self) {
        if let Some(unref) = self.loader.symbols.g_object_unref { unsafe { unref(self.inner); } }
    }
}

pub struct Button {
    inner: *mut c_void,
    loader: Arc<Loader>,
    _not_send: PhantomData<Rc<()>>,
}

impl Button {
    pub fn with_label(loader: Arc<Loader>, label: &str) -> Result<Self, Error> {
        let symbols = &loader.symbols;
        let ctor = symbols.gtk_button_new_with_label.ok_or(Error::MissingSymbol("gtk_button_new_with_label".into()))?;
        let c = CString::new(label).unwrap();
        let inner = unsafe { ctor(c.as_ptr()) };
        if let Some(ref_sink) = symbols.g_object_ref_sink { unsafe { ref_sink(inner); } }
        else if let Some(gref) = symbols.g_object_ref { unsafe { gref(inner); } }
        Ok(Button { inner, loader, _not_send: PhantomData })
    }

    pub fn new_from_ptr(loader: Arc<Loader>, ptr: *mut c_void) -> Self {
        if let Some(ref_sink) = loader.symbols.g_object_ref_sink { unsafe { ref_sink(ptr); } }
        else if let Some(gref) = loader.symbols.g_object_ref { unsafe { gref(ptr); } }
        Button { inner: ptr, loader, _not_send: PhantomData }
    }

    pub fn connect_clicked<F: FnMut() + 'static>(&self, f: F) -> Result<u64, Error> {
        let symbols = &self.loader.symbols;
        let boxed: Box<dyn FnMut()> = Box::new(f);
        // crate::signals::connect_clicked returns Result<u64, String>
        let res = unsafe { crate::signals::connect_signal(symbols, self.inner, "clicked", boxed, 2) };
        match res {
            Ok(id) => Ok(id),
            Err(e) => Err(Error::Other(e)),
        }
    }

    pub fn emit_clicked(&self) -> Result<u64, Error> {
        if let Some(emit) = self.loader.symbols.g_signal_emit_by_name {
            let name = CString::new("clicked").unwrap();
            let id = unsafe { emit(self.inner, name.as_ptr()) };
            Ok(id)
        } else { Err(Error::MissingSymbol("g_signal_emit_by_name".into())) }
    }
}

impl AsRef<*mut c_void> for Button { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for Button {
    fn drop(&mut self) {
        if let Some(unref) = self.loader.symbols.g_object_unref { unsafe { unref(self.inner); } }
    }
}

impl Clone for Button {
    fn clone(&self) -> Self {
        if let Some(gref) = self.loader.symbols.g_object_ref {
            unsafe { gref(self.inner); }
        }
        Button { inner: self.inner, loader: self.loader.clone(), _not_send: PhantomData }
    }
}

pub struct Label {
    inner: *mut c_void,
    loader: Arc<Loader>,
    _not_send: PhantomData<Rc<()>>,
}

impl Label {
    pub fn new(loader: Arc<Loader>, text: &str) -> Result<Self, Error> {
        let symbols = &loader.symbols;
        let ctor = symbols.gtk_label_new.ok_or(Error::MissingSymbol("gtk_label_new".into()))?;
        let c = CString::new(text).unwrap();
        let inner = unsafe { ctor(c.as_ptr()) };
        if let Some(ref_sink) = symbols.g_object_ref_sink { unsafe { ref_sink(inner); } }
        else if let Some(gref) = symbols.g_object_ref { unsafe { gref(inner); } }
        Ok(Label { inner, loader, _not_send: PhantomData })
    }

    pub fn set_text(&self, text: &str) {
        if let Some(set_text) = self.loader.symbols.gtk_label_set_text {
            let c = CString::new(text).unwrap();
            unsafe { set_text(self.inner, c.as_ptr()); }
        }
    }

    pub fn get_text(&self) -> Option<String> {
        if let Some(get_text) = self.loader.symbols.gtk_label_get_text {
            unsafe {
                let s = get_text(self.inner);
                if s.is_null() { return None; }
                let c = std::ffi::CStr::from_ptr(s);
                return Some(c.to_string_lossy().into_owned());
            }
        }
        None
    }

    pub fn set_markup(&self, markup: &str) {
        if let Some(set_markup) = self.loader.symbols.gtk_label_set_markup {
            let c = CString::new(markup).unwrap();
            unsafe { set_markup(self.inner, c.as_ptr()); }
        } else {
            self.set_text(markup);
        }
    }

    pub fn add_class(&self, class_name: &str) {
        if let Some(get_ctx) = self.loader.symbols.gtk_widget_get_style_context {
            if let Some(add_class) = self.loader.symbols.gtk_style_context_add_class {
                let c = CString::new(class_name).unwrap();
                unsafe {
                    let ctx = get_ctx(self.inner);
                    if !ctx.is_null() { add_class(ctx, c.as_ptr()); }
                }
            }
        }
    }

    pub fn remove_class(&self, class_name: &str) {
        if let Some(get_ctx) = self.loader.symbols.gtk_widget_get_style_context {
            if let Some(remove_class) = self.loader.symbols.gtk_style_context_remove_class {
                let c = CString::new(class_name).unwrap();
                unsafe {
                    let ctx = get_ctx(self.inner);
                    if !ctx.is_null() { remove_class(ctx, c.as_ptr()); }
                }
            }
        }
    }

    pub fn set_visible(&self, visible: bool) {
        if let Some(vfn) = self.loader.symbols.gtk_widget_set_visible {
            unsafe { vfn(self.inner, if visible { 1 } else { 0 }); }
        }
    }

    pub fn set_xalign(&self, x: f32) {
        if let Some(xalign_fn) = self.loader.symbols.gtk_label_set_xalign {
            unsafe { xalign_fn(self.inner, x); }
        }
    }
}

impl AsRef<*mut c_void> for Label { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for Label { fn drop(&mut self) { if let Some(unref) = self.loader.symbols.g_object_unref { unsafe { unref(self.inner); } } } }

impl Clone for Label {
    fn clone(&self) -> Self {
        if let Some(gref) = self.loader.symbols.g_object_ref {
            unsafe { gref(self.inner); }
        }
        Label { inner: self.inner, loader: self.loader.clone(), _not_send: PhantomData }
    }
}

impl Clone for Entry {
    fn clone(&self) -> Self {
        if let Some(gref) = self.loader.symbols.g_object_ref {
            unsafe { gref(self.inner); }
        }
        Entry { inner: self.inner, loader: self.loader.clone(), _not_send: PhantomData }
    }
}

// Application wrapper (simplified)
pub struct Application {
    inner: *mut c_void,
    loader: Arc<Loader>,
}

impl Application {
    pub fn new(loader: Arc<Loader>, _id: Option<&str>) -> Result<Self, Error> {
        // If possible try to create a GApplication via symbol (glib/gio)
        if let Some(app_new) = loader.symbols.gtk_application_new {
            let id_c = CString::new("org.example.GtkCompatApp").unwrap();
            let app = unsafe { app_new(id_c.as_ptr(), 0) };
            if !app.is_null() {
                return Ok(Application { inner: app, loader });
            }
        }
        Ok(Application { inner: std::ptr::null_mut(), loader })
    }

    pub fn with_app(loader: Arc<Loader>, app_ptr: *mut c_void) -> Result<Self, Error> {
        Ok(Application { inner: app_ptr, loader })
    }

    // Expose the raw GApplication pointer when available
    pub fn as_ptr(&self) -> *mut c_void { self.inner }

    pub fn run(self) -> Result<(), Error> {
        let symbols = &self.loader.symbols;
        let loop_new = symbols.g_main_loop_new.ok_or(Error::MissingSymbol("g_main_loop_new".into()))?;
        let loop_run = symbols.g_main_loop_run.ok_or(Error::MissingSymbol("g_main_loop_run".into()))?;
        let loop_ptr = unsafe { loop_new(std::ptr::null_mut(), 0) };
        unsafe { loop_run(loop_ptr); }
        Ok(())
    }

    pub fn set_app_menu(&self, menu_ptr: *mut c_void) -> Result<(), Error> {
        if let Some(set_app_menu) = self.loader.symbols.g_application_set_app_menu {
            if self.inner.is_null() { return Err(Error::Other("Application has no GApplication pointer".into())); }
            unsafe { set_app_menu(self.inner, menu_ptr); }
            Ok(())
        } else { Err(Error::MissingSymbol("g_application_set_app_menu".into())) }
    }
}

// Grid wrapper
pub struct Grid {
    inner: *mut c_void,
    loader: Arc<Loader>,
}

impl Grid {
    pub fn new(loader: Arc<Loader>) -> Result<Self, Error> {
        let symbols = &loader.symbols;
        let inner = if let Some(grid_new) = symbols.gtk_grid_new {
            unsafe { grid_new() }
        } else {
            let b = symbols.gtk_box_new.ok_or(Error::MissingSymbol("gtk_box_new".into()))?;
            unsafe { b(1, 0) }
        };
        if let Some(ref_sink) = symbols.g_object_ref_sink { unsafe { ref_sink(inner); } }
        else if let Some(gref) = symbols.g_object_ref { unsafe { gref(inner); } }
        Ok(Grid { inner, loader })
    }

    pub fn attach(&self, child: &impl AsRef<*mut c_void>, left: i32, top: i32, width: i32, height: i32) {
        let symbols = &self.loader.symbols;
        let child_ptr = *child.as_ref();
        if let Some(grid_attach) = symbols.gtk_grid_attach {
            unsafe { grid_attach(self.inner, child_ptr, left, top, width, height); }
        } else if let Some(box_append) = symbols.gtk_box_append {
            unsafe { box_append(self.inner, child_ptr); }
        }
    }
}

impl AsRef<*mut c_void> for Grid { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for Grid { fn drop(&mut self) { if let Some(unref) = self.loader.symbols.g_object_unref { unsafe { unref(self.inner); } } }
}

// Overlay wrapper
pub struct Overlay {
    inner: *mut c_void,
    loader: Arc<Loader>,
    _not_send: PhantomData<Rc<()>>,
}

impl Overlay {
    pub fn new(loader: Arc<Loader>) -> Result<Self, Error> {
        if let Some(ctor) = loader.symbols.gtk_overlay_new {
            let inner = unsafe { ctor() };
            if inner.is_null() { return Err(Error::MissingSymbol("gtk_overlay_new".into())); }
            if let Some(ref_sink) = loader.symbols.g_object_ref_sink { unsafe { ref_sink(inner); } }
            else if let Some(gref) = loader.symbols.g_object_ref { unsafe { gref(inner); } }
            Ok(Overlay { inner, loader, _not_send: PhantomData })
        } else {
            // Fallback to a box if overlay not available
            let b = loader.symbols.gtk_box_new.ok_or(Error::MissingSymbol("gtk_box_new".into()))?;
            let inner = unsafe { b(1, 0) };
            if inner.is_null() { return Err(Error::MissingSymbol("gtk_box_new".into())); }
            if let Some(ref_sink) = loader.symbols.g_object_ref_sink { unsafe { ref_sink(inner); } }
            else if let Some(gref) = loader.symbols.g_object_ref { unsafe { gref(inner); } }
            Ok(Overlay { inner, loader, _not_send: PhantomData })
        }
    }

    /// Add the main child (the underlying grid) to the overlay.
    pub fn add_main_child(&self, child: &impl AsRef<*mut c_void>) {
        if let Some(container_add) = self.loader.symbols.gtk_container_add {
            let child_ptr = *child.as_ref();
            unsafe { container_add(self.inner, child_ptr); }
        }
    }

    /// Add an overlay child (drawn above the main child)
    pub fn add_overlay(&self, child: &impl AsRef<*mut c_void>) {
        if let Some(add_overlay) = self.loader.symbols.gtk_overlay_add_overlay {
            let child_ptr = *child.as_ref();
            unsafe { add_overlay(self.inner, child_ptr); }
        } else if let Some(container_add) = self.loader.symbols.gtk_container_add {
            // fallback: just add as container child
            let child_ptr = *child.as_ref();
            unsafe { container_add(self.inner, child_ptr); }
        }
    }

    /// If supported, set overlay pass-through so events reach underlying widgets.
    pub fn set_overlay_pass_through(&self, overlay_child: &impl AsRef<*mut c_void>, pass: bool) {
        if let Some(set_pass) = self.loader.symbols.gtk_overlay_set_overlay_pass_through {
            let child_ptr = *overlay_child.as_ref();
            unsafe { set_pass(self.inner, child_ptr, if pass { 1 } else { 0 }); }
        }
    }
}

impl AsRef<*mut c_void> for Overlay { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for Overlay {
    fn drop(&mut self) {
        if let Some(unref) = self.loader.symbols.g_object_unref { unsafe { unref(self.inner); } }
    }
}

impl Clone for Overlay {
    fn clone(&self) -> Self {
        if let Some(gref) = self.loader.symbols.g_object_ref {
            unsafe { gref(self.inner); }
        }
        Overlay { inner: self.inner, loader: self.loader.clone(), _not_send: PhantomData }
    }
}


// DrawingArea wrapper
pub struct DrawingArea {
    inner: *mut c_void,
    loader: Arc<Loader>,
}

impl DrawingArea {
    pub fn new(loader: Arc<Loader>) -> Result<Self, Error> {
        if let Some(ctor) = loader.symbols.gtk_drawing_area_new {
            let inner = unsafe { ctor() };
            if let Some(ref_sink) = loader.symbols.g_object_ref_sink { unsafe { ref_sink(inner); } }
            else if let Some(gref) = loader.symbols.g_object_ref { unsafe { gref(inner); } }
            return Ok(DrawingArea { inner, loader });
        }
        Err(Error::MissingSymbol("gtk_drawing_area_new".into()))
    }

    pub fn queue_draw(&self) {
        if let Some(q) = self.loader.symbols.gtk_widget_queue_draw { unsafe { q(self.inner); } }
    }
    pub fn set_size_request(&self, w: i32, h: i32) {
        if let Some(sr) = self.loader.symbols.gtk_widget_set_size_request { unsafe { sr(self.inner, w, h); } }
    }
}

impl AsRef<*mut c_void> for DrawingArea { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for DrawingArea { fn drop(&mut self) { if let Some(unref) = self.loader.symbols.g_object_unref { unsafe { unref(self.inner); } } }
}

// Entry wrapper
pub struct Entry {
    inner: *mut c_void,
    loader: Arc<Loader>,
    _not_send: PhantomData<Rc<()>>,
}

impl Entry {
    pub fn new(loader: Arc<Loader>) -> Result<Self, Error> {
        let symbols = &loader.symbols;
        let ctor = symbols.gtk_entry_new.ok_or(Error::MissingSymbol("gtk_entry_new".into()))?;
        let inner = unsafe { ctor() };
        if let Some(ref_sink) = symbols.g_object_ref_sink { unsafe { ref_sink(inner); } }
        else if let Some(gref) = symbols.g_object_ref { unsafe { gref(inner); } }
        if let Some(set_has_frame) = symbols.gtk_entry_set_has_frame { unsafe { set_has_frame(inner, 0); } }
        Ok(Entry { inner, loader, _not_send: PhantomData })
    }

    pub fn set_text(&self, text: &str) {
        if let Some(set_text) = self.loader.symbols.gtk_entry_set_text {
            let c = CString::new(text).unwrap();
            unsafe { set_text(self.inner, c.as_ptr()); }
        }
    }

    pub fn get_text(&self) -> Option<String> {
        if let Some(get_text) = self.loader.symbols.gtk_entry_get_text {
            unsafe {
                let s = get_text(self.inner);
                if s.is_null() { return None; }
                let c = std::ffi::CStr::from_ptr(s);
                return Some(c.to_string_lossy().into_owned());
            }
        }
        None
    }

    pub fn set_width_chars(&self, n: i32) {
        if let Some(w) = self.loader.symbols.gtk_entry_set_width_chars { unsafe { w(self.inner, n); } }
    }

    pub fn set_size_request(&self, w: i32, h: i32) {
        if let Some(sr) = self.loader.symbols.gtk_widget_set_size_request { unsafe { sr(self.inner, w, h); } }
    }

    pub fn connect_changed<F: FnMut() + 'static>(&self, f: F) -> Result<u64, Error> {
        let boxed: Box<dyn FnMut()> = Box::new(f);
        let res = unsafe { crate::signals::connect_signal(&self.loader.symbols, self.inner, "changed", boxed, 2) };
        match res {
            Ok(id) => Ok(id),
            Err(e) => Err(Error::Other(e)),
        }
    }

    pub fn connect_button_press<F: FnMut() + 'static>(&self, f: F) -> Result<u64, Error> {
        let boxed: Box<dyn FnMut()> = Box::new(f);
        let res = unsafe { crate::signals::connect_signal(&self.loader.symbols, self.inner, "button-press-event", boxed, 3) };
        match res {
            Ok(id) => Ok(id),
            Err(e) => Err(Error::Other(e)),
        }
    }

    pub fn add_class(&self, class_name: &str) {
        if let Some(get_ctx) = self.loader.symbols.gtk_widget_get_style_context {
            if let Some(add_class) = self.loader.symbols.gtk_style_context_add_class {
                let c = CString::new(class_name).unwrap();
                unsafe {
                    let ctx = get_ctx(self.inner);
                    if !ctx.is_null() { add_class(ctx, c.as_ptr()); }
                }
            }
        }
    }

    pub fn remove_class(&self, class_name: &str) {
        if let Some(get_ctx) = self.loader.symbols.gtk_widget_get_style_context {
            if let Some(remove_class) = self.loader.symbols.gtk_style_context_remove_class {
                let c = CString::new(class_name).unwrap();
                unsafe {
                    let ctx = get_ctx(self.inner);
                    if !ctx.is_null() { remove_class(ctx, c.as_ptr()); }
                }
            }
        }
    }

    pub fn grab_focus(&self) {
        if let Some(grab) = self.loader.symbols.gtk_widget_grab_focus { unsafe { grab(self.inner); } }
    }
}

impl AsRef<*mut c_void> for Entry { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for Entry { fn drop(&mut self) { if let Some(unref) = self.loader.symbols.g_object_unref { unsafe { unref(self.inner); } } }
}

// Measure text width in pixels using gtk_widget_create_pango_layout + pango_layout_get_pixel_size when possible.
pub fn measure_text_px(loader: &Arc<Loader>, widget: Option<*mut c_void>, text: &str) -> i32 {
    // If no display, avoid Pango.
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return (text.chars().count() as i32) * 8;
    }

    let widget_ptr = match widget { Some(p) => p, None => return (text.chars().count() as i32) * 8 };
    let pango_lib = match loader.libs.get("libpango") { Some(l) => l, None => return (text.chars().count() as i32) * 8 };
    let gtk_lib = match loader.libs.get("libgtk") { Some(l) => l, None => return (text.chars().count() as i32) * 8 };

    unsafe {
        // Prefer helper that takes a widget and text
        if let Ok(create_layout) = gtk_lib.get::<unsafe extern "C" fn(*mut c_void, *const i8) -> *mut c_void>(b"gtk_widget_create_pango_layout") {
            let c = CString::new(text).unwrap();
            let layout = create_layout(widget_ptr, c.as_ptr());
            if !layout.is_null() {
                if let Ok(get_pixel) = pango_lib.get::<unsafe extern "C" fn(*mut c_void, *mut i32, *mut i32)>(b"pango_layout_get_pixel_size") {
                    let mut w: i32 = 0; let mut h: i32 = 0;
                    get_pixel(layout, &mut w as *mut i32, &mut h as *mut i32);
                    return w;
                }
            }
        }
    }

    (text.chars().count() as i32) * 8
}

/// Create a CSS provider from the given CSS string and return the raw provider pointer if successful.
/// The caller may add the provider to widget style contexts via `add_provider_to_widget`.
pub fn create_css_provider(loader: &Arc<Loader>, css: &str) -> Option<*mut c_void> {
    if let Some(ctor) = loader.symbols.gtk_css_provider_new {
        let provider = unsafe { ctor() };
        if provider.is_null() { return None; }
        if let Some(load_fn) = loader.symbols.gtk_css_provider_load_from_data {
            let c = CString::new(css).unwrap_or_default();
            // load_from_data(provider, data, length, error)
            let mut err: *mut c_void = std::ptr::null_mut();
            unsafe { let _ = load_fn(provider, c.as_ptr(), c.as_bytes().len() as isize, &mut err as *mut *mut c_void); }
        }
        Some(provider)
    } else { None }
}

/// Add an existing CSS provider to the widget's style context at the given priority.
pub fn add_provider_to_widget(loader: &Arc<Loader>, widget: *mut c_void, provider: *mut c_void, priority: u32) {
    if let Some(get_ctx) = loader.symbols.gtk_widget_get_style_context {
        if let Some(add_provider) = loader.symbols.gtk_style_context_add_provider {
            unsafe {
                let ctx = get_ctx(widget);
                if !ctx.is_null() { add_provider(ctx, provider, priority); }
            }
        }
    }
}

/// Set widget size request (width, height)
pub fn widget_set_size_request(loader: &Arc<Loader>, widget: *mut c_void, w: i32, h: i32) {
    if let Some(sr) = loader.symbols.gtk_widget_set_size_request {
        unsafe { sr(widget, w, h); }
    }
}

/// Set widget margin start (left)
pub fn widget_set_margin_start(loader: &Arc<Loader>, widget: *mut c_void, margin: i32) {
    if let Some(set_margin) = loader.symbols.gtk_widget_set_margin_start {
        unsafe { set_margin(widget, margin); }
    }
}

/// Set widget margin top
pub fn widget_set_margin_top(loader: &Arc<Loader>, widget: *mut c_void, margin: i32) {
    if let Some(set_margin) = loader.symbols.gtk_widget_set_margin_top {
        unsafe { set_margin(widget, margin); }
    }
}

/// Destroy a widget (remove from its parent). Falls back to g_object_unref if destroy not available.
pub fn destroy_widget(loader: &Arc<Loader>, widget: *mut c_void) {
    if let Some(destroy) = loader.symbols.gtk_widget_destroy {
        unsafe { destroy(widget); }
    } else if let Some(unref) = loader.symbols.g_object_unref {
        unsafe { unref(widget); }
    }
}
