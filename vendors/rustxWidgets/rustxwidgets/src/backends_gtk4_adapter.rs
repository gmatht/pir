// GTK4 backend adapter using gtk4-rs with dlopen-loaded sys crates.
use std::cell::RefCell;
use std::rc::Rc;
use crate::core::{DrawContext, Error};

use gtk4::{self, gio, glib, cairo, gdk};
use gtk4::prelude::*;
use glib::translate::*;

fn ensure_dlopen() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        gtk4_sys::__dlopen_ensure_loaded();
        glib_sys::__dlopen_ensure_loaded();
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Orientation { Horizontal, Vertical }

type CList = Rc<RefCell<Vec<Box<dyn std::any::Any>>>>;
fn new_controllers() -> CList { Rc::new(RefCell::new(Vec::new())) }

fn shid_to_u64(id: glib::SignalHandlerId) -> u64 {
    unsafe { std::mem::transmute(id) }
}
fn key_to_u32(key: gdk::Key) -> u32 { unsafe { std::mem::transmute(key) } }

#[derive(Clone)]
pub struct Application(pub gtk4::Application);
impl Application {
    pub fn register(&self) -> Result<(), Error> {
        self.0.register(gio::Cancellable::NONE)
            .map_err(|e| Error::Backend(e.to_string()))
    }
    pub fn as_ptr(&self) -> *mut std::ffi::c_void {
        self.0.as_ptr() as *mut std::ffi::c_void
    }
    pub fn add_action(&self, action: &SimpleAction) -> Result<(), Error> {
        self.0.add_action(&action.0); Ok(())
    }
}

#[derive(Clone)]
pub struct SimpleAction(pub gio::SimpleAction);
impl SimpleAction {
    pub fn connect_activate<F: FnMut(*mut std::ffi::c_void) + 'static>(&self, f: F) -> Result<u64, Error> {
        let f = RefCell::new(f);
        Ok(shid_to_u64(self.0.connect_activate(move |_, _| (f.borrow_mut())(std::ptr::null_mut()))))
    }
}

#[derive(Clone)]
pub struct Window(pub gtk4::Window, CList);
impl Window {
    pub fn new() -> Self { ensure_dlopen(); Window(gtk4::Window::new(), new_controllers()) }
    pub fn set_title(&self, title: &str) { self.0.set_title(Some(title)); }
    pub fn set_default_size(&self, w: i32, h: i32) { self.0.set_default_size(w, h); }
    pub fn present(&self) { self.0.present(); }
    pub unsafe fn insert_action_group(&self, name: &str, p: *mut std::ffi::c_void) {
        let group = gio::ActionGroup::from_glib_none(p as *mut _);
        self.0.insert_action_group(name, Some(&group));
    }
    pub fn hwnd(&self) -> *mut std::ffi::c_void { self.0.as_ptr() as *mut _ }
    pub fn set_child_box(&self, bx: &BoxWidget) { self.0.set_child(Some(&bx.0)); }
    pub fn on_event(&self, _cb: Box<dyn FnMut(*mut std::ffi::c_void) -> i32>) {}
    pub fn on_event_key(&self, cb: Box<dyn FnMut(u32, u32) -> i32>) {
        let c = RefCell::new(cb);
        let ctrl = gtk4::EventControllerKey::new();
        ctrl.connect_key_pressed(move |_, keyval, _, state| {
            if c.borrow_mut()(key_to_u32(keyval), state.bits()) != 0 { glib::Propagation::Stop } else { glib::Propagation::Proceed }
        });
        self.0.upcast_ref::<gtk4::Widget>().add_controller(ctrl.clone());
        self.1.borrow_mut().push(Box::new(ctrl));
    }
    pub fn on_close(&self, cb: Box<dyn FnMut()>) {
        let cb = RefCell::new(cb);
        self.0.connect_close_request(move |_| { (cb.borrow_mut())(); glib::Propagation::Proceed });
    }
    pub fn queue_redraw(&self) { self.0.queue_draw(); }
    pub fn raw_handle(&self) -> *mut std::ffi::c_void { self.0.as_ptr() as *mut _ }
}

