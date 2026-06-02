use crate::loader::{Loader, Version};
use crate::error::Error;
use std::ffi::CString;
use std::marker::PhantomData;
use std::os::raw::c_void;
use std::rc::Rc;
use std::sync::Arc;

pub trait GtkWidget {
    fn widget_ptr(&self) -> *mut c_void;
}

impl<T: AsRef<*mut c_void>> GtkWidget for T {
    fn widget_ptr(&self) -> *mut c_void {
        *self.as_ref()
    }
}

fn debug_zombie_warning(kind: &str, method: &str) {
    if cfg!(debug_assertions) {
        eprintln!("gtk_dynamic_loader warning: {kind}::{method} called on a null/dropped widget; no-op");
    }
}

fn guard_widget_ptr(ptr: *mut c_void, kind: &str, method: &str) -> bool {
    if ptr.is_null() {
        debug_zombie_warning(kind, method);
        false
    } else {
        true
    }
}

macro_rules! guard_widget {
    ($self:expr, $kind:literal, $method:literal) => {
        if !guard_widget_ptr($self.inner, $kind, $method) {
            return;
        }
    };
}

macro_rules! guard_widget_or {
    ($self:expr, $kind:literal, $method:literal, $ret:expr) => {
        if !guard_widget_ptr($self.inner, $kind, $method) {
            return $ret;
        }
    };
}

extern "C" fn idle_once_trampoline(data: *mut c_void) -> i32 {
    unsafe {
        if data.is_null() {
            return 0;
        }
        let boxed: Box<Box<dyn FnMut()>> = Box::from_raw(data as *mut Box<dyn FnMut()>);
        let mut cb = boxed;
        (*cb)();
    }
    0
}

pub struct BoxWidget {
    inner: *mut c_void,
    loader: Arc<Loader>,
    orientation: Orientation,
    _not_send: PhantomData<Rc<()>>,
}

#[derive(Clone, Copy)]
pub enum Orientation { Horizontal = 0, Vertical = 1 }

impl BoxWidget {
    pub fn new(loader: Arc<Loader>, orientation: Orientation, spacing: i32) -> Result<Self, Error> {
        let symbols = &loader.symbols;
        let gtk_box_new = symbols.gtk_box_new.ok_or(Error::MissingSymbol("gtk_box_new".into()))?;
        let inner = unsafe { gtk_box_new(orientation as i32, spacing) };
        unsafe { take_ownership(&symbols, &loader.version, inner); }
        Ok(BoxWidget { inner, loader, orientation, _not_send: PhantomData })
    }

    pub fn append(&self, child: &impl GtkWidget) {
        guard_widget!(self, "BoxWidget", "append");
        let symbols = &self.loader.symbols;
        let child_ptr = child.widget_ptr();
        if !guard_widget_ptr(child_ptr, "BoxWidget", "append.child") {
            return;
        }
        if let Some(box_append) = symbols.gtk_box_append {
            unsafe { box_append(self.inner, child_ptr); }
        } else if let Some(pack) = symbols.gtk_box_pack_start {
            let expand = match self.orientation {
                Orientation::Horizontal => symbols.gtk_widget_get_hexpand.map(|f| unsafe { f(child_ptr) }).unwrap_or(0),
                Orientation::Vertical => symbols.gtk_widget_get_vexpand.map(|f| unsafe { f(child_ptr) }).unwrap_or(0),
            };
            unsafe { pack(self.inner, child_ptr, expand, expand, 0); }
        }
    }
}

impl AsRef<*mut c_void> for BoxWidget { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for BoxWidget {
    fn drop(&mut self) {
        unsafe { crate::wrappers::unref_widget(&self.loader, self.inner); }
        self.inner = std::ptr::null_mut();
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
        unsafe { take_ownership(&symbols, &loader.version, inner); }
        Ok(Window { inner, loader, _not_send: PhantomData })
    }

    pub fn set_title(&self, title: &str) {
        guard_widget!(self, "Window", "set_title");
        if let Some(set_title) = self.loader.symbols.gtk_window_set_title {
            let c = CString::new(title).unwrap();
            unsafe { set_title(self.inner, c.as_ptr()); }
        }
    }

    pub fn set_child(&self, child: &impl GtkWidget) {
        guard_widget!(self, "Window", "set_child");
        let symbols = &self.loader.symbols;
        let child_ptr = child.widget_ptr();
        if !guard_widget_ptr(child_ptr, "Window", "set_child.child") {
            return;
        }
        if let Some(set_child) = symbols.gtk_window_set_child {
            unsafe { set_child(self.inner, child_ptr); }
        } else if let Some(container_add) = symbols.gtk_container_add {
            unsafe { container_add(self.inner, child_ptr); }
        }
    }

    pub fn present(&self) {
        guard_widget!(self, "Window", "present");
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

    /// # Safety
    /// `app_ptr` must be a valid GtkApplication pointer or null.
    pub unsafe fn set_application(&self, app_ptr: *mut c_void) {
        guard_widget!(self, "Window", "set_application");
        if let Some(set_app) = self.loader.symbols.gtk_window_set_application { unsafe { set_app(self.inner, app_ptr); } }
    }

    pub fn set_default_size(&self, width: i32, height: i32) {
        guard_widget!(self, "Window", "set_default_size");
        if let Some(set_size) = self.loader.symbols.gtk_window_set_default_size {
            unsafe { set_size(self.inner, width, height); }
        }
    }

    /// # Safety
    /// `group_ptr` must be a valid GActionGroup pointer or null.
    pub unsafe fn insert_action_group(&self, name: &str, group_ptr: *mut c_void) {
        guard_widget!(self, "Window", "insert_action_group");
        if let Some(insert) = self.loader.symbols.gtk_widget_insert_action_group {
            let c = CString::new(name).unwrap();
            unsafe { insert(self.inner, c.as_ptr(), group_ptr); }
        }
    }
}

impl AsRef<*mut c_void> for Window { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for Window {
    fn drop(&mut self) {
        unsafe { crate::wrappers::unref_widget(&self.loader, self.inner); }
        self.inner = std::ptr::null_mut();
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
        unsafe { take_ownership(&symbols, &loader.version, inner); }
        Ok(Button { inner, loader, _not_send: PhantomData })
    }

    /// # Safety
    /// `ptr` must be a valid, non-null GtkButton pointer.
    pub unsafe fn new_from_ptr(loader: Arc<Loader>, ptr: *mut c_void) -> Self {
        if ptr.is_null() { panic!("Button::new_from_ptr received null"); }
        take_ownership(&loader.symbols, &loader.version, ptr);
        Button { inner: ptr, loader, _not_send: PhantomData }
    }

    pub fn connect_clicked<F: FnMut() + 'static>(&self, f: F) -> Result<u64, Error> {
        guard_widget_or!(self, "Button", "connect_clicked", Err(Error::Other("button dropped".into())));
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
        guard_widget_or!(self, "Button", "emit_clicked", Err(Error::Other("button dropped".into())));
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
        unsafe { crate::wrappers::unref_widget(&self.loader, self.inner); }
        self.inner = std::ptr::null_mut();
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
        unsafe { take_ownership(&symbols, &loader.version, inner); }
        Ok(Label { inner, loader, _not_send: PhantomData })
    }

    pub fn set_text(&self, text: &str) {
        guard_widget!(self, "Label", "set_text");
        if let Some(set_text) = self.loader.symbols.gtk_label_set_text {
            let c = CString::new(text).unwrap();
            unsafe { set_text(self.inner, c.as_ptr()); }
        }
    }

    pub fn get_text(&self) -> Option<String> {
        guard_widget_or!(self, "Label", "get_text", None);
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
        guard_widget!(self, "Label", "set_markup");
        if let Some(set_markup) = self.loader.symbols.gtk_label_set_markup {
            let c = CString::new(markup).unwrap();
            unsafe { set_markup(self.inner, c.as_ptr()); }
        } else {
            self.set_text(markup);
        }
    }

    pub fn add_class(&self, class_name: &str) {
        guard_widget!(self, "Label", "add_class");
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
        guard_widget!(self, "Label", "remove_class");
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
        guard_widget!(self, "Label", "set_visible");
        if let Some(vfn) = self.loader.symbols.gtk_widget_set_visible {
            unsafe { vfn(self.inner, if visible { 1 } else { 0 }); }
        }
    }

    pub fn set_xalign(&self, x: f32) {
        guard_widget!(self, "Label", "set_xalign");
        if let Some(xalign_fn) = self.loader.symbols.gtk_label_set_xalign {
            unsafe { xalign_fn(self.inner, x); }
        }
    }
}

impl AsRef<*mut c_void> for Label { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for Label { fn drop(&mut self) { unsafe { crate::wrappers::unref_widget(&self.loader, self.inner); } self.inner = std::ptr::null_mut(); } }

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

