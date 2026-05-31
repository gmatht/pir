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
            unsafe { pack(self.inner, child_ptr, 0, 0, 0); }
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
        let loader = &self.loader;
        if let Some(show_all) = loader.symbols.gtk_widget_show_all {
            // GTK3: show_all forces synchronous layout, then present
            unsafe { show_all(self.inner); }
        }
        if let Some(present) = loader.symbols.gtk_window_present {
            unsafe { present(self.inner); }
            // GTK4: gtk_window_present defers layout to the next loop iteration.
            // Force one iteration so widgets appear immediately, not a frame later.
            if loader.symbols.gtk_widget_show_all.is_none() {
                if let Some(glib_lib) = loader.libs.get("libglib") {
                    type Iteration = unsafe extern "C" fn(*mut std::ffi::c_void, i32) -> i32;
                    if let Ok(iter_fn) = unsafe { glib_lib.get::<Iteration>(b"g_main_context_iteration") } {
                        let iter = *iter_fn;
                        unsafe { iter(std::ptr::null_mut(), 0); }
                    }
                }
            }
            return;
        }
    }

    pub fn set_application(&self, app_ptr: *mut c_void) {
        if let Some(set_app) = self.loader.symbols.gtk_window_set_application { unsafe { set_app(self.inner, app_ptr); } }
    }

    pub fn insert_action_group(&self, name: &str, group_ptr: *mut c_void) {
        if let Some(insert) = self.loader.symbols.gtk_widget_insert_action_group {
            let c = CString::new(name).unwrap();
            unsafe { insert(self.inner, c.as_ptr(), group_ptr); }
        }
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

    pub fn register(&self) -> Result<(), Error> {
        if let Some(reg) = self.loader.symbols.g_application_register {
            if self.inner.is_null() {
                return Err(Error::Other("Application has no GApplication pointer".into()));
            }
            let mut error: *mut c_void = std::ptr::null_mut();
            let ok = unsafe { reg(self.inner, std::ptr::null_mut(), &mut error as *mut *mut c_void) };
            if ok == 0 {
                return Err(Error::Other("g_application_register failed".into()));
            }
            Ok(())
        } else {
            // If g_application_register isn't available, just proceed
            Ok(())
        }
    }

    pub fn set_app_menu(&self, menu_ptr: *mut c_void) -> Result<(), Error> {
        if let Some(set_app_menu) = self.loader.symbols.g_application_set_app_menu {
            if self.inner.is_null() { return Err(Error::Other("Application has no GApplication pointer".into())); }
            unsafe { set_app_menu(self.inner, menu_ptr); }
            Ok(())
        } else { Err(Error::MissingSymbol("g_application_set_app_menu".into())) }
    }

    pub fn set_menubar(&self, menu: &Menu) -> Result<(), Error> {
        if let Some(set_menubar) = self.loader.symbols.g_application_set_menubar {
            if self.inner.is_null() { return Err(Error::Other("Application has no GApplication pointer".into())); }
            unsafe { set_menubar(self.inner, menu.ptr()); }
            Ok(())
        } else { Err(Error::MissingSymbol("g_application_set_menubar".into())) }
    }

    pub fn add_action(&self, action: &SimpleAction) -> Result<(), Error> {
        if let Some(add_act) = self.loader.symbols.g_action_map_add_action {
            if self.inner.is_null() { return Err(Error::Other("Application has no GActionMap pointer".into())); }
            unsafe { add_act(self.inner, action.ptr()); }
            Ok(())
        } else { Err(Error::MissingSymbol("g_action_map_add_action".into())) }
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

/// Add a CSS provider globally so it affects all widgets.
/// Uses `gtk_style_context_add_provider_for_display` (GTK4) or
/// `gtk_style_context_add_provider_for_screen` (GTK3) or falls back
/// to per-widget `add_provider_to_widget`.
pub fn add_css_provider_global(loader: &Arc<Loader>, widget: *mut c_void, provider: *mut c_void, priority: u32) {
    // GTK4: per-display provider applies to all widgets
    if let (Some(get_display), Some(add_for_display)) = (loader.symbols.gdk_display_get_default, loader.symbols.gtk_style_context_add_provider_for_display) {
        unsafe {
            let display = get_display();
            if !display.is_null() {
                add_for_display(display, provider, priority);
                return;
            }
        }
    }
    // GTK3: per-screen provider
    if let (Some(get_screen), Some(add_for_screen)) = (loader.symbols.gdk_screen_get_default, loader.symbols.gtk_style_context_add_provider_for_screen) {
        unsafe {
            let screen = get_screen();
            if !screen.is_null() {
                add_for_screen(screen, provider, priority);
                return;
            }
        }
    }
    // Fallback: per-widget provider
    add_provider_to_widget(loader, widget, provider, priority);
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

// ---- Menu wrappers ----

struct MenuItem {
    label: String,
    detailed_action: String,
    submenu: Option<Menu>,
}

/// A GMenu model for building menu structures.
/// Stores both a GMenu pointer (for GTK4/GMenuModel consumers) and Rust-side
/// item data (for building GTK3 GtkMenuBar without GMenuModel iteration).
pub struct Menu {
    inner: *mut c_void,
    loader: Arc<Loader>,
    items: Vec<MenuItem>,
}

impl Menu {
    pub fn new(loader: Arc<Loader>) -> Result<Self, Error> {
        let symbols = &loader.symbols;
        let ctor = symbols.g_menu_new.ok_or(Error::MissingSymbol("g_menu_new".into()))?;
        let inner = unsafe { ctor() };
        if inner.is_null() {
            return Err(Error::Other("g_menu_new returned null".into()));
        }
        if let Some(ref_sink) = symbols.g_object_ref_sink { unsafe { ref_sink(inner); } }
        else if let Some(gref) = symbols.g_object_ref { unsafe { gref(inner); } }
        Ok(Menu { inner, loader, items: Vec::new() })
    }

    pub fn append(&mut self, label: &str, detailed_action: &str) {
        if let Some(append) = self.loader.symbols.g_menu_append {
            let l = CString::new(label).unwrap();
            let a = CString::new(detailed_action).unwrap();
            unsafe { append(self.inner, l.as_ptr(), a.as_ptr()); }
        }
        self.items.push(MenuItem {
            label: label.to_string(),
            detailed_action: detailed_action.to_string(),
            submenu: None,
        });
    }

    pub fn append_submenu(&mut self, label: &str, submenu: &Menu) {
        if let Some(append_sub) = self.loader.symbols.g_menu_append_submenu {
            let l = CString::new(label).unwrap();
            unsafe { append_sub(self.inner, l.as_ptr(), submenu.inner); }
        } else {
            self.append(label, "");
        }
        self.items.push(MenuItem {
            label: label.to_string(),
            detailed_action: String::new(),
            submenu: Some(submenu.clone()),
        });
    }

    pub fn ptr(&self) -> *mut c_void { self.inner }
}

impl AsRef<*mut c_void> for Menu { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for Menu {
    fn drop(&mut self) {
        if let Some(unref) = self.loader.symbols.g_object_unref { unsafe { unref(self.inner); } }
    }
}

impl Clone for Menu {
    fn clone(&self) -> Self {
        if let Some(gref) = self.loader.symbols.g_object_ref {
            unsafe { gref(self.inner); }
        }
        Menu { inner: self.inner, loader: self.loader.clone(), items: self.items.clone() }
    }
}

// Manual clone for MenuItem since it contains Option<Menu>
impl Clone for MenuItem {
    fn clone(&self) -> Self {
        MenuItem {
            label: self.label.clone(),
            detailed_action: self.detailed_action.clone(),
            submenu: self.submenu.clone(),
        }
    }
}

/// A GSimpleAction for menu item callbacks
pub struct SimpleAction {
    inner: *mut c_void,
    loader: Arc<Loader>,
}

impl SimpleAction {
    pub fn new(loader: Arc<Loader>, name: &str) -> Result<Self, Error> {
        let symbols = &loader.symbols;
        let ctor = symbols.g_simple_action_new.ok_or(Error::MissingSymbol("g_simple_action_new".into()))?;
        let n = CString::new(name).unwrap();
        let inner = unsafe { ctor(n.as_ptr(), std::ptr::null_mut()) };
        if inner.is_null() {
            return Err(Error::Other("g_simple_action_new returned null".into()));
        }
        if let Some(ref_sink) = symbols.g_object_ref_sink { unsafe { ref_sink(inner); } }
        else if let Some(gref) = symbols.g_object_ref { unsafe { gref(inner); } }
        Ok(SimpleAction { inner, loader })
    }

    pub fn ptr(&self) -> *mut c_void { self.inner }

    pub fn connect_activate<F: FnMut(*mut c_void) + 'static>(&self, f: F) -> Result<u64, Error> {
        let boxed: Box<Box<dyn FnMut(*mut c_void)>> = Box::new(Box::new(f));
        let res = unsafe { crate::signals::connect_signal_param(&self.loader.symbols, self.inner, "activate", boxed) };
        match res {
            Ok(id) => Ok(id),
            Err(e) => Err(Error::Other(e)),
        }
    }
}

impl AsRef<*mut c_void> for SimpleAction { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for SimpleAction {
    fn drop(&mut self) {
        if let Some(unref) = self.loader.symbols.g_object_unref { unsafe { unref(self.inner); } }
    }
}

impl Clone for SimpleAction {
    fn clone(&self) -> Self {
        if let Some(gref) = self.loader.symbols.g_object_ref {
            unsafe { gref(self.inner); }
        }
        SimpleAction { inner: self.inner, loader: self.loader.clone() }
    }
}

/// A menubar widget.
/// GTK4: uses GtkPopoverMenuBar from GMenuModel.
/// GTK3: builds GtkMenuBar from the Rust-side Menu item list.
/// Implements AsRef<*mut c_void> so it can be packed into a Box.
pub struct MenuBar {
    inner: *mut c_void,
    loader: Arc<Loader>,
    _not_send: PhantomData<Rc<()>>,
}

impl MenuBar {
    pub fn new(loader: Arc<Loader>, model: &Menu, action_group: *mut c_void) -> Result<Self, Error> {
        let symbols = &loader.symbols;
        // GTK4: GtkPopoverMenuBar — uses the GMenuModel directly
        if let Some(ctor) = symbols.gtk_popover_menu_bar_new_from_model {
            let inner = unsafe { ctor(model.ptr()) };
            if inner.is_null() {
                return Err(Error::Other("gtk_popover_menu_bar_new_from_model returned null".into()));
            }
            if let Some(ref_sink) = symbols.g_object_ref_sink { unsafe { ref_sink(inner); } }
            else if let Some(gref) = symbols.g_object_ref { unsafe { gref(inner); } }
            return Ok(MenuBar { inner, loader, _not_send: PhantomData });
        }
        // GTK3: build GtkMenuBar from the Rust-side items
        if let (Some(menu_bar_new), Some(_), Some(_)) = (
            symbols.gtk_menu_bar_new,
            symbols.gtk_menu_item_new_with_label,
            symbols.gtk_menu_shell_append,
        ) {
            let inner = unsafe { menu_bar_new() };
            if inner.is_null() {
                return Err(Error::Other("gtk_menu_bar_new returned null".into()));
            }
            Self::build_gtk3(&loader, inner, &model.items, &symbols, action_group);
            if let Some(ref_sink) = symbols.g_object_ref_sink { unsafe { ref_sink(inner); } }
            else if let Some(gref) = symbols.g_object_ref { unsafe { gref(inner); } }
            return Ok(MenuBar { inner, loader, _not_send: PhantomData });
        }
        Err(Error::MissingSymbol("gtk_popover_menu_bar_new_from_model".into()))
    }

    fn build_gtk3(
        loader: &Arc<Loader>,
        shell: *mut c_void,
        items: &[MenuItem],
        symbols: &crate::symbols::Symbols,
        action_group: *mut c_void,
    ) {
        for item in items {
            let c_label = match CString::new(item.label.as_str()) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if let Some(new_item) = symbols.gtk_menu_item_new_with_label {
                let gtk_item = unsafe { new_item(c_label.as_ptr()) };
                if let Some(ref submenu) = item.submenu {
                    // Submenu item: create GtkMenu and recurse
                    if let Some(menu_new) = symbols.gtk_menu_new {
                        let submenu_widget = unsafe { menu_new() };
                        Self::build_gtk3(loader, submenu_widget, &submenu.items, symbols, action_group);
                        if let Some(set_sub) = symbols.gtk_menu_item_set_submenu {
                            unsafe { set_sub(gtk_item, submenu_widget); }
                        }
                    }
                } else if !item.detailed_action.is_empty() && !action_group.is_null() {
                    let _ = set_detailed_action_name(symbols, gtk_item, &item.detailed_action);
                    if let (Some(lookup), Some(activate_fn)) = (
                        symbols.g_action_map_lookup_action,
                        symbols.g_action_activate,
                    ) {
                        let action_name = item.detailed_action.rsplit('.').next()
                            .unwrap_or(&item.detailed_action).to_string();
                        // Connect to "button-release-event" (GtkWidget signal, always fires on click)
                        let cb_action = action_name.clone();
                        let cb_group = action_group;
                        let cb_lookup = lookup;
                        let cb_activate = activate_fn;
                        let cb = Box::new(move |_event: *mut c_void| -> i32 {
                            let c = CString::new(cb_action.as_str()).unwrap();
                            let gaction = unsafe { cb_lookup(cb_group, c.as_ptr()) };
                            if !gaction.is_null() {
                                unsafe { cb_activate(gaction, std::ptr::null_mut()); }
                            }
                            0 // FALSE = let event propagate to GtkMenuItem default handler
                        });
                        let _ = unsafe { crate::signals::connect_signal_bool(
                            symbols, gtk_item, "button-release-event", cb,
                        )};
                    }
                }
                if let Some(append) = symbols.gtk_menu_shell_append {
                    unsafe { append(shell, gtk_item); }
                }
            }
        }
    }
}

fn set_detailed_action_name(symbols: &crate::symbols::Symbols, item: *mut c_void, action_str: &str) -> Result<(), ()> {
    if let Some(set_action) = symbols.gtk_actionable_set_detailed_action_name {
        let c = CString::new(action_str).unwrap();
        unsafe { set_action(item, c.as_ptr()); }
        Ok(())
    } else { Err(()) }
}

impl AsRef<*mut c_void> for MenuBar { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for MenuBar {
    fn drop(&mut self) {
        if let Some(unref) = self.loader.symbols.g_object_unref { unsafe { unref(self.inner); } }
    }
}