#[derive(Clone)]
pub struct BoxWidget(pub gtk4::Box);
impl BoxWidget {
    pub fn new(o: Orientation, s: i32) -> Self {
        ensure_dlopen();
        BoxWidget(gtk4::Box::new(match o { Orientation::Horizontal => gtk4::Orientation::Horizontal, Orientation::Vertical => gtk4::Orientation::Vertical }, s))
    }
    pub fn append(&self, c: &impl AsRef<*mut std::ffi::c_void>) { self.0.append(unsafe { &*(c.as_ref() as *const *mut _ as *const gtk4::Box) }); }
    pub fn set_child_hexpand(&self, c: &impl AsRef<*mut std::ffi::c_void>, e: bool) { self.0.set_hexpand(e); }
    pub fn set_child_vexpand(&self, c: &impl AsRef<*mut std::ffi::c_void>, e: bool) { self.0.set_vexpand(e); }
    pub fn set_hexpand(&self, e: bool) { self.0.set_hexpand(e); }
    pub fn set_vexpand(&self, e: bool) { self.0.set_vexpand(e); }
    pub fn set_visible(&self, v: bool) { self.0.set_visible(v); }
    pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
    pub fn raw_handle(&self) -> *mut std::ffi::c_void { self.0.as_ptr() as *mut _ }
}

#[derive(Clone)]
pub struct Button(pub gtk4::Button);
impl Button {
    pub fn new(label: &str) -> Self { ensure_dlopen(); Button(gtk4::Button::with_label(label)) }
    pub fn on_click<F: FnMut() + 'static>(&self, f: F) -> Result<u64, Error> {
        let f = RefCell::new(f);
        Ok(shid_to_u64(self.0.connect_clicked(move |_| (f.borrow_mut())())))
    }
    pub fn emit_clicked(&self) -> Result<u64, Error> { self.0.activate(); Ok(0) }
    pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
    pub fn set_visible(&self, v: bool) { self.0.set_visible(v); }
    pub fn set_hexpand(&self, e: bool) { self.0.set_hexpand(e); }
    pub fn set_vexpand(&self, e: bool) { self.0.set_vexpand(e); }
    pub fn set_font_style(&self, _w: i32, _i: bool) {}
    pub fn add_class(&self, n: &str) { self.0.add_css_class(n); }
    pub fn remove_class(&self, n: &str) { self.0.remove_css_class(n); }
    pub fn raw_handle(&self) -> *mut std::ffi::c_void { self.0.as_ptr() as *mut _ }
}

#[derive(Clone)]
pub struct Label(pub gtk4::Label);
impl Label {
    pub fn new(text: &str) -> Self { ensure_dlopen(); Label(gtk4::Label::new(Some(text))) }
    pub fn set_text(&self, t: &str) { self.0.set_label(t); }
    pub fn get_text(&self) -> Option<String> { Some(self.0.text().to_string()) }
    pub fn set_markup(&self, m: &str) { self.0.set_markup(m); }
    pub fn set_visible(&self, v: bool) { self.0.set_visible(v); }
    pub fn set_hexpand(&self, e: bool) { self.0.set_hexpand(e); }
    pub fn set_vexpand(&self, e: bool) { self.0.set_vexpand(e); }
    pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
    pub fn set_xalign(&self, x: f32) { self.0.set_xalign(x); }
    pub fn add_class(&self, n: &str) { self.0.add_css_class(n); }
    pub fn remove_class(&self, n: &str) { self.0.remove_css_class(n); }
    pub fn raw_handle(&self) -> *mut std::ffi::c_void { self.0.as_ptr() as *mut _ }
}