    /// # Safety
    /// `app_ptr` must be a valid GApplication pointer or null.
    pub unsafe fn with_app(loader: Arc<Loader>, app_ptr: *mut c_void) -> Result<Self, Error> {
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

    /// # Safety
    /// `menu_ptr` must be a valid GMenuModel pointer or null.
    pub unsafe fn set_app_menu(&self, menu_ptr: *mut c_void) -> Result<(), Error> {
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
        unsafe { take_ownership(&symbols, &loader.version, inner); }
        Ok(Grid { inner, loader })
    }

    pub fn attach(&self, child: &impl AsRef<*mut c_void>, left: i32, top: i32, width: i32, height: i32) {
        guard_widget!(self, "Grid", "attach");
        let symbols = &self.loader.symbols;
        let child_ptr = child.widget_ptr();
        if !guard_widget_ptr(child_ptr, "Grid", "attach.child") {
            return;
        }
        if let Some(grid_attach) = symbols.gtk_grid_attach {
            unsafe { grid_attach(self.inner, child_ptr, left, top, width, height); }
        } else if let Some(box_append) = symbols.gtk_box_append {
            unsafe { box_append(self.inner, child_ptr); }
        }
    }
}

impl AsRef<*mut c_void> for Grid { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for Grid { fn drop(&mut self) { unsafe { crate::wrappers::unref_widget(&self.loader, self.inner); } self.inner = std::ptr::null_mut(); }
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
            unsafe { take_ownership(&loader.symbols, &loader.version, inner); }
            Ok(Overlay { inner, loader, _not_send: PhantomData })
        } else {
            // Fallback to a box if overlay not available
            let b = loader.symbols.gtk_box_new.ok_or(Error::MissingSymbol("gtk_box_new".into()))?;
            let inner = unsafe { b(1, 0) };
            if inner.is_null() { return Err(Error::MissingSymbol("gtk_box_new".into())); }
            unsafe { take_ownership(&loader.symbols, &loader.version, inner); }
            Ok(Overlay { inner, loader, _not_send: PhantomData })
        }
    }

    /// Add the main child (the underlying grid) to the overlay.
    pub fn add_main_child(&self, child: &impl GtkWidget) {
        guard_widget!(self, "Overlay", "add_main_child");
        if let Some(container_add) = self.loader.symbols.gtk_container_add {
            let child_ptr = child.widget_ptr();
            if !guard_widget_ptr(child_ptr, "Overlay", "add_main_child.child") {
                return;
            }
            unsafe { container_add(self.inner, child_ptr); }
        }
    }

    /// Add an overlay child (drawn above the main child)
    pub fn add_overlay(&self, child: &impl GtkWidget) {
        guard_widget!(self, "Overlay", "add_overlay");
        let child_ptr = child.widget_ptr();
        if !guard_widget_ptr(child_ptr, "Overlay", "add_overlay.child") {
            return;
        }
        if let Some(add_overlay) = self.loader.symbols.gtk_overlay_add_overlay {
            unsafe { add_overlay(self.inner, child_ptr); }
        } else if let Some(container_add) = self.loader.symbols.gtk_container_add {
            // fallback: just add as container child
            unsafe { container_add(self.inner, child_ptr); }
        }
    }

    /// Set the main child of the overlay (useful after construction).
    pub fn set_child(&self, child: &impl GtkWidget) {
        guard_widget!(self, "Overlay", "set_child");
        let child_ptr = child.widget_ptr();
        if !guard_widget_ptr(child_ptr, "Overlay", "set_child.child") {
            return;
        }
        if let Some(set_child) = self.loader.symbols.gtk_overlay_set_child {
            unsafe { set_child(self.inner, child_ptr); }
        } else if let Some(container_add) = self.loader.symbols.gtk_container_add {
            unsafe { container_add(self.inner, child_ptr); }
        }
    }

    /// If supported, set overlay pass-through so events reach underlying widgets.
    pub fn set_overlay_pass_through(&self, overlay_child: &impl GtkWidget, pass: bool) {
        guard_widget!(self, "Overlay", "set_overlay_pass_through");
        if let Some(set_pass) = self.loader.symbols.gtk_overlay_set_overlay_pass_through {
            let child_ptr = overlay_child.widget_ptr();
            if !guard_widget_ptr(child_ptr, "Overlay", "set_overlay_pass_through.child") {
                return;
            }
            unsafe { set_pass(self.inner, child_ptr, if pass { 1 } else { 0 }); }
        }
    }

    /// Remove an overlay child widget.
    pub fn remove(&self, child: &impl GtkWidget) {
        guard_widget!(self, "Overlay", "remove");
        let child_ptr = child.widget_ptr();
        if !guard_widget_ptr(child_ptr, "Overlay", "remove.child") {
            return;
        }
        if let Some(container_remove) = self.loader.symbols.gtk_container_remove {
            unsafe { container_remove(self.inner, child_ptr); }
        } else if let Some(unparent) = self.loader.symbols.gtk_widget_unparent {
            unsafe { unparent(child_ptr); }
        }
    }
}

impl AsRef<*mut c_void> for Overlay { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe { crate::wrappers::unref_widget(&self.loader, self.inner); }
        self.inner = std::ptr::null_mut();
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
            unsafe { take_ownership(&loader.symbols, &loader.version, inner); }
            return Ok(DrawingArea { inner, loader });
        }
        Err(Error::MissingSymbol("gtk_drawing_area_new".into()))
    }

    pub fn queue_draw(&self) {
        guard_widget!(self, "DrawingArea", "queue_draw");
        if let Some(q) = self.loader.symbols.gtk_widget_queue_draw { unsafe { q(self.inner); } }
    }
    pub fn set_size_request(&self, w: i32, h: i32) {
        guard_widget!(self, "DrawingArea", "set_size_request");
        if let Some(sr) = self.loader.symbols.gtk_widget_set_size_request { unsafe { sr(self.inner, w, h); } }
    }

    /// GTK4: set draw function callback (cr, width, height)
    pub fn set_draw_func(&self, cb: Box<dyn FnMut(*mut std::ffi::c_void, i32, i32)>) -> Result<(), Error> {
        guard_widget_or!(self, "DrawingArea", "set_draw_func", Err(Error::Other("drawing area dropped".into())));
        if let Some(f) = self.loader.symbols.gtk_drawing_area_set_draw_func {
            let boxed: Box<Box<dyn FnMut(*mut std::ffi::c_void, i32, i32)>> = Box::new(Box::new(cb));
            let raw = Box::into_raw(boxed) as *mut std::ffi::c_void;
            unsafe {
                f(self.inner, Some(crate::signals::gtk_compat_trampoline_draw_gtk4 as unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void, i32, i32, *mut std::ffi::c_void)), raw, Some(crate::signals::gtk_compat_destroy_notify_draw_gtk4 as unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void)));
            }
            Ok(())
        } else {
            Err(Error::MissingSymbol("gtk_drawing_area_set_draw_func".into()))
        }
    }

    /// GTK4: set content width for scrollable area
    pub fn set_content_width(&self, w: i32) {
        guard_widget!(self, "DrawingArea", "set_content_width");
        if let Some(f) = self.loader.symbols.gtk_drawing_area_set_content_width {
            unsafe { f(self.inner, w); }
        }
    }

    /// GTK4: set content height for scrollable area
    pub fn set_content_height(&self, h: i32) {
        guard_widget!(self, "DrawingArea", "set_content_height");
        if let Some(f) = self.loader.symbols.gtk_drawing_area_set_content_height {
            unsafe { f(self.inner, h); }
        }
    }

    /// GTK3: connect to the "draw" signal. The closure receives (widget_ptr, cairo_t*) and returns gboolean.
    pub fn connect_draw_gtk3(&self, cb: Box<dyn FnMut(*mut std::ffi::c_void, *mut std::ffi::c_void) -> i32>) -> Result<u64, String> {
        guard_widget_or!(self, "DrawingArea", "connect_draw_gtk3", Err("drawing area dropped".into()));
        let boxed: Box<Box<dyn FnMut(*mut std::ffi::c_void, *mut std::ffi::c_void) -> i32>> = Box::new(Box::new(cb));
        let raw = Box::into_raw(boxed) as *mut std::ffi::c_void;
        let sig_name = std::ffi::CString::new("draw").unwrap();
        if let Some(gscd) = self.loader.symbols.g_signal_connect_data {
            let handler_ptr = crate::signals::gtk_compat_trampoline_draw_gtk3 as *const () as *mut std::ffi::c_void;
            let destroy_ptr = Some(crate::signals::gtk_compat_destroy_notify_draw_gtk3 as unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void));
            let id = unsafe { gscd(self.inner, sig_name.as_ptr(), handler_ptr, raw, destroy_ptr, 0) };
            Ok(id)
        } else if let Some(gsc) = self.loader.symbols.g_signal_connect {
            let handler_ptr = crate::signals::gtk_compat_trampoline_draw_gtk3 as *const () as *mut std::ffi::c_void;
            let id = unsafe { gsc(self.inner, sig_name.as_ptr(), handler_ptr, raw) };
            Ok(id)
        } else {
            Err("no g_signal_connect available".into())
        }
    }
}

impl AsRef<*mut c_void> for DrawingArea { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for DrawingArea { fn drop(&mut self) { unsafe { crate::wrappers::unref_widget(&self.loader, self.inner); } self.inner = std::ptr::null_mut(); }
}

// ---- ScrolledWindow wrapper ----
pub struct ScrolledWindow {
    inner: *mut c_void,
    loader: Arc<Loader>,
    _not_send: PhantomData<Rc<()>>,
}

impl ScrolledWindow {
    pub fn new(loader: Arc<Loader>) -> Result<Self, Error> {
        let symbols = &loader.symbols;
        type ScrolledNew = unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void) -> *mut std::ffi::c_void;
        let ctor = symbols.gtk_scrolled_window_new.ok_or(Error::MissingSymbol("gtk_scrolled_window_new".into()))?;
        let inner = unsafe { ctor(std::ptr::null_mut(), std::ptr::null_mut()) };
        if inner.is_null() { return Err(Error::Other("gtk_scrolled_window_new returned null".into())); }
        unsafe { take_ownership(&symbols, &loader.version, inner); }
        Ok(ScrolledWindow { inner, loader, _not_send: PhantomData })
    }