#[derive(Clone)]
pub struct Entry(pub gtk4::Entry, CList);
impl Entry {
    pub fn new() -> Self { ensure_dlopen(); Entry(gtk4::Entry::new(), new_controllers()) }
    pub fn set_text(&self, t: &str) { self.0.set_text(t); }
    pub fn get_text(&self) -> Option<String> { Some(self.0.text().to_string()) }
    pub fn grab_focus(&self) { self.0.grab_focus(); }
    pub fn set_hexpand(&self, e: bool) { self.0.set_hexpand(e); }
    pub fn set_vexpand(&self, e: bool) { self.0.set_vexpand(e); }
    pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
    pub fn set_visible(&self, v: bool) { self.0.set_visible(v); }
    pub fn set_width_chars(&self, n: i32) { self.0.set_width_chars(n); }
    pub fn set_halign(&self, a: i32) { self.0.set_halign(if a == 1 { gtk4::Align::Start } else { gtk4::Align::Fill }); }
    pub fn set_valign(&self, a: i32) { self.0.set_valign(if a == 1 { gtk4::Align::Start } else { gtk4::Align::Fill }); }
    pub fn set_margin_start(&self, p: i32) { self.0.set_margin_start(p); }
    pub fn set_margin_top(&self, p: i32) { self.0.set_margin_top(p); }
    pub fn add_class(&self, n: &str) { self.0.add_css_class(n); }
    pub fn remove_class(&self, n: &str) { self.0.remove_css_class(n); }
    pub fn connect_changed<F: FnMut() + 'static>(&self, f: F) -> Result<u64, Error> {
        let f = RefCell::new(f);
        Ok(shid_to_u64(self.0.connect_changed(move |_| (f.borrow_mut())())))
    }
    pub fn connect_activate<F: FnMut(*mut std::ffi::c_void) + 'static>(&self, f: F) -> Result<u64, Error> {
        let f = RefCell::new(f);
        Ok(shid_to_u64(self.0.connect_activate(move |_| (f.borrow_mut())(std::ptr::null_mut()))))
    }
    pub fn connect_focus_in_event<F: FnMut(*mut std::ffi::c_void) -> i32 + 'static>(&self, f: F) -> Result<u64, Error> {
        let f = RefCell::new(f); let c = gtk4::EventControllerFocus::new();
        c.connect_enter(move |_| { (f.borrow_mut())(std::ptr::null_mut()); });
        self.0.add_controller(c.clone()); self.1.borrow_mut().push(Box::new(c)); Ok(0)
    }
    pub fn connect_focus_out_event<F: FnMut(*mut std::ffi::c_void) -> i32 + 'static>(&self, f: F) -> Result<u64, Error> {
        let f = RefCell::new(f); let c = gtk4::EventControllerFocus::new();
        c.connect_leave(move |_| { (f.borrow_mut())(std::ptr::null_mut()); });
        self.0.add_controller(c.clone()); self.1.borrow_mut().push(Box::new(c)); Ok(0)
    }
    pub fn connect_button_press<F: FnMut() + 'static>(&self, f: F) -> Result<u64, Error> {
        let f = RefCell::new(f); let ges = gtk4::GestureClick::new();
        let id = shid_to_u64(ges.connect_pressed(move |_, _, _, _| (f.borrow_mut())()));
        self.0.add_controller(ges.clone()); self.1.borrow_mut().push(Box::new(ges)); Ok(id)
    }
    pub fn on_key_raw(&self, cb: Box<dyn FnMut(u32, u32) -> bool>) {
        let c = RefCell::new(cb); let ctrl = gtk4::EventControllerKey::new();
        ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);
        ctrl.connect_key_pressed(move |_, keyval, _, state| {
            if c.borrow_mut()(key_to_u32(keyval), state.bits()) { glib::Propagation::Stop } else { glib::Propagation::Proceed }
        });
        self.0.add_controller(ctrl.clone()); self.1.borrow_mut().push(Box::new(ctrl));
    }
    pub fn raw_handle(&self) -> *mut std::ffi::c_void { self.0.as_ptr() as *mut _ }
}

#[derive(Clone)]
pub struct Grid(pub gtk4::Grid);
impl Grid {
    pub fn new() -> Self { ensure_dlopen(); Grid(gtk4::Grid::new()) }
    pub fn attach(&self, c: &impl AsRef<*mut std::ffi::c_void>, l: i32, t: i32, w: i32, h: i32) { self.0.attach(unsafe { &*(c.as_ref() as *const *mut _ as *const gtk4::Widget) }, l, t, w, h); }
    pub fn set_visible(&self, v: bool) { self.0.set_visible(v); }
    pub fn set_hexpand(&self, e: bool) { self.0.set_hexpand(e); }
    pub fn set_vexpand(&self, e: bool) { self.0.set_vexpand(e); }
    pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
    pub fn raw_handle(&self) -> *mut std::ffi::c_void { self.0.as_ptr() as *mut _ }
}