    /// # Safety
    /// `ptr` must be a valid, non-null GtkButton pointer.
    pub unsafe fn new_from_ptr(loader: Arc<Loader>, ptr: *mut c_void) -> Self {
        if ptr.is_null() { panic!("Button::new_from_ptr received null"); }
        take_ownership(&loader.symbols, &loader.version, ptr);
        ScrolledWindow { inner: ptr, loader, _not_send: PhantomData }
    }

    pub fn set_policy(&self, h_policy: u32, v_policy: u32) {
        guard_widget!(self, "ScrolledWindow", "set_policy");
        type SetPolicy = unsafe extern "C" fn(*mut std::ffi::c_void, u32, u32);
        if let Some(set_policy) = self.loader.symbols.gtk_scrolled_window_set_policy {
            unsafe { set_policy(self.inner, h_policy, v_policy); }
        }
    }

    pub fn set_child(&self, child: &impl GtkWidget) {
        guard_widget!(self, "ScrolledWindow", "set_child");
        let child_ptr = child.widget_ptr();
        if !guard_widget_ptr(child_ptr, "ScrolledWindow", "set_child.child") {
            return;
        }
        type SetChild = unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void);
        if let Some(set_child) = self.loader.symbols.gtk_scrolled_window_set_child {
            unsafe { set_child(self.inner, child_ptr); }
        } else if let Some(container_add) = self.loader.symbols.gtk_container_add {
            unsafe { container_add(self.inner, child_ptr); }
        }
    }

    pub fn get_hadjustment_value(&self) -> f64 {
        guard_widget_or!(self, "ScrolledWindow", "get_hadjustment_value", 0.0);
        self.adj_value(
            self.loader.symbols.gtk_scrolled_window_get_hadjustment,
            self.loader.symbols.gtk_adjustment_get_value,
        )
    }

    pub fn get_vadjustment_value(&self) -> f64 {
        guard_widget_or!(self, "ScrolledWindow", "get_vadjustment_value", 0.0);
        self.adj_value(
            self.loader.symbols.gtk_scrolled_window_get_vadjustment,
            self.loader.symbols.gtk_adjustment_get_value,
        )
    }

    fn adj_value(&self, get_adj: Option<unsafe extern "C" fn(*mut std::ffi::c_void) -> *mut std::ffi::c_void>, get_val: Option<unsafe extern "C" fn(*mut std::ffi::c_void) -> f64>) -> f64 {
        get_adj.and_then(|f| {
            let adj = unsafe { f(self.inner) };
            if adj.is_null() { None } else { get_val.map(|gv| unsafe { gv(adj) }) }
        }).unwrap_or(0.0)
    }
}

impl AsRef<*mut c_void> for ScrolledWindow { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for ScrolledWindow {
    fn drop(&mut self) {
        unsafe { crate::wrappers::unref_widget(&self.loader, self.inner); }
        self.inner = std::ptr::null_mut();
    }
}

impl Clone for ScrolledWindow {
    fn clone(&self) -> Self {
        if let Some(gref) = self.loader.symbols.g_object_ref { unsafe { gref(self.inner); } }
        ScrolledWindow { inner: self.inner, loader: self.loader.clone(), _not_send: PhantomData }
    }
}

// ---- GestureClick wrapper ----
pub struct GestureClick {
    inner: *mut c_void,
    loader: Arc<Loader>,
    _not_send: PhantomData<Rc<()>>,
}

impl GestureClick {
    pub fn new(loader: Arc<Loader>) -> Result<Self, Error> {
        let symbols = &loader.symbols;
        let ctor = symbols.gtk_gesture_click_new.ok_or(Error::MissingSymbol("gtk_gesture_click_new".into()))?;
        let inner = unsafe { ctor() };
        if inner.is_null() { return Err(Error::Other("gtk_gesture_click_new returned null".into())); }
        unsafe { take_ownership(&symbols, &loader.version, inner); }
        Ok(GestureClick { inner, loader, _not_send: PhantomData })
    }

    pub fn connect_pressed<F: FnMut(i32, f64, f64) + 'static>(&self, f: F) -> Result<u64, Error> {
        let boxed: Box<dyn FnMut(i32, f64, f64)> = Box::new(f);
        let res = unsafe { crate::signals::connect_signal_gesture(&self.loader.symbols, self.inner, "pressed", boxed) };
        match res { Ok(id) => Ok(id), Err(e) => Err(Error::Other(e)) }
    }

    pub fn add_to_widget(&self, widget: &impl GtkWidget) {
        guard_widget!(self, "GestureClick", "add_to_widget");
        if let Some(add_ctrl) = self.loader.symbols.gtk_widget_add_controller {
            let widget_ptr = widget.widget_ptr();
            if !guard_widget_ptr(widget_ptr, "GestureClick", "add_to_widget.widget") {
                return;
            }
            unsafe { add_ctrl(widget_ptr, self.inner); }
        }
    }
}

impl Drop for GestureClick {
    fn drop(&mut self) {
        if let Some(unref) = self.loader.symbols.g_object_unref { unsafe { unref(self.inner); } }
        self.inner = std::ptr::null_mut();
    }
}

// ---- EventControllerKey wrapper ----
pub struct EventControllerKey {
    inner: *mut c_void,
    loader: Arc<Loader>,
    _not_send: PhantomData<Rc<()>>,
}

impl EventControllerKey {
    pub fn new(loader: Arc<Loader>) -> Result<Self, Error> {
        let symbols = &loader.symbols;
        let ctor = symbols.gtk_event_controller_key_new.ok_or(Error::MissingSymbol("gtk_event_controller_key_new".into()))?;
        let inner = unsafe { ctor() };
        if inner.is_null() { return Err(Error::Other("gtk_event_controller_key_new returned null".into())); }
        unsafe { take_ownership(&symbols, &loader.version, inner); }
        Ok(EventControllerKey { inner, loader, _not_send: PhantomData })
    }

    pub fn connect_key_pressed<F: FnMut(u32) -> i32 + 'static>(&self, f: F) -> Result<u64, Error> {
        guard_widget_or!(self, "EventControllerKey", "connect_key_pressed", Err(Error::Other("key controller dropped".into())));
        let boxed: Box<Box<dyn FnMut(u32) -> i32>> = Box::new(Box::new(f));
        let raw = Box::into_raw(boxed) as *mut c_void;
        unsafe { self.connect_key_pressed_raw(raw) }
    }

    unsafe fn connect_key_pressed_raw(&self, raw: *mut c_void) -> Result<u64, Error> {
        let sig_name = std::ffi::CString::new("key-pressed").unwrap();
        if let Some(gscd) = self.loader.symbols.g_signal_connect_data {
            let handler_ptr = crate::signals::gtk_compat_trampoline_key_pressed as *const () as *mut c_void;
            let destroy_ptr = Some(crate::signals::gtk_compat_destroy_notify_key_pressed as unsafe extern "C" fn(*mut c_void, *mut c_void));
            let id = gscd(self.inner, sig_name.as_ptr(), handler_ptr, raw, destroy_ptr, 0);
            Ok(id)
        } else if let Some(gsc) = self.loader.symbols.g_signal_connect {
            let handler_ptr = crate::signals::gtk_compat_trampoline_key_pressed as *const () as *mut c_void;
            let id = gsc(self.inner, sig_name.as_ptr(), handler_ptr, raw);
            Ok(id)
        } else {
            Err(Error::Other("no g_signal_connect available".into()))
        }
    }

    pub fn connect_key_press_event<F: FnMut(*mut c_void) -> i32 + 'static>(&self, f: F) -> Result<u64, Error> {
        guard_widget_or!(self, "EventControllerKey", "connect_key_press_event", Err(Error::Other("key controller dropped".into())));
        let boxed: Box<dyn FnMut(*mut c_void) -> i32> = Box::new(f);
        let res = unsafe { crate::signals::connect_signal_bool(&self.loader.symbols, self.inner, "key-press-event", boxed) };
        match res { Ok(id) => Ok(id), Err(e) => Err(Error::Other(e)) }
    }

    pub fn add_to_widget(&self, widget: &impl GtkWidget) {
        guard_widget!(self, "EventControllerKey", "add_to_widget");
        if let Some(add_ctrl) = self.loader.symbols.gtk_widget_add_controller {
            let widget_ptr = widget.widget_ptr();
            if !guard_widget_ptr(widget_ptr, "EventControllerKey", "add_to_widget.widget") {
                return;
            }
            unsafe { add_ctrl(widget_ptr, self.inner); }
        }
    }

    /// Get the keyval from a GDK key event
    ///
    /// # Safety
    /// `event` must be a valid GDK key event pointer.
    pub unsafe fn get_keyval(&self, event: *mut c_void) -> u32 {
        Self::get_keyval_static(&self.loader, event)
    }

    /// Static version of get_keyval that doesn't need a controller instance
    ///
    /// # Safety
    /// `event` must be a valid GDK key event pointer.
    pub unsafe fn get_keyval_static(loader: &Arc<Loader>, event: *mut c_void) -> u32 {
        if let Some(get_kv) = loader.symbols.gdk_event_get_keyval {
            unsafe { get_kv(event) }
        } else { 0 }
    }
}

impl Drop for EventControllerKey {
    fn drop(&mut self) {
        if let Some(unref) = self.loader.symbols.g_object_unref { unsafe { unref(self.inner); } }
        self.inner = std::ptr::null_mut();
    }
}

// ---- FileChooserNative wrapper ----
pub struct FileChooserNative {
    inner: *mut c_void,
    loader: Arc<Loader>,
}

impl FileChooserNative {
    /// # Safety
    /// `parent` must be a valid GtkWindow pointer or null.
    pub unsafe fn open(loader: Arc<Loader>, title: &str, parent: *mut c_void) -> Result<Self, Error> {
        Self::new(loader, title, parent, 0, "Open", None)
    }

    /// # Safety
    /// `parent` must be a valid GtkWindow pointer or null.
    pub unsafe fn save(loader: Arc<Loader>, title: &str, parent: *mut c_void) -> Result<Self, Error> {
        Self::new(loader, title, parent, 1, "Save", None)
    }

    fn new(loader: Arc<Loader>, title: &str, parent: *mut c_void, action: i32, accept_label: &str, cancel_label: Option<&str>) -> Result<Self, Error> {
        let symbols = &loader.symbols;
        let ctor = symbols.gtk_file_chooser_native_new.ok_or(Error::MissingSymbol("gtk_file_chooser_native_new".into()))?;
        let title_c = CString::new(title).unwrap();
        let accept_c = CString::new(accept_label).unwrap();
        let cancel_c = cancel_label.map(|s| CString::new(s).unwrap());
        let cancel_ptr = cancel_c.as_ref().map(|c| c.as_ptr()).unwrap_or(std::ptr::null::<i8>() as *const i8);
        let inner = unsafe { ctor(title_c.as_ptr(), parent, action, accept_c.as_ptr(), cancel_ptr) };
        if inner.is_null() { return Err(Error::Other("gtk_file_chooser_native_new returned null".into())); }
        Ok(FileChooserNative { inner, loader })
    }

    pub fn run(&self) -> i32 {
        if !guard_widget_ptr(self.inner, "FileChooserNative", "run") {
            return -1;
        }
        if let Some(run) = self.loader.symbols.gtk_native_dialog_run {
            unsafe { run(self.inner) }
        } else { -1 }
    }

    pub fn get_filename(&self) -> Option<String> {
        if !guard_widget_ptr(self.inner, "FileChooserNative", "get_filename") {
            return None;
        }
        if let Some(get_fn) = self.loader.symbols.gtk_file_chooser_get_filename {
            let ptr = unsafe { get_fn(self.inner) };
            if !ptr.is_null() {
                let filename = unsafe { std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned() };
                if let Some(gfree) = self.loader.symbols.g_free {
                    unsafe { gfree(ptr as *mut c_void); }
                }
                return Some(filename);
            }
        }
        None
    }

    pub fn destroy(&self) {
        if !guard_widget_ptr(self.inner, "FileChooserNative", "destroy") {
            return;
        }
        if let Some(destroy_fn) = self.loader.symbols.gtk_widget_destroy {
            unsafe { destroy_fn(self.inner); }
        }
    }
}

impl Drop for FileChooserNative {
    fn drop(&mut self) {
        self.destroy();
        self.inner = std::ptr::null_mut();
    }
}

// ---- CairoContext wrapper ----
/// Safe wrapper around a Cairo context (`cairo_t*`).
pub struct CairoContext<'a> {
    cr: *mut c_void,
    loader: &'a Arc<Loader>,
}

impl<'a> CairoContext<'a> {
    /// # Safety
    /// `cr` must be a valid, non-null `cairo_t*` pointer.
    pub unsafe fn new(loader: &'a Arc<Loader>, cr: *mut c_void) -> Self {
        CairoContext { cr, loader }
    }

    pub fn set_source_rgb(&self, r: f64, g: f64, b: f64) {
        if let Some(f) = self.loader.symbols.cairo_set_source_rgb { unsafe { f(self.cr, r, g, b); } }
    }

    pub fn set_source_rgba(&self, r: f64, g: f64, b: f64, a: f64) {
        if let Some(f) = self.loader.symbols.cairo_set_source_rgba { unsafe { f(self.cr, r, g, b, a); } }
    }

    pub fn rectangle(&self, x: f64, y: f64, w: f64, h: f64) {
        if let Some(f) = self.loader.symbols.cairo_rectangle { unsafe { f(self.cr, x, y, w, h); } }
    }

    pub fn fill(&self) {
        if let Some(f) = self.loader.symbols.cairo_fill { unsafe { f(self.cr); } }
    }

    pub fn stroke(&self) {
        if let Some(f) = self.loader.symbols.cairo_stroke { unsafe { f(self.cr); } }
    }

    pub fn move_to(&self, x: f64, y: f64) {
        if let Some(f) = self.loader.symbols.cairo_move_to { unsafe { f(self.cr, x, y); } }
    }

    pub fn line_to(&self, x: f64, y: f64) {
        if let Some(f) = self.loader.symbols.cairo_line_to { unsafe { f(self.cr, x, y); } }
    }

    pub fn set_line_width(&self, width: f64) {
        if let Some(f) = self.loader.symbols.cairo_set_line_width { unsafe { f(self.cr, width); } }
    }

    pub fn select_font_face(&self, family: &str, slant: i32, weight: i32) {
        if let Some(f) = self.loader.symbols.cairo_select_font_face {
            let c = CString::new(family).unwrap();
            unsafe { f(self.cr, c.as_ptr(), slant, weight); }
        }
    }

    pub fn set_font_size(&self, size: f64) {
        if let Some(f) = self.loader.symbols.cairo_set_font_size { unsafe { f(self.cr, size); } }
    }

    pub fn show_text(&self, text: &str) {
        if let Some(f) = self.loader.symbols.cairo_show_text {
            let c = CString::new(text).unwrap();
            unsafe { f(self.cr, c.as_ptr()); }
        }
    }

    pub fn text_extents(&self, text: &str) -> CairoTextExtents {
        if let Some(f) = self.loader.symbols.cairo_text_extents {
            let c = CString::new(text).unwrap();
            let mut ext: crate::symbols::CairoTextExtentsT = unsafe { std::mem::zeroed() };
            unsafe { f(self.cr, c.as_ptr(), &mut ext as *mut _ as *mut c_void); }
            CairoTextExtents {
                x_bearing: ext.x_bearing,
                y_bearing: ext.y_bearing,
                width: ext.width,
                height: ext.height,
                x_advance: ext.x_advance,
                y_advance: ext.y_advance,
            }
        } else {
            CairoTextExtents::default()
        }
    }

    pub fn save(&self) {
        if let Some(f) = self.loader.symbols.cairo_save { unsafe { f(self.cr); } }
    }

    pub fn restore(&self) {
        if let Some(f) = self.loader.symbols.cairo_restore { unsafe { f(self.cr); } }
    }

    pub fn clip(&self) {
        if let Some(f) = self.loader.symbols.cairo_clip { unsafe { f(self.cr); } }
    }

    pub fn paint(&self) {
        if let Some(f) = self.loader.symbols.cairo_paint { unsafe { f(self.cr); } }
    }
}

/// Result of cairo_text_extents
#[derive(Default, Clone, Debug)]
pub struct CairoTextExtents {
    pub x_bearing: f64,
    pub y_bearing: f64,
    pub width: f64,
    pub height: f64,
    pub x_advance: f64,
    pub y_advance: f64,
}