type DrawCb = Rc<RefCell<Option<Box<dyn FnMut(&mut dyn DrawContext, i32, i32)>>>>;
#[derive(Clone)]
pub struct Canvas(pub gtk4::DrawingArea, DrawCb, CList);
impl Canvas {
    pub fn new() -> Self {
        ensure_dlopen();
        let area = gtk4::DrawingArea::new();
        let cb: DrawCb = Rc::new(RefCell::new(None));
        let cb2 = cb.clone();
        area.set_draw_func(move |_, ctx, w, h| {
            // Debug: check cairo context status and draw a test pattern
            let status = ctx.status();
            eprintln!("Cairo status before draw: {status:?}");
            ctx.set_source_rgba(1.0, 0.0, 0.0, 1.0);
            ctx.rectangle(0.0, 0.0, 8.0, 8.0);
            let fill_res = ctx.fill();
            eprintln!("Cairo fill result: {fill_res:?}");
            let status2 = ctx.status();
            eprintln!("Cairo status after fill: {status2:?}");
            if let Some(ref mut f) = *cb2.borrow_mut() {
                f(&mut GtkDrawContext(ctx), w, h);
            }
        });
        Canvas(area, cb, new_controllers())
    }
    pub fn set_draw_callback(&self, cb: Box<dyn FnMut(&mut dyn DrawContext, i32, i32)>) { *self.1.borrow_mut() = Some(cb); }
    pub fn queue_redraw(&self) { self.0.queue_draw(); }
    pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
    pub fn set_content_size(&self, _w: i32, _h: i32) {}
    pub fn grab_focus(&self) { self.0.grab_focus(); }
    pub fn set_can_focus(&self, c: bool) { self.0.set_focusable(c); }
    pub fn on_click(&self, cb: Box<dyn FnMut(f64, f64)>) {
        let c = RefCell::new(cb); let ges = gtk4::GestureClick::new();
        ges.connect_pressed(move |_, _, x, y| { c.borrow_mut()(x, y); });
        self.0.add_controller(ges.clone()); self.2.borrow_mut().push(Box::new(ges));
    }
    pub fn on_key(&self, cb: Box<dyn FnMut(u32, u32) -> bool>) { self.on_key_raw(cb); }
    pub fn on_key_raw(&self, cb: Box<dyn FnMut(u32, u32) -> bool>) {
        let c = RefCell::new(cb); let ctrl = gtk4::EventControllerKey::new();
        ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);
        ctrl.connect_key_pressed(move |_, keyval, _, state| {
            if c.borrow_mut()(key_to_u32(keyval), state.bits()) { glib::Propagation::Stop } else { glib::Propagation::Proceed }
        });
        self.0.add_controller(ctrl.clone()); self.2.borrow_mut().push(Box::new(ctrl));
    }
    pub fn force_draw(&self, _window_ptr: *mut std::ffi::c_void, _fallback_w: i32, _fallback_h: i32) {
        self.queue_redraw();
    }
    pub fn raw_handle(&self) -> *mut std::ffi::c_void { self.0.as_ptr() as *mut _ }
}