// ---- End of new wrappers ----

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
        unsafe { take_ownership(&symbols, &loader.version, inner); }
        if let Some(set_has_frame) = symbols.gtk_entry_set_has_frame { unsafe { set_has_frame(inner, 0); } }
        Ok(Entry { inner, loader, _not_send: PhantomData })
    }

    pub fn set_text(&self, text: &str) {
        guard_widget!(self, "Entry", "set_text");
        // GTK4 uses gtk_editable_set_text; GTK3 uses gtk_entry_set_text
        if let Some(set_text) = self.loader.symbols.gtk_editable_set_text {
            let c = CString::new(text).unwrap();
            unsafe { set_text(self.inner, c.as_ptr()); }
        } else if let Some(set_text) = self.loader.symbols.gtk_entry_set_text {
            let c = CString::new(text).unwrap();
            unsafe { set_text(self.inner, c.as_ptr()); }
        }
    }

    pub fn get_text(&self) -> Option<String> {
        guard_widget_or!(self, "Entry", "get_text", None);
        // GTK4 uses gtk_editable_get_text; GTK3 uses gtk_entry_get_text
        if let Some(get_text) = self.loader.symbols.gtk_editable_get_text {
            unsafe {
                let s = get_text(self.inner);
                if s.is_null() { return None; }
                let c = std::ffi::CStr::from_ptr(s);
                return Some(c.to_string_lossy().into_owned());
            }
        } else if let Some(get_text) = self.loader.symbols.gtk_entry_get_text {
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
        guard_widget!(self, "Entry", "set_width_chars");
        if let Some(w) = self.loader.symbols.gtk_entry_set_width_chars { unsafe { w(self.inner, n); } }
    }

    pub fn set_size_request(&self, w: i32, h: i32) {
        guard_widget!(self, "Entry", "set_size_request");
        if w < 150 {
            eprintln!("WARNING: Entry width {} is below GTK minimum (150px); the entry may not fit the intended column width", w);
        }
        if let Some(sr) = self.loader.symbols.gtk_widget_set_size_request { unsafe { sr(self.inner, w, h); } }
    }

    pub fn connect_changed<F: FnMut() + 'static>(&self, f: F) -> Result<u64, Error> {
        guard_widget_or!(self, "Entry", "connect_changed", Err(Error::Other("entry dropped".into())));
        let boxed: Box<dyn FnMut()> = Box::new(f);
        let res = unsafe { crate::signals::connect_signal(&self.loader.symbols, self.inner, "changed", boxed, 2) };
        match res {
            Ok(id) => Ok(id),
            Err(e) => Err(Error::Other(e)),
        }
    }

    pub fn connect_activate<F: FnMut(*mut c_void) + 'static>(&self, f: F) -> Result<u64, Error> {
        guard_widget_or!(self, "Entry", "connect_activate", Err(Error::Other("entry dropped".into())));
        let boxed: Box<Box<dyn FnMut(*mut c_void)>> = Box::new(Box::new(f));
        let res = unsafe { crate::signals::connect_signal_param(&self.loader.symbols, self.inner, "activate", boxed) };
        match res {
            Ok(id) => Ok(id),
            Err(e) => Err(Error::Other(e)),
        }
    }

    pub fn connect_button_press<F: FnMut() + 'static>(&self, f: F) -> Result<u64, Error> {
        guard_widget_or!(self, "Entry", "connect_button_press", Err(Error::Other("entry dropped".into())));
        let boxed: Box<dyn FnMut()> = Box::new(f);
        let res = unsafe { crate::signals::connect_signal(&self.loader.symbols, self.inner, "button-press-event", boxed, 3) };
        match res {
            Ok(id) => Ok(id),
            Err(e) => Err(Error::Other(e)),
        }
    }

    pub fn add_class(&self, class_name: &str) {
        guard_widget!(self, "Entry", "add_class");
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
        guard_widget!(self, "Entry", "remove_class");
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
        guard_widget!(self, "Entry", "grab_focus");
        if let Some(grab) = self.loader.symbols.gtk_widget_grab_focus { unsafe { grab(self.inner); } }
    }

    pub fn connect_focus_in_event<F: FnMut(*mut c_void) -> i32 + 'static>(&self, f: F) -> Result<u64, Error> {
        guard_widget_or!(self, "Entry", "connect_focus_in_event", Err(Error::Other("entry dropped".into())));
        let cb: Box<dyn FnMut(*mut c_void) -> i32> = Box::new(f);
        let res = unsafe { crate::signals::connect_signal_bool(&self.loader.symbols, self.inner, "focus-in-event", cb) };
        match res { Ok(id) => Ok(id), Err(e) => Err(Error::Other(e)) }
    }

    pub fn connect_focus_out_event<F: FnMut(*mut c_void) -> i32 + 'static>(&self, f: F) -> Result<u64, Error> {
        guard_widget_or!(self, "Entry", "connect_focus_out_event", Err(Error::Other("entry dropped".into())));
        let cb: Box<dyn FnMut(*mut c_void) -> i32> = Box::new(f);
        let res = unsafe { crate::signals::connect_signal_bool(&self.loader.symbols, self.inner, "focus-out-event", cb) };
        match res { Ok(id) => Ok(id), Err(e) => Err(Error::Other(e)) }
    }
}

impl AsRef<*mut c_void> for Entry { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for Entry { fn drop(&mut self) { unsafe { crate::wrappers::unref_widget(&self.loader, self.inner); } self.inner = std::ptr::null_mut(); }
}

// Measure text width in pixels using gtk_widget_create_pango_layout + pango_layout_get_pixel_size when possible.
/// # Safety
/// If `widget` is `Some`, the pointer must be a valid GtkWidget pointer.
pub unsafe fn measure_text_px(loader: &Arc<Loader>, widget: Option<*mut c_void>, text: &str) -> i32 {
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
        let c = CString::new(css).unwrap_or_default();
        match loader.version {
            Version::Gtk4 => {
                type LoadGtk4 = unsafe extern "C" fn(*mut c_void, *const i8, isize);
                if let Some(lib) = loader.libs.get("libgtk") {
                    if let Ok(f) = unsafe { lib.get::<LoadGtk4>(b"gtk_css_provider_load_from_data") } {
                        let fn4 = *f;
                        unsafe { fn4(provider, c.as_ptr(), c.as_bytes().len() as isize); }
                    }
                }
            }
            _ => {
                if let Some(load_fn) = loader.symbols.gtk_css_provider_load_from_data {
                    let mut err: *mut c_void = std::ptr::null_mut();
                    unsafe { load_fn(provider, c.as_ptr(), c.as_bytes().len() as isize, &mut err as *mut *mut c_void); }
                }
            }
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
pub unsafe fn widget_set_size_request(loader: &Arc<Loader>, widget: *mut c_void, w: i32, h: i32) {
    if let Some(sr) = loader.symbols.gtk_widget_set_size_request {
        unsafe { sr(widget, w, h); }
    }
}

/// Set widget margin start (left)
pub unsafe fn widget_set_margin_start(loader: &Arc<Loader>, widget: *mut c_void, margin: i32) {
    if let Some(set_margin) = loader.symbols.gtk_widget_set_margin_start {
        unsafe { set_margin(widget, margin); }
    }
}

/// Set widget margin top
pub unsafe fn widget_set_margin_top(loader: &Arc<Loader>, widget: *mut c_void, margin: i32) {
    if let Some(set_margin) = loader.symbols.gtk_widget_set_margin_top {
        unsafe { set_margin(widget, margin); }
    }
}

/// Add GTK event mask bits to a widget (GTK3).
pub unsafe fn widget_add_events(loader: &Arc<Loader>, widget: *mut c_void, events: i32) {
    if let Some(add_events) = loader.symbols.gtk_widget_add_events {
        unsafe { add_events(widget, events); }
    }
}

/// Set widget hexpand
pub unsafe fn widget_set_hexpand(loader: &Arc<Loader>, widget: *mut c_void, expand: bool) {
    if let Some(set) = loader.symbols.gtk_widget_set_hexpand {
        unsafe { set(widget, if expand { 1 } else { 0 }); }
    }
}

/// Set widget vexpand
pub unsafe fn widget_set_vexpand(loader: &Arc<Loader>, widget: *mut c_void, expand: bool) {
    if let Some(set) = loader.symbols.gtk_widget_set_vexpand {
        unsafe { set(widget, if expand { 1 } else { 0 }); }
    }
}

/// Set widget horizontal alignment
pub unsafe fn widget_set_halign(loader: &Arc<Loader>, widget: *mut c_void, align: i32) {
    if let Some(set) = loader.symbols.gtk_widget_set_halign {
        unsafe { set(widget, align); }
    }
}

/// Set widget vertical alignment
pub unsafe fn widget_set_valign(loader: &Arc<Loader>, widget: *mut c_void, align: i32) {
    if let Some(set) = loader.symbols.gtk_widget_set_valign {
        unsafe { set(widget, align); }
    }
}

/// Set whether a widget can be the target of pointer events
pub unsafe fn widget_set_can_target(loader: &Arc<Loader>, widget: *mut c_void, can_target: bool) {
    if let Some(set) = loader.symbols.gtk_widget_set_can_target {
        unsafe { set(widget, if can_target { 1 } else { 0 }); }
    }
}

/// Set widget visibility
pub unsafe fn widget_set_visible(loader: &Arc<Loader>, widget: *mut c_void, visible: bool) {
    if let Some(f) = loader.symbols.gtk_widget_set_visible {
        unsafe { f(widget, if visible { 1 } else { 0 }); }
    }
}

/// Show a widget and all its descendants (GTK3).
pub unsafe fn widget_show_all(loader: &Arc<Loader>, widget: *mut c_void) {
    if let Some(show_all) = loader.symbols.gtk_widget_show_all {
        unsafe { show_all(widget); }
    }
}

/// Run a callback on the next GTK main-loop idle turn.
pub unsafe fn idle_add_once(loader: &Arc<Loader>, cb: Box<dyn FnMut()>) {
    if let Some(idle_add) = loader.symbols.g_idle_add {
        let raw = Box::into_raw(Box::new(cb)) as *mut c_void;
        unsafe { idle_add(Some(idle_once_trampoline), raw); }
    } else {
        let mut cb = cb;
        cb();
    }
}

/// Unparent a widget (GTK4) — remove from its parent container
pub unsafe fn widget_unparent(loader: &Arc<Loader>, widget: *mut c_void) {
    if let Some(unparent) = loader.symbols.gtk_widget_unparent {
        unsafe { unparent(widget); }
    }
}

/// Connect to a widget signal where the handler returns a boolean (e.g. key-press-event, button-press-event).
/// The closure receives the event pointer and should return 0 (propagate) or 1 (stop).
pub unsafe fn widget_connect_signal_bool(
    loader: &Arc<Loader>,
    widget: *mut c_void,
    signal_name: &str,
    cb: Box<dyn FnMut(*mut c_void) -> i32>,
) -> Result<u64, Error> {
    let res = unsafe { crate::signals::connect_signal_bool(&loader.symbols, widget, signal_name, cb) };
    match res {
        Ok(id) => Ok(id),
        Err(e) => Err(Error::Other(e)),
    }
}

/// Queue a redraw on a widget
pub unsafe fn widget_queue_draw(loader: &Arc<Loader>, widget: *mut c_void) {
    if let Some(q) = loader.symbols.gtk_widget_queue_draw { unsafe { q(widget); } }
}

/// Create a GtkGestureClick, add it to `target_widget`, and connect its `pressed` signal.
/// The widget takes full ownership of the gesture — no Rust handle is returned.
/// Returns the signal handler ID on success.
pub unsafe fn connect_gesture_click_pressed(
    loader: &Arc<Loader>,
    target_widget: *mut c_void,
    cb: Box<dyn FnMut(i32, f64, f64)>,
) -> Result<u64, Error> {
    if let Some(ctor) = loader.symbols.gtk_gesture_click_new {
        let gesture = unsafe { ctor() };
        if gesture.is_null() {
            return Err(Error::Other("gtk_gesture_click_new returned null".into()));
        }
        // The gesture has a floating ref. gtk_widget_add_controller takes ownership.
        if let Some(add_ctrl) = loader.symbols.gtk_widget_add_controller {
            unsafe { add_ctrl(target_widget, gesture); }
        }
        let res = unsafe { crate::signals::connect_signal_gesture(&loader.symbols, gesture, "pressed", cb) };
        match res {
            Ok(id) => Ok(id),
            Err(e) => Err(Error::Other(e)),
        }
    } else {
        Err(Error::MissingSymbol("gtk_gesture_click_new".into()))
    }
}

/// Get coordinates from a GDK event. Returns `None` if the symbol is unavailable.
pub unsafe fn gdk_event_get_coords(loader: &Arc<Loader>, event: *mut c_void) -> Option<(f64, f64)> {
    type GetEventCoords = unsafe extern "C" fn(*mut std::ffi::c_void, *mut f64, *mut f64) -> i32;

    let get_coords = loader.libs.get("libgdk").and_then(|gdk_lib| {
        unsafe { gdk_lib.get::<GetEventCoords>(b"gdk_event_get_coords").ok().map(|s| *s) }
    }).or_else(|| {
        loader.libs.get("libgtk").and_then(|gtk_lib| {
            unsafe { gtk_lib.get::<GetEventCoords>(b"gdk_event_get_coords").ok().map(|s| *s) }
        })
    });

    if let Some(get_coords) = get_coords {
        let mut x: f64 = 0.0;
        let mut y: f64 = 0.0;
        if unsafe { get_coords(event, &mut x as *mut f64, &mut y as *mut f64) } != 0 {
            return Some((x, y));
        }
    }
    None
}

/// Destroy a widget (remove from parent) and release the reference held by
/// [`take_ownership`] (via `g_object_ref_sink`).  Without the extra unref the
/// widget is never freed and a later reuse of the pointer causes a segfault.
pub unsafe fn destroy_widget(loader: &Arc<Loader>, widget: *mut c_void) {
    if let Some(destroy) = loader.symbols.gtk_widget_destroy {
        // GTK3: gtk_widget_destroy removes from parent and container ref
        unsafe { destroy(widget); }
        // Release the reference take_ownership acquired
        if let Some(unref) = loader.symbols.g_object_unref {
            unsafe { unref(widget); }
        }
    } else {
        // GTK4: first unparent (remove from parent)
        if let Some(unparent) = loader.symbols.gtk_widget_unparent {
            unsafe { unparent(widget); }
        }
        // then release the reference take_ownership acquired
        if let Some(unref) = loader.symbols.g_object_unref {
            unsafe { unref(widget); }
        }
    }
}

/// Release a widget reference without destroying or unparenting.
/// This is used in Drop impls to balance `g_object_ref` from Clone /
/// `g_object_ref_sink` from take_ownership, without interfering with
/// GTK's own parent-child destruction cascade.
pub unsafe fn unref_widget(loader: &Arc<Loader>, widget: *mut c_void) {
    if let Some(unref) = loader.symbols.g_object_unref {
        unsafe { unref(widget); }
    }
}

/// Remove a child widget from its parent without unreffing.
/// GTK4: uses gtk_widget_unparent. GTK3: prefers gtk_container_remove(parent,
/// widget) when the parent can be queried, and only falls back to
/// gtk_widget_destroy if parent lookup is unavailable.
pub unsafe fn remove_from_parent(loader: &Arc<Loader>, widget: *mut c_void) {
    if let Some(unparent) = loader.symbols.gtk_widget_unparent {
        unsafe { unparent(widget); }
    } else if let (Some(get_parent), Some(container_remove)) = (
        loader.symbols.gtk_widget_get_parent,
        loader.symbols.gtk_container_remove,
    ) {
        let parent = unsafe { get_parent(widget) };
        if !parent.is_null() {
            unsafe { container_remove(parent, widget); }
        }
    } else if let Some(destroy) = loader.symbols.gtk_widget_destroy {
        unsafe { destroy(widget); }
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
        unsafe { take_ownership(&symbols, &loader.version, inner); }
        Ok(Menu { inner, loader, items: Vec::new() })
    }

    pub fn append(&mut self, label: &str, detailed_action: &str) {
        guard_widget!(self, "Menu", "append");
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
        guard_widget!(self, "Menu", "append_submenu");
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
        unsafe { crate::wrappers::unref_widget(&self.loader, self.inner); }
        self.inner = std::ptr::null_mut();
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
        unsafe { take_ownership(&symbols, &loader.version, inner); }
        Ok(SimpleAction { inner, loader })
    }

    pub fn ptr(&self) -> *mut c_void { self.inner }

    pub fn connect_activate<F: FnMut(*mut c_void) + 'static>(&self, f: F) -> Result<u64, Error> {
        guard_widget_or!(self, "SimpleAction", "connect_activate", Err(Error::Other("simple action dropped".into())));
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
        unsafe { crate::wrappers::unref_widget(&self.loader, self.inner); }
        self.inner = std::ptr::null_mut();
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
    /// # Safety
    /// `action_group` must be a valid GActionGroup pointer or null.
    pub unsafe fn new(loader: Arc<Loader>, model: &Menu, action_group: *mut c_void) -> Result<Self, Error> {
        let symbols = &loader.symbols;
        // GTK4: GtkPopoverMenuBar — uses the GMenuModel directly
        if let Some(ctor) = symbols.gtk_popover_menu_bar_new_from_model {
            let inner = unsafe { ctor(model.ptr()) };
            if inner.is_null() {
                return Err(Error::Other("gtk_popover_menu_bar_new_from_model returned null".into()));
            }
            take_ownership(&symbols, &loader.version, inner);
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
            take_ownership(&symbols, &loader.version, inner);
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
        unsafe { crate::wrappers::unref_widget(&self.loader, self.inner); }
    }
}

// ---- Dialog ----
pub struct Dialog {
    inner: *mut c_void,
    loader: Arc<Loader>,
    _not_send: PhantomData<Rc<()>>,
}

impl Dialog {
    pub fn new(loader: Arc<Loader>) -> Result<Self, Error> {
        let symbols = &loader.symbols;
        let ctor = symbols.gtk_dialog_new.ok_or(Error::MissingSymbol("gtk_dialog_new".into()))?;
        let inner = unsafe { ctor() };
        if inner.is_null() { return Err(Error::Other("gtk_dialog_new returned null".into())); }
        unsafe { take_ownership(&symbols, &loader.version, inner); }
        Ok(Dialog { inner, loader, _not_send: PhantomData })
    }

    pub fn set_title(&self, title: &str) {
        guard_widget!(self, "Dialog", "set_title");
        if let Some(set_title) = self.loader.symbols.gtk_window_set_title {
            let c = CString::new(title).unwrap();
            unsafe { set_title(self.inner, c.as_ptr()); }
        }
    }

    pub fn set_default_size(&self, width: i32, height: i32) {
        guard_widget!(self, "Dialog", "set_default_size");
        if let Some(set_size) = self.loader.symbols.gtk_dialog_set_default_size {
            unsafe { set_size(self.inner, width, height); }
        }
    }

    pub fn add_button(&self, button_text: &str, response_id: i32) {
        guard_widget!(self, "Dialog", "add_button");
        if let Some(add_btn) = self.loader.symbols.gtk_dialog_add_button {
            let c = CString::new(button_text).unwrap();
            unsafe { add_btn(self.inner, c.as_ptr(), response_id); }
        }
    }

    pub fn get_content_area(&self) -> *mut c_void {
        if !guard_widget_ptr(self.inner, "Dialog", "get_content_area") {
            return std::ptr::null_mut();
        }
        if let Some(get_area) = self.loader.symbols.gtk_dialog_get_content_area {
            unsafe { get_area(self.inner) }
        } else {
            self.inner
        }
    }

    pub fn append_content_area(&self, child: &impl GtkWidget) {
        guard_widget!(self, "Dialog", "append_content_area");
        let content = self.get_content_area();
        if !guard_widget_ptr(content, "Dialog", "append_content_area.content") {
            return;
        }
        let child_ptr = child.widget_ptr();
        if !guard_widget_ptr(child_ptr, "Dialog", "append_content_area.child") {
            return;
        }
        if let Some(box_append) = self.loader.symbols.gtk_box_append {
            unsafe { box_append(content, child_ptr); }
        } else if let Some(container_add) = self.loader.symbols.gtk_container_add {
            unsafe { container_add(content, child_ptr); }
        }
    }

    pub fn present(&self) {
        guard_widget!(self, "Dialog", "present");
        if let Some(show_all) = self.loader.symbols.gtk_widget_show_all {
            unsafe { show_all(self.inner); }
        }
        if let Some(present) = self.loader.symbols.gtk_window_present {
            unsafe { present(self.inner); }
        }
    }

    pub fn connect_response<F: FnMut(i32) + 'static>(&self, mut f: F) -> Result<u64, Error> {
        guard_widget_or!(self, "Dialog", "connect_response", Err(Error::Other("dialog dropped".into())));
        let boxed: Box<Box<dyn FnMut(*mut c_void, i32)>> = Box::new(Box::new(move |_dialog, response_id| {
            f(response_id);
        }));
        let raw = Box::into_raw(boxed) as *mut c_void;
        let sig_name = CString::new("response").unwrap();
        if let Some(gscd) = self.loader.symbols.g_signal_connect_data {
            extern "C" fn trampoline_response(_instance: *mut c_void, response_id: i32, user_data: *mut c_void) {
                unsafe {
                    if user_data.is_null() { return; }
                    let inner_ptr = user_data as *mut Box<dyn FnMut(*mut c_void, i32)>;
                    if inner_ptr.is_null() { return; }
                    (*inner_ptr)(_instance, response_id);
                }
            }
            extern "C" fn destroy_response(data: *mut c_void, _closure: *mut c_void) {
                unsafe { let _boxed: Box<Box<dyn FnMut(*mut c_void, i32)>> = Box::from_raw(data as *mut _); }
            }
            unsafe {
                let id = gscd(self.inner, sig_name.as_ptr(), trampoline_response as *const () as *mut c_void, raw, Some(destroy_response as unsafe extern "C" fn(*mut c_void, *mut c_void)), 0);
                Ok(id)
            }
        } else if let Some(gsc) = self.loader.symbols.g_signal_connect {
            extern "C" fn trampoline_response_simple(_instance: *mut c_void, response_id: i32, user_data: *mut c_void) {
                unsafe {
                    if user_data.is_null() { return; }
                    let inner_ptr = user_data as *mut Box<dyn FnMut(*mut c_void, i32)>;
                    if inner_ptr.is_null() { return; }
                    (*inner_ptr)(_instance, response_id);
                }
            }
            unsafe {
                let id = gsc(self.inner, sig_name.as_ptr(), trampoline_response_simple as *const () as *mut c_void, raw);
                Ok(id)
            }
        } else {
            Err(Error::MissingSymbol("g_signal_connect_data".into()))
        }
    }

    pub fn response(&self, _response_id: i32) {
        guard_widget!(self, "Dialog", "response");
        if let Some(emit) = self.loader.symbols.g_signal_emit_by_name {
            let name = CString::new("response").unwrap();
            // We need to pass response_id as a parameter; but g_signal_emit_by_name only takes
            // instance and name. For simplicity, just emit the signal without the param.
            unsafe { emit(self.inner, name.as_ptr()); }
        }
    }
}

impl AsRef<*mut c_void> for Dialog { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for Dialog {
    fn drop(&mut self) {
        unsafe { crate::wrappers::unref_widget(&self.loader, self.inner); }
        self.inner = std::ptr::null_mut();
    }
}

// ---- DropDown (combo box) ----
pub struct DropDown {
    inner: *mut c_void,
    loader: Arc<Loader>,
    _not_send: PhantomData<Rc<()>>,
    // For GTK4, we keep a reference to the string list so it stays alive
    string_list: Option<*mut c_void>,
}

impl DropDown {
    pub fn new(loader: Arc<Loader>, items: &[&str]) -> Result<Self, Error> {
        let symbols = &loader.symbols;
        // Try GTK4 DropDown API first
        if let (Some(dd_new), Some(sl_new)) = (symbols.gtk_drop_down_new, symbols.gtk_string_list_new) {
            let c_strings: Vec<CString> = items.iter().map(|s| CString::new(*s).unwrap()).collect();
            let mut raw_ptrs: Vec<*const i8> = c_strings.iter().map(|c| c.as_ptr()).collect();
            raw_ptrs.push(std::ptr::null());
            let string_list = unsafe { sl_new(raw_ptrs.as_ptr()) };
            if string_list.is_null() {
                return Err(Error::Other("gtk_string_list_new returned null".into()));
            }
            let inner = unsafe { dd_new(string_list, std::ptr::null_mut()) };
            if inner.is_null() {
                return Err(Error::Other("gtk_drop_down_new returned null".into()));
            }
            unsafe { take_ownership(&symbols, &loader.version, inner); }
            Ok(DropDown { inner, loader, _not_send: PhantomData, string_list: Some(string_list) })
        }
        // Fall back to GTK3 ComboBoxText API
        else if let (Some(ct_new), Some(ct_append)) = (symbols.gtk_combo_box_text_new, symbols.gtk_combo_box_text_append_text) {
            let inner = unsafe { ct_new() };
            if inner.is_null() {
                return Err(Error::Other("gtk_combo_box_text_new returned null".into()));
            }
            for item in items {
                let c = CString::new(*item).unwrap();
                unsafe { ct_append(inner, c.as_ptr()); }
            }
            unsafe { take_ownership(&symbols, &loader.version, inner); }
            Ok(DropDown { inner, loader, _not_send: PhantomData, string_list: None })
        } else {
            Err(Error::MissingSymbol("gtk_drop_down_new or gtk_combo_box_text_new".into()))
        }
    }

    pub fn set_active(&self, index: u32) {
        guard_widget!(self, "DropDown", "set_active");
        let symbols = &self.loader.symbols;
        if let Some(set_sel) = symbols.gtk_drop_down_set_selected {
            unsafe { set_sel(self.inner, index); }
        } else if let Some(set_act) = symbols.gtk_combo_box_set_active {
            unsafe { set_act(self.inner, index as i32); }
        }
    }

    pub fn get_active(&self) -> i32 {
        guard_widget_or!(self, "DropDown", "get_active", -1);
        let symbols = &self.loader.symbols;
        if let Some(get_sel) = symbols.gtk_drop_down_get_selected {
            unsafe { get_sel(self.inner) as i32 }
        } else if let Some(get_act) = symbols.gtk_combo_box_get_active {
            unsafe { get_act(self.inner) }
        } else { -1 }
    }

    pub fn connect_changed<F: FnMut() + 'static>(&self, f: F) -> Result<u64, Error> {
        guard_widget_or!(self, "DropDown", "connect_changed", Err(Error::Other("dropdown dropped".into())));
        let boxed: Box<dyn FnMut()> = Box::new(f);
        if self.loader.symbols.gtk_drop_down_new.is_some() {
            // "notify::selected" has 3 args: (object, pspec, user_data)
            let res = unsafe { crate::signals::connect_signal(&self.loader.symbols, self.inner, "notify::selected", boxed, 3) };
            match res { Ok(id) => Ok(id), Err(e) => Err(Error::Other(e)) }
        } else {
            // GtkComboBoxText "changed" has 2 args: (widget, user_data)
            let res = unsafe { crate::signals::connect_signal(&self.loader.symbols, self.inner, "changed", boxed, 2) };
            match res { Ok(id) => Ok(id), Err(e) => Err(Error::Other(e)) }
        }
    }
}

impl AsRef<*mut c_void> for DropDown { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Clone for DropDown {
    fn clone(&self) -> Self {
        if let Some(gref) = self.loader.symbols.g_object_ref {
            unsafe {
                gref(self.inner);
                if let Some(sl) = self.string_list { gref(sl); }
            }
        }
        DropDown { inner: self.inner, loader: self.loader.clone(), _not_send: PhantomData, string_list: self.string_list }
    }
}

impl Drop for DropDown {
    fn drop(&mut self) {
        unsafe { crate::wrappers::unref_widget(&self.loader, self.inner); }
        self.inner = std::ptr::null_mut();
        if let Some(sl) = self.string_list {
            if let Some(unref) = self.loader.symbols.g_object_unref {
                unsafe { unref(sl); }
            }
        }
    }
}

// ---- CheckButton (Checkbox) ----
pub struct CheckButton {
    inner: *mut c_void,
    loader: Arc<Loader>,
    _not_send: PhantomData<Rc<()>>,
}

impl CheckButton {
    pub fn new(loader: Arc<Loader>, label: &str) -> Result<Self, Error> {
        let symbols = &loader.symbols;
        let ctor = symbols.gtk_check_button_new_with_label.ok_or(Error::MissingSymbol("gtk_check_button_new_with_label".into()))?;
        let c = CString::new(label).unwrap();
        let inner = unsafe { ctor(c.as_ptr()) };
        if inner.is_null() { return Err(Error::Other("gtk_check_button_new_with_label returned null".into())); }
        unsafe { take_ownership(&symbols, &loader.version, inner); }
        Ok(CheckButton { inner, loader, _not_send: PhantomData })
    }

    pub fn is_active(&self) -> bool {
        guard_widget_or!(self, "CheckButton", "is_active", false);
        // GTK4: GtkCheckButton is NOT a GtkToggleButton subclass; use its own API
        if let Some(get_active) = self.loader.symbols.gtk_check_button_get_active {
            unsafe { get_active(self.inner) != 0 }
        } else if let Some(get_active) = self.loader.symbols.gtk_toggle_button_get_active {
            unsafe { get_active(self.inner) != 0 }
        } else { false }
    }

    pub fn set_active(&self, active: bool) {
        guard_widget!(self, "CheckButton", "set_active");
        if let Some(set_active) = self.loader.symbols.gtk_check_button_set_active {
            unsafe { set_active(self.inner, if active { 1 } else { 0 }); }
        } else if let Some(set_active) = self.loader.symbols.gtk_toggle_button_set_active {
            unsafe { set_active(self.inner, if active { 1 } else { 0 }); }
        }
    }

    pub fn connect_toggled<F: FnMut() + 'static>(&self, f: F) -> Result<u64, Error> {
        guard_widget_or!(self, "CheckButton", "connect_toggled", Err(Error::Other("check button dropped".into())));
        let boxed: Box<dyn FnMut()> = Box::new(f);
        let res = unsafe { crate::signals::connect_signal(&self.loader.symbols, self.inner, "toggled", boxed, 2) };
        match res { Ok(id) => Ok(id), Err(e) => Err(Error::Other(e)) }
    }
}

impl AsRef<*mut c_void> for CheckButton { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for CheckButton {
    fn drop(&mut self) {
        unsafe { crate::wrappers::unref_widget(&self.loader, self.inner); }
        self.inner = std::ptr::null_mut();
    }
}

impl Clone for CheckButton {
    fn clone(&self) -> Self {
        if let Some(gref) = self.loader.symbols.g_object_ref { unsafe { gref(self.inner); } }
        CheckButton { inner: self.inner, loader: self.loader.clone(), _not_send: PhantomData }
    }
}

// ---- RadioButton (ChooseBox) ----
pub struct RadioButton {
    inner: *mut c_void,
    loader: Arc<Loader>,
    _not_send: PhantomData<Rc<()>>,
}

impl RadioButton {
    pub fn new(loader: Arc<Loader>, group: Option<&RadioButton>, label: &str) -> Result<Self, Error> {
        let symbols = &loader.symbols;
        // GTK3: use gtk_radio_button_new_with_label
        if let Some(ctor) = symbols.gtk_radio_button_new_with_label {
            let c = CString::new(label).unwrap();
            let group_ptr = group.map(|r| r.inner).unwrap_or(std::ptr::null_mut());
            let inner = unsafe { ctor(group_ptr, c.as_ptr()) };
            if inner.is_null() { return Err(Error::Other("gtk_radio_button_new_with_label returned null".into())); }
            unsafe { take_ownership(&symbols, &loader.version, inner); }
            return Ok(RadioButton { inner, loader, _not_send: PhantomData });
        }
        // GTK4: GtkRadioButton was removed; use GtkCheckButton with set_group
        if let (Some(ctor_cb), Some(set_group)) = (symbols.gtk_check_button_new_with_label, symbols.gtk_check_button_set_group) {
            let c = CString::new(label).unwrap();
            let inner = unsafe { ctor_cb(c.as_ptr()) };
            if inner.is_null() { return Err(Error::Other("gtk_check_button_new_with_label returned null".into())); }
            if let Some(g) = group {
                unsafe { set_group(inner, g.inner); }
            }
            unsafe { take_ownership(&symbols, &loader.version, inner); }
            return Ok(RadioButton { inner, loader, _not_send: PhantomData });
        }
        Err(Error::MissingSymbol("gtk_radio_button_new_with_label or gtk_check_button_new_with_label".into()))
    }

    pub fn is_active(&self) -> bool {
        guard_widget_or!(self, "RadioButton", "is_active", false);
        if let Some(get_active) = self.loader.symbols.gtk_check_button_get_active {
            unsafe { get_active(self.inner) != 0 }
        } else if let Some(get_active) = self.loader.symbols.gtk_toggle_button_get_active {
            unsafe { get_active(self.inner) != 0 }
        } else { false }
    }

    pub fn set_active(&self, active: bool) {
        guard_widget!(self, "RadioButton", "set_active");
        if let Some(set_active) = self.loader.symbols.gtk_check_button_set_active {
            unsafe { set_active(self.inner, if active { 1 } else { 0 }); }
        } else if let Some(set_active) = self.loader.symbols.gtk_toggle_button_set_active {
            unsafe { set_active(self.inner, if active { 1 } else { 0 }); }
        }
    }

    pub fn connect_toggled<F: FnMut() + 'static>(&self, f: F) -> Result<u64, Error> {
        guard_widget_or!(self, "RadioButton", "connect_toggled", Err(Error::Other("radio button dropped".into())));
        let boxed: Box<dyn FnMut()> = Box::new(f);
        let res = unsafe { crate::signals::connect_signal(&self.loader.symbols, self.inner, "toggled", boxed, 2) };
        match res { Ok(id) => Ok(id), Err(e) => Err(Error::Other(e)) }
    }
}

impl AsRef<*mut c_void> for RadioButton { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for RadioButton {
    fn drop(&mut self) {
        unsafe { crate::wrappers::unref_widget(&self.loader, self.inner); }
        self.inner = std::ptr::null_mut();
    }
}

impl Clone for RadioButton {
    fn clone(&self) -> Self {
        if let Some(gref) = self.loader.symbols.g_object_ref { unsafe { gref(self.inner); } }
        RadioButton { inner: self.inner, loader: self.loader.clone(), _not_send: PhantomData }
    }
}

// ---- TextView (TextArea) ----
pub struct TextView {
    inner: *mut c_void,
    loader: Arc<Loader>,
    _not_send: PhantomData<Rc<()>>,
}

impl TextView {
    pub fn new(loader: Arc<Loader>) -> Result<Self, Error> {
        let symbols = &loader.symbols;
        let ctor = symbols.gtk_text_view_new.ok_or(Error::MissingSymbol("gtk_text_view_new".into()))?;
        let inner = unsafe { ctor() };
        if inner.is_null() { return Err(Error::Other("gtk_text_view_new returned null".into())); }
        unsafe { take_ownership(&symbols, &loader.version, inner); }
        Ok(TextView { inner, loader, _not_send: PhantomData })
    }

    pub fn set_text(&self, text: &str) {
        guard_widget!(self, "TextView", "set_text");
        let symbols = &self.loader.symbols;
        if let (Some(get_buf), Some(set_text)) = (symbols.gtk_text_view_get_buffer, symbols.gtk_text_buffer_set_text) {
            let buf = unsafe { get_buf(self.inner) };
            if !buf.is_null() {
                let c = CString::new(text).unwrap();
                unsafe { set_text(buf, c.as_ptr(), -1); }
            }
        }
    }

    pub fn get_text(&self) -> Option<String> {
        guard_widget_or!(self, "TextView", "get_text", None);
        let symbols = &self.loader.symbols;
        if let (Some(get_buf), Some(get_start), Some(get_end), Some(get_text_fn)) = (
            symbols.gtk_text_view_get_buffer,
            symbols.gtk_text_buffer_get_start_iter,
            symbols.gtk_text_buffer_get_end_iter,
            symbols.gtk_text_buffer_get_text,
        ) {
            unsafe {
                let buf = get_buf(self.inner);
                if buf.is_null() { return None; }
                // GtkTextIter is opaque; allocate generously (256 bytes)
                let mut start_iter: [u8; 256] = [0; 256];
                let mut end_iter: [u8; 256] = [0; 256];
                get_start(buf, start_iter.as_mut_ptr() as *mut c_void);
                get_end(buf, end_iter.as_mut_ptr() as *mut c_void);
                let c_str = get_text_fn(buf, start_iter.as_mut_ptr() as *mut c_void, end_iter.as_mut_ptr() as *mut c_void, 1);
                if c_str.is_null() { return None; }
                let s = std::ffi::CStr::from_ptr(c_str as *const i8).to_string_lossy().into_owned();
                if let Some(free_fn) = symbols.g_free { free_fn(c_str as *mut c_void); }
                Some(s)
            }
        } else { None }
    }

    pub fn set_wrap_mode(&self, wrap_mode: i32) {
        guard_widget!(self, "TextView", "set_wrap_mode");
        if let Some(set_wrap) = self.loader.symbols.gtk_text_view_set_wrap_mode {
            unsafe { set_wrap(self.inner, wrap_mode); }
        }
    }

    pub fn set_size_request(&self, w: i32, h: i32) {
        guard_widget!(self, "TextView", "set_size_request");
        if let Some(sr) = self.loader.symbols.gtk_widget_set_size_request { unsafe { sr(self.inner, w, h); } }
    }

    pub fn set_hexpand(&self, expand: bool) {
        guard_widget!(self, "TextView", "set_hexpand");
        if let Some(set_hex) = self.loader.symbols.gtk_widget_set_hexpand { unsafe { set_hex(self.inner, if expand { 1 } else { 0 }); } }
    }

    pub fn set_vexpand(&self, expand: bool) {
        guard_widget!(self, "TextView", "set_vexpand");
        if let Some(set_vex) = self.loader.symbols.gtk_widget_set_vexpand { unsafe { set_vex(self.inner, if expand { 1 } else { 0 }); } }
    }
}

impl AsRef<*mut c_void> for TextView { fn as_ref(&self) -> &*mut c_void { &self.inner } }

impl Drop for TextView {
    fn drop(&mut self) {
        if let Some(unref) = self.loader.symbols.g_object_unref { unsafe { unref(self.inner); } }
        self.inner = std::ptr::null_mut();
    }
}

impl Clone for TextView {
    fn clone(&self) -> Self {
        if let Some(gref) = self.loader.symbols.g_object_ref { unsafe { gref(self.inner); } }
        TextView { inner: self.inner, loader: self.loader.clone(), _not_send: PhantomData }
    }
}

/// Take ownership of a newly-created GtkWidget.
/// GTK3: widgets start with a floating reference; sink it so we own ref count 1.
/// GTK4: `ref_sink` on a non-floating ref increments count (minor leak tolerated).
pub unsafe fn take_ownership(symbols: &crate::symbols::Symbols, _version: &crate::loader::Version, inner: *mut c_void) {
    if inner.is_null() { return; }
    if let Some(ref_sink) = symbols.g_object_ref_sink {
        ref_sink(inner);
    } else if let Some(gref) = symbols.g_object_ref {
        gref(inner);
    }
}