pub struct GtkDrawContext<'a>(pub &'a cairo::Context);
impl DrawContext for GtkDrawContext<'_> {
    fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, g: f64, b: f64, a: f64) {
        self.0.set_source_rgba(r, g, b, a); self.0.rectangle(x, y, w, h); let _ = self.0.fill();
    }
    fn stroke_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, g: f64, b: f64, a: f64, lw: f64) {
        self.0.set_source_rgba(r, g, b, a); self.0.set_line_width(lw); self.0.rectangle(x, y, w, h); let _ = self.0.stroke();
    }
    fn draw_text(&mut self, x: f64, y: f64, t: &str, f: &str, s: f64, r: f64, g: f64, b: f64, a: f64) {
        self.draw_text_styled(x, y, t, f, s, r, g, b, a, 0, 0);
    }
    fn draw_text_styled(&mut self, x: f64, y: f64, t: &str, _f: &str, s: f64, r: f64, g: f64, b: f64, a: f64, _sl: i32, _w: i32) {
        self.0.set_source_rgba(r, g, b, a); self.0.set_font_size(s); self.0.move_to(x, y); let _ = self.0.show_text(t);
    }
    fn text_extents(&self, t: &str, f: &str, s: f64) -> (f64, f64, f64, f64) { self.text_extents_styled(t, f, s, 0, 0) }
    fn text_extents_styled(&self, t: &str, _f: &str, s: f64, _sl: i32, _w: i32) -> (f64, f64, f64, f64) {
        self.0.set_font_size(s);
        match self.0.text_extents(t) { Ok(e) => (e.x_bearing(), e.y_bearing(), e.width(), e.height()), _ => (0.0, 0.0, 0.0, 0.0) }
    }
    fn clear(&mut self, r: f64, g: f64, b: f64, a: f64) { self.0.set_source_rgba(r, g, b, a); let _ = self.0.paint(); }
    fn save(&mut self) { let _ = self.0.save(); }
    fn restore(&mut self) { let _ = self.0.restore(); }
    fn clip(&mut self, x: f64, y: f64, w: f64, h: f64) { self.0.rectangle(x, y, w, h); let _ = self.0.clip(); }
}

#[derive(Clone)]
pub struct Menu(pub gio::Menu);
impl Menu {
    pub fn new() -> Self { ensure_dlopen(); Menu(gio::Menu::new()) }
    pub fn append(&mut self, l: &str, a: &str) { self.0.append(Some(l), Some(a)); }
    pub fn append_submenu(&mut self, l: &str, s: &Menu) { self.0.append_submenu(Some(l), &s.0); }
}

#[derive(Clone)]
pub struct MenuBar(pub gtk4::PopoverMenuBar);
impl MenuBar {
    pub fn new(model: &gio::Menu) -> Self { ensure_dlopen(); MenuBar(gtk4::PopoverMenuBar::from_model(Some(model))) }
    pub fn activate_submenu_by_mnemonic(&self, _k: u32) -> bool { false }
    pub fn activate_submenu_item_by_mnemonic(&self, _k: u32) -> bool { false }
    pub unsafe fn insert_action_group(&self, name: &str, p: *mut std::ffi::c_void) {
        let group = gio::ActionGroup::from_glib_none(p as *mut _);
        self.0.insert_action_group(name, Some(&group));
    }
    pub fn raw_handle(&self) -> *mut std::ffi::c_void { self.0.as_ptr() as *mut _ }
    pub fn handle_mnemonic_key(&self, _keyval: u32) -> bool { false }
    pub fn handle_menu_key(&self, _keyval: u32, _mod: u32) -> bool { false }
    pub fn menu_active(&self) -> bool { false }
    pub fn menu_close(&self) {}
}

#[derive(Clone)]
pub struct Dialog(pub gtk4::Window, CList);
impl Dialog {
    pub fn new() -> Self { ensure_dlopen(); Dialog(gtk4::Window::new(), new_controllers()) }
    pub fn set_title(&self, t: &str) { self.0.set_title(Some(t)); }
    pub fn set_default_size(&self, w: i32, h: i32) { self.0.set_default_size(w, h); }
    pub fn get_content_area(&self) -> *mut std::ffi::c_void { self.0.as_ptr() as *mut _ }
    pub fn append_content_area(&self, c: &impl AsRef<*mut std::ffi::c_void>) { self.0.set_child(Some(unsafe { &*(c.as_ref() as *const *mut _ as *const gtk4::Widget) })); }
    pub fn add_button(&self, _t: &str, _r: i32) {}
    pub fn present(&self) { self.0.present(); }
    pub fn connect_response<F: FnMut(i32) + 'static>(&self, _f: F) -> Result<u64, Error> { Ok(0) }
    pub fn close(&self) { self.0.close(); }
    pub fn mark_destroyed(&self) {}
    pub fn raw_handle(&self) -> *mut std::ffi::c_void { self.0.as_ptr() as *mut _ }
}

#[derive(Clone)]
pub struct DropDown(pub gtk4::DropDown);
impl DropDown {
    pub fn new(items: &[&str]) -> Self {
        ensure_dlopen();
        let store = gio::ListStore::new::<gtk4::StringObject>();
        for &item in items { store.append(&gtk4::StringObject::new(item)); }
        DropDown(gtk4::DropDown::new(Some(store.clone()), Some(gtk4::PropertyExpression::new(gtk4::StringObject::static_type(), None::<gtk4::PropertyExpression>, "string"))))
    }
    pub fn set_active(&self, idx: Option<u32>) { self.0.set_selected(idx.unwrap_or(u32::MAX)); }
    pub fn get_active(&self) -> i32 { self.0.selected() as i32 }
    pub fn connect_changed<F: FnMut() + 'static>(&self, f: F) -> Result<u64, Error> {
        let f = RefCell::new(f); Ok(shid_to_u64(self.0.connect_selected_item_notify(move |_| (f.borrow_mut())())))
    }
    pub fn set_visible(&self, v: bool) { self.0.set_visible(v); }
    pub fn set_hexpand(&self, e: bool) { self.0.set_hexpand(e); }
    pub fn set_vexpand(&self, e: bool) { self.0.set_vexpand(e); }
    pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
    pub fn raw_handle(&self) -> *mut std::ffi::c_void { self.0.as_ptr() as *mut _ }
}

#[derive(Clone)]
pub struct CheckButton(pub gtk4::CheckButton);
impl CheckButton {
    pub fn new(l: &str) -> Self { ensure_dlopen(); CheckButton(gtk4::CheckButton::with_label(l)) }
    pub fn is_active(&self) -> bool { self.0.is_active() }
    pub fn set_active(&self, a: bool) { self.0.set_active(a); }
    pub fn connect_toggled<F: FnMut() + 'static>(&self, f: F) -> Result<u64, Error> {
        let f = RefCell::new(f); Ok(shid_to_u64(self.0.connect_toggled(move |_| (f.borrow_mut())())))
    }
    pub fn set_visible(&self, v: bool) { self.0.set_visible(v); }
    pub fn set_hexpand(&self, e: bool) { self.0.set_hexpand(e); }
    pub fn set_vexpand(&self, e: bool) { self.0.set_vexpand(e); }
    pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
    pub fn raw_handle(&self) -> *mut std::ffi::c_void { self.0.as_ptr() as *mut _ }
}

#[derive(Clone)]
pub struct RadioButton(pub gtk4::CheckButton);
impl RadioButton {
    pub fn new(_g: Option<&RadioButton>, l: &str) -> Self { ensure_dlopen(); RadioButton(gtk4::CheckButton::with_label(l)) }
    pub fn is_active(&self) -> bool { self.0.is_active() }
    pub fn set_active(&self, a: bool) { self.0.set_active(a); }
    pub fn set_visible(&self, v: bool) { self.0.set_visible(v); }
    pub fn set_hexpand(&self, e: bool) { self.0.set_hexpand(e); }
    pub fn set_vexpand(&self, e: bool) { self.0.set_vexpand(e); }
    pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
    pub fn connect_toggled<F: FnMut() + 'static>(&self, f: F) -> Result<u64, Error> {
        let f = RefCell::new(f); Ok(shid_to_u64(self.0.connect_toggled(move |_| (f.borrow_mut())())))
    }
    pub fn raw_handle(&self) -> *mut std::ffi::c_void { self.0.as_ptr() as *mut _ }
}

#[derive(Clone)]
pub struct TextView(pub gtk4::TextView);
impl TextView {
    pub fn new() -> Self { ensure_dlopen(); TextView(gtk4::TextView::new()) }
    pub fn set_text(&self, t: &str) { self.0.buffer().set_text(t); }
    pub fn get_text(&self) -> Option<String> { Some(self.0.buffer().text(&self.0.buffer().start_iter(), &self.0.buffer().end_iter(), false).to_string()) }
    pub fn set_wrap_mode(&self, _w: i32) { self.0.set_wrap_mode(gtk4::WrapMode::WordChar); }
    pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
    pub fn set_hexpand(&self, e: bool) { self.0.set_hexpand(e); }
    pub fn set_vexpand(&self, e: bool) { self.0.set_vexpand(e); }
    pub fn set_visible(&self, v: bool) { self.0.set_visible(v); }
    pub fn raw_handle(&self) -> *mut std::ffi::c_void { self.0.as_ptr() as *mut _ }
}

#[derive(Clone)]
pub struct ScrolledWindow(pub gtk4::ScrolledWindow);
impl ScrolledWindow {
    pub fn new() -> Self { ensure_dlopen(); ScrolledWindow(gtk4::ScrolledWindow::new()) }
    pub fn set_child(&self, c: &impl AsRef<*mut std::ffi::c_void>) { self.0.set_child(Some(unsafe { &*(c.as_ref() as *const *mut _ as *const gtk4::Widget) })); }
    pub fn set_policy(&self, _h: u32, _v: u32) {}
    pub fn set_hexpand(&self, e: bool) { self.0.set_hexpand(e); }
    pub fn set_vexpand(&self, e: bool) { self.0.set_vexpand(e); }
    pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
    pub fn raw_handle(&self) -> *mut std::ffi::c_void { self.0.as_ptr() as *mut _ }
}

#[derive(Clone)]
pub struct Overlay(pub gtk4::Overlay);
impl Overlay {
    pub fn new() -> Self { ensure_dlopen(); Overlay(gtk4::Overlay::new()) }
    pub fn set_child(&self, c: &impl AsRef<*mut std::ffi::c_void>) { self.0.set_child(Some(unsafe { &*(c.as_ref() as *const *mut _ as *const gtk4::Widget) })); }
    pub fn add_overlay(&self, c: &impl AsRef<*mut std::ffi::c_void>) { self.0.add_overlay(unsafe { &*(c.as_ref() as *const *mut _ as *const gtk4::Widget) }); }
    pub fn set_overlay_pass_through(&self, _c: &impl AsRef<*mut std::ffi::c_void>, _p: bool) {}
    pub fn remove(&self, c: &impl AsRef<*mut std::ffi::c_void>) { self.0.remove_overlay(unsafe { &*(c.as_ref() as *const *mut _ as *const gtk4::Widget) }); }
    pub fn show_all(&self) { self.0.set_visible(true); }
    pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
    pub fn set_hexpand(&self, e: bool) { self.0.set_hexpand(e); }
    pub fn set_vexpand(&self, e: bool) { self.0.set_vexpand(e); }
    pub fn raw_handle(&self) -> *mut std::ffi::c_void { self.0.as_ptr() as *mut _ }
}

#[derive(Clone)]
pub struct Spreadsheet(pub Canvas, pub Overlay);
impl Spreadsheet {
    pub fn new(_r: usize, _c: usize) -> Self { let c = Canvas::new(); let o = Overlay::new(); o.set_child(&c); Spreadsheet(c, o) }
    pub fn set_cell(&self, _r: usize, _c: usize, _t: &str) {}
    pub fn get_cell(&self, _r: usize, _c: usize) -> Option<String> { None }
    pub fn queue_redraw(&self) { self.0.queue_redraw(); }
    pub fn set_draw_callback(&self, cb: Box<dyn FnMut(&mut dyn DrawContext, i32, i32)>) { self.0.set_draw_callback(cb); }
    pub fn on_key(&self, cb: Box<dyn FnMut(u32, u32) -> bool>) { self.0.on_key(cb); }
    pub fn on_click(&self, cb: Box<dyn FnMut(f64, f64)>) { self.0.on_click(cb); }
    pub fn set_hexpand(&self, e: bool) { self.1.set_hexpand(e); }
    pub fn set_vexpand(&self, e: bool) { self.1.set_vexpand(e); }
    pub fn canvas(&self) -> &Canvas { &self.0 }
    pub fn overlay(&self) -> &Overlay { &self.1 }
    pub fn raw_handle(&self) -> *mut std::ffi::c_void { self.1.raw_handle() }
}

macro_rules! impl_asref {
    ($t:ty, $inner:ty) => {
        impl AsRef<*mut std::ffi::c_void> for $t {
            fn as_ref(&self) -> &*mut std::ffi::c_void {
                unsafe { std::mem::transmute::<&$inner, &*mut std::ffi::c_void>(&self.0) }
            }
        }
    };
}
impl_asref!(Window, gtk4::Window);
impl_asref!(BoxWidget, gtk4::Box);
impl_asref!(Label, gtk4::Label);
impl_asref!(Entry, gtk4::Entry);
impl_asref!(Canvas, gtk4::DrawingArea);
impl_asref!(Button, gtk4::Button);
impl_asref!(Dialog, gtk4::Window);
impl_asref!(DropDown, gtk4::DropDown);
impl_asref!(CheckButton, gtk4::CheckButton);
impl_asref!(RadioButton, gtk4::CheckButton);
impl_asref!(TextView, gtk4::TextView);
impl_asref!(Grid, gtk4::Grid);
impl_asref!(ScrolledWindow, gtk4::ScrolledWindow);
impl_asref!(Overlay, gtk4::Overlay);
impl_asref!(MenuBar, gtk4::PopoverMenuBar);
impl AsRef<*mut std::ffi::c_void> for Spreadsheet { fn as_ref(&self) -> &*mut std::ffi::c_void { self.1.as_ref() } }

pub fn create_window() -> Result<Window, Error> { Ok(Window::new()) }
pub fn create_button(l: &str) -> Result<Button, Error> { Ok(Button::new(l)) }
pub fn create_label(t: &str) -> Result<Label, Error> { Ok(Label::new(t)) }
pub fn create_box(o: Orientation, s: i32) -> Result<BoxWidget, Error> { Ok(BoxWidget::new(o, s)) }
pub fn create_grid() -> Result<Grid, Error> { Ok(Grid::new()) }
pub fn create_entry() -> Result<Entry, Error> { Ok(Entry::new()) }
pub fn create_menu() -> Result<Menu, Error> { Ok(Menu::new()) }
pub fn create_simple_action(n: &str) -> Result<SimpleAction, Error> { Ok(SimpleAction(gio::SimpleAction::new(n, None))) }
pub unsafe fn create_menubar(m: &Menu, _a: *mut std::ffi::c_void) -> Result<MenuBar, Error> { Ok(MenuBar::new(&m.0)) }
pub fn create_dialog() -> Result<Dialog, Error> { Ok(Dialog::new()) }
pub fn create_dropdown(i: &[&str]) -> Result<DropDown, Error> { Ok(DropDown::new(i)) }
pub fn create_checkbutton(l: &str) -> Result<CheckButton, Error> { Ok(CheckButton::new(l)) }
pub fn create_radiobutton(g: Option<&RadioButton>, l: &str) -> Result<RadioButton, Error> { Ok(RadioButton::new(g, l)) }
pub fn create_textview() -> Result<TextView, Error> { Ok(TextView::new()) }
pub fn create_canvas() -> Result<Canvas, Error> { Ok(Canvas::new()) }
pub fn create_scrolled_window() -> Result<ScrolledWindow, Error> { Ok(ScrolledWindow::new()) }
pub fn create_overlay() -> Result<Overlay, Error> { Ok(Overlay::new()) }
pub fn create_application() -> Result<Application, Error> { Ok(Application(gtk4::Application::new(Some("org.corro.Corro"), gio::ApplicationFlags::empty()))) }
pub fn quit_main_loop() -> Result<(), Error> { Ok(()) }
pub fn pump_main_context(count: usize) { for _ in 0..count { glib::MainContext::default().iteration(false); } }
pub fn open_file(_t: &str) -> Result<Option<String>, Error> { Ok(None) }
pub fn save_file(_t: &str) -> Result<Option<String>, Error> { Ok(None) }
pub fn create_spreadsheet(r: usize, c: usize) -> Result<Spreadsheet, Error> { Ok(Spreadsheet::new(r, c)) }
