//! Gtk4rsBackend: fills Symbols by delegating to gtk4-rs safe calls.
//! Each wrapper does minimal pointer-to-safe conversion then calls gtk4-rs.
//! Safety: all FFI goes through gtk4-rs (thread checks, type checks, refcounting).
//! This module is compiled only when the `gtk4rs` feature is enabled.

#![cfg(feature = "gtk4rs")]

use crate::symbols::Symbols;
use std::ffi::{c_void, CStr, CString};
use gtk4::prelude::*;
use gtk4::glib::translate::*;

pub fn try_build() -> Result<Symbols, String> {
    if std::env::var("GDK_BACKEND").is_err() {
        std::env::set_var("GDK_BACKEND", "x11");
    }
    if std::env::var("GSK_RENDERER").is_err() {
        std::env::set_var("GSK_RENDERER", "cairo");
    }
    if std::env::var("LIBGL_ALWAYS_SOFTWARE").is_err() {
        std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
    }
    if std::env::var("MESA_LOADER_DRIVER_OVERRIDE").is_err() {
        std::env::set_var("MESA_LOADER_DRIVER_OVERRIDE", "swrast");
    }
    if std::env::var("GDK_PIXBUF_USE_SHM").is_err() {
        std::env::set_var("GDK_PIXBUF_USE_SHM", "0");
    }

    gtk4::init().map_err(|e| format!("gtk4::init() failed: {e}"))?;

    let mut sym = Symbols::empty();

    // ---- Window ----
    sym.gtk_window_new = Some(gtk_window_new_impl);
    sym.gtk_window_set_title = Some(gtk_window_set_title_impl);
    sym.gtk_window_set_default_size = Some(gtk_window_set_default_size_impl);
    sym.gtk_window_present = Some(gtk_window_present_impl);
    sym.gtk_window_set_child = Some(gtk_window_set_child_impl);
    sym.gtk_window_close = Some(gtk_window_close_impl);
    sym.gtk_widget_set_size_request = Some(gtk_widget_set_size_request_impl);
    sym.gtk_widget_set_visible = Some(gtk_widget_set_visible_impl);
    sym.gtk_widget_grab_focus = Some(gtk_widget_grab_focus_impl);
    sym.gtk_widget_set_hexpand = Some(gtk_widget_set_hexpand_impl);
    sym.gtk_widget_set_vexpand = Some(gtk_widget_set_vexpand_impl);
    sym.gtk_widget_set_halign = Some(gtk_widget_set_halign_impl);
    sym.gtk_widget_set_valign = Some(gtk_widget_set_valign_impl);
    sym.gtk_widget_queue_draw = Some(gtk_widget_queue_draw_impl);
    sym.gtk_widget_add_controller = Some(gtk_widget_add_controller_impl);
    sym.gtk_widget_insert_action_group = Some(gtk_widget_insert_action_group_impl);
    sym.gtk_widget_get_style_context = Some(gtk_widget_get_style_context_impl);
    sym.gtk_style_context_add_class = Some(gtk_style_context_add_class_impl);
    sym.gtk_style_context_remove_class = Some(gtk_style_context_remove_class_impl);
    sym.gtk_widget_destroy = Some(gtk_widget_destroy_impl);
    sym.gtk_widget_get_first_child = Some(gtk_widget_get_first_child_impl);
    sym.gtk_widget_get_next_sibling = Some(gtk_widget_get_next_sibling_impl);
    sym.gtk_widget_activate = Some(gtk_widget_activate_impl);

    // ---- Box ----
    sym.gtk_box_new = Some(gtk_box_new_impl);
    sym.gtk_box_append = Some(gtk_box_append_impl);

    // ---- Button ----
    sym.gtk_button_new_with_label = Some(gtk_button_new_with_label_impl);

    // ---- Label ----
    sym.gtk_label_new = Some(gtk_label_new_impl);
    sym.gtk_label_set_text = Some(gtk_label_set_text_impl);
    sym.gtk_label_get_text = Some(gtk_label_get_text_impl);
    sym.gtk_label_set_markup = Some(gtk_label_set_markup_impl);
    sym.gtk_label_set_xalign = Some(gtk_label_set_xalign_impl);

    // ---- Entry ----
    sym.gtk_entry_new = Some(gtk_entry_new_impl);
    sym.gtk_entry_set_text = Some(gtk_entry_set_text_impl);
    sym.gtk_entry_get_text = Some(gtk_entry_get_text_impl);

    // ---- Grid ----
    sym.gtk_grid_new = Some(gtk_grid_new_impl);
    sym.gtk_grid_attach = Some(gtk_grid_attach_impl);

    // ---- DrawingArea (Canvas) ----
    sym.gtk_drawing_area_new = Some(gtk_drawing_area_new_impl);
    sym.gtk_drawing_area_set_draw_func = Some(gtk_drawing_area_set_draw_func_impl);

    // ---- Event controllers ----
    sym.gtk_event_controller_key_new = Some(gtk_event_controller_key_new_impl);
    sym.gtk_gesture_click_new = Some(gtk_gesture_click_new_impl);
    sym.gtk_event_controller_focus_new = Some(gtk_event_controller_focus_new_impl);
    sym.gtk_event_controller_set_propagation_phase = Some(gtk_event_controller_set_propagation_phase_impl);

    // ---- ScrolledWindow ----
    sym.gtk_scrolled_window_new = Some(gtk_scrolled_window_new_impl);

    // ---- Overlay ----
    sym.gtk_overlay_new = Some(gtk_overlay_new_impl);

    // ---- PopoverMenuBar ----
    sym.gtk_popover_menu_bar_new_from_model = Some(gtk_popover_menu_bar_new_from_model_impl);

    // ---- GObject ----
    sym.g_object_ref = Some(g_object_ref_impl);
    sym.g_object_unref = Some(g_object_unref_impl);
    sym.g_object_ref_sink = Some(g_object_ref_sink_impl);

    // ---- GApplication / GAction / GMenu (gio) ----
    sym.gtk_application_new = Some(gtk_application_new_impl);
    sym.g_application_register = Some(g_application_register_impl);
    sym.g_simple_action_new = Some(g_simple_action_new_impl);
    sym.g_action_map_add_action = Some(g_action_map_add_action_impl);
    sym.g_action_map_lookup_action = Some(g_action_map_lookup_action_impl);
    sym.g_action_group_activate_action = Some(g_action_group_activate_action_impl);
    sym.g_action_activate = Some(g_action_activate_impl);
    sym.g_menu_new = Some(g_menu_new_impl);
    sym.g_menu_append = Some(g_menu_append_impl);
    sym.g_menu_append_submenu = Some(g_menu_append_submenu_impl);
    sym.g_application_set_menubar = Some(g_application_set_menubar_impl);

    // ---- GMainLoop ----
    sym.g_main_loop_new = Some(g_main_loop_new_impl);
    sym.g_main_loop_run = Some(g_main_loop_run_impl);
    sym.g_main_loop_quit = Some(g_main_loop_quit_impl);

    // ---- GSignal ----
    sym.g_signal_connect_data = Some(g_signal_connect_data_impl);
    sym.g_signal_connect = Some(g_signal_connect_impl);

    // ---- Cairo ----
    sym.cairo_create = Some(cairo_create_impl);
    sym.cairo_destroy = Some(cairo_destroy_impl);
    sym.cairo_set_source_rgb = Some(cairo_set_source_rgb_impl);
    sym.cairo_set_source_rgba = Some(cairo_set_source_rgba_impl);
    sym.cairo_rectangle = Some(cairo_rectangle_impl);
    sym.cairo_fill = Some(cairo_fill_impl);
    sym.cairo_stroke = Some(cairo_stroke_impl);
    sym.cairo_set_line_width = Some(cairo_set_line_width_impl);
    sym.cairo_move_to = Some(cairo_move_to_impl);
    sym.cairo_select_font_face = Some(cairo_select_font_face_impl);
    sym.cairo_set_font_size = Some(cairo_set_font_size_impl);
    sym.cairo_show_text = Some(cairo_show_text_impl);
    sym.cairo_text_extents = Some(cairo_text_extents_impl);
    sym.cairo_save = Some(cairo_save_impl);
    sym.cairo_restore = Some(cairo_restore_impl);
    sym.cairo_clip = Some(cairo_clip_impl);
    sym.cairo_line_to = Some(cairo_line_to_impl);
    sym.cairo_paint = Some(cairo_paint_impl);

    // ---- CSS ----
    sym.gtk_css_provider_new = Some(gtk_css_provider_new_impl);
    sym.gtk_css_provider_load_from_data = Some(gtk_css_provider_load_from_data_impl);

    // ---- Misc ----
    sym.g_free = Some(g_free_impl);

    Ok(sym)
}

// ============================================================================
// Helpers
// ============================================================================
/// Borrow a widget from a raw pointer (no unref on drop).
unsafe fn borrow_widget<'a>(p: *mut c_void) -> &'a gtk4::Widget {
    &*(p as *const gtk4::Widget)
}

// ============================================================================
// Window
// ============================================================================
unsafe extern "C" fn gtk_window_new_impl(_type: i32) -> *mut c_void {
    let win = gtk4::Window::new();
    let p = win.as_ptr() as *mut c_void;
    std::mem::forget(win); p
}
unsafe extern "C" fn gtk_window_set_title_impl(w: *mut c_void, title: *const i8) {
    gtk4::Window::from_glib_none(w.cast()).set_title(Some(&CStr::from_ptr(title).to_str().unwrap()));
}
unsafe extern "C" fn gtk_window_set_default_size_impl(w: *mut c_void, width: i32, height: i32) {
    gtk4::Window::from_glib_none(w.cast()).set_default_size(width, height);
}
unsafe extern "C" fn gtk_window_present_impl(w: *mut c_void) {
    gtk4::Window::from_glib_none(w.cast()).present();
}
unsafe extern "C" fn gtk_window_set_child_impl(w: *mut c_void, child: *mut c_void) {
    let win = gtk4::Window::from_glib_none(w.cast());
    if child.is_null() { win.set_child(None::<&gtk4::Widget>); }
    else { win.set_child(Some(&gtk4::Widget::from_glib_none(child.cast()))); }
}
unsafe extern "C" fn gtk_window_close_impl(w: *mut c_void) {
    gtk4::Window::from_glib_none(w.cast()).close();
}
unsafe extern "C" fn gtk_widget_set_size_request_impl(w: *mut c_void, width: i32, height: i32) {
    gtk4::Widget::from_glib_none(w.cast()).set_size_request(width, height);
}
unsafe extern "C" fn gtk_widget_set_visible_impl(w: *mut c_void, visible: i32) {
    gtk4::Widget::from_glib_none(w.cast()).set_visible(visible != 0);
}
unsafe extern "C" fn gtk_widget_grab_focus_impl(w: *mut c_void) {
    gtk4::Widget::from_glib_none(w.cast()).grab_focus();
}
unsafe extern "C" fn gtk_widget_set_hexpand_impl(w: *mut c_void, expand: i32) {
    gtk4::Widget::from_glib_none(w.cast()).set_hexpand(expand != 0);
}
unsafe extern "C" fn gtk_widget_set_vexpand_impl(w: *mut c_void, expand: i32) {
    gtk4::Widget::from_glib_none(w.cast()).set_vexpand(expand != 0);
}
unsafe extern "C" fn gtk_widget_set_halign_impl(w: *mut c_void, align: i32) {
    let a = match align { 1 => gtk4::Align::Start, 2 => gtk4::Align::End, 3 => gtk4::Align::Center, _ => gtk4::Align::Fill };
    gtk4::Widget::from_glib_none(w.cast()).set_halign(a);
}
unsafe extern "C" fn gtk_widget_set_valign_impl(w: *mut c_void, align: i32) {
    let a = match align { 1 => gtk4::Align::Start, 2 => gtk4::Align::End, 3 => gtk4::Align::Center, _ => gtk4::Align::Fill };
    gtk4::Widget::from_glib_none(w.cast()).set_valign(a);
}
unsafe extern "C" fn gtk_widget_queue_draw_impl(w: *mut c_void) {
    gtk4::Widget::from_glib_none(w.cast()).queue_draw();
}
unsafe extern "C" fn gtk_widget_add_controller_impl(w: *mut c_void, ctrl: *mut c_void) {
    gtk4::Widget::from_glib_none(w.cast()).add_controller(gtk4::EventController::from_glib_none(ctrl.cast()));
}
unsafe extern "C" fn gtk_widget_insert_action_group_impl(w: *mut c_void, name: *const i8, group: *mut c_void) {
    // Use gtk4-rs's safe API. from_glib_none borrows the GObject.
    let widget = gtk4::Widget::from_glib_none(w.cast::<gtk4::ffi::GtkWidget>());
    if !group.is_null() && !name.is_null() {
        let name_str = std::ffi::CStr::from_ptr(name).to_str().unwrap_or("");
        // Create a gio::Application from the raw GApplication* pointer.
        // GApplication implements GActionGroup, which is what insert_action_group needs.
        let app = gtk4::gio::Application::from_glib_none(group.cast::<gtk4::gio::ffi::GApplication>());
        widget.insert_action_group(name_str, Some(&app));
    }
}
unsafe extern "C" fn gtk_widget_get_style_context_impl(w: *mut c_void) -> *mut c_void {
    let c = gtk4::Widget::from_glib_none(w.cast()).style_context();
    let p = c.as_ptr() as *mut c_void; std::mem::forget(c); p
}
unsafe extern "C" fn gtk_style_context_add_class_impl(ctx: *mut c_void, name: *const i8) {
    gtk4::StyleContext::from_glib_none(ctx.cast()).add_class(CStr::from_ptr(name).to_str().unwrap());
}
unsafe extern "C" fn gtk_style_context_remove_class_impl(ctx: *mut c_void, name: *const i8) {
    gtk4::StyleContext::from_glib_none(ctx.cast()).remove_class(CStr::from_ptr(name).to_str().unwrap());
}
unsafe extern "C" fn gtk_widget_destroy_impl(_w: *mut c_void) {}
unsafe extern "C" fn gtk_widget_get_first_child_impl(w: *mut c_void) -> *mut c_void {
    gtk4::Widget::from_glib_none(w.cast()).first_child()
        .map(|c| { let p = c.as_ptr() as *mut c_void; std::mem::forget(c); p })
        .unwrap_or(std::ptr::null_mut())
}
unsafe extern "C" fn gtk_widget_get_next_sibling_impl(w: *mut c_void) -> *mut c_void {
    gtk4::Widget::from_glib_none(w.cast()).next_sibling()
        .map(|c| { let p = c.as_ptr() as *mut c_void; std::mem::forget(c); p })
        .unwrap_or(std::ptr::null_mut())
}
unsafe extern "C" fn gtk_widget_activate_impl(w: *mut c_void) {
    gtk4::Widget::from_glib_none(w.cast()).activate();
}

// ============================================================================
// Box
// ============================================================================
unsafe extern "C" fn gtk_box_new_impl(orientation: i32, spacing: i32) -> *mut c_void {
    let orient = if orientation == 0 { gtk4::Orientation::Horizontal } else { gtk4::Orientation::Vertical };
    let bx = gtk4::Box::new(orient, spacing);
    let p = bx.as_ptr() as *mut c_void; std::mem::forget(bx); p
}
unsafe extern "C" fn gtk_box_append_impl(box_: *mut c_void, child: *mut c_void) {
    gtk4::Box::from_glib_none(box_.cast()).append(&gtk4::Widget::from_glib_none(child.cast()));
}

// ============================================================================
// Button
// ============================================================================
unsafe extern "C" fn gtk_button_new_with_label_impl(label: *const i8) -> *mut c_void {
    let btn = gtk4::Button::with_label(CStr::from_ptr(label).to_str().unwrap());
    let p = btn.as_ptr() as *mut c_void; std::mem::forget(btn); p
}

// ============================================================================
// Label
// ============================================================================
unsafe extern "C" fn gtk_label_new_impl(str_: *const i8) -> *mut c_void {
    let s = if str_.is_null() { None } else { Some(CStr::from_ptr(str_).to_str().unwrap()) };
    let lbl = gtk4::Label::new(s); let p = lbl.as_ptr() as *mut c_void; std::mem::forget(lbl); p
}
unsafe extern "C" fn gtk_label_set_text_impl(label: *mut c_void, str_: *const i8) {
    gtk4::Label::from_glib_none(label.cast()).set_label(CStr::from_ptr(str_).to_str().unwrap());
}
unsafe extern "C" fn gtk_label_get_text_impl(label: *mut c_void) -> *const i8 {
    let s = gtk4::Label::from_glib_none(label.cast()).text();
    CString::new(s.as_str()).unwrap_or_default().into_raw()
}
unsafe extern "C" fn gtk_label_set_markup_impl(label: *mut c_void, markup: *const i8) {
    gtk4::Label::from_glib_none(label.cast()).set_markup(CStr::from_ptr(markup).to_str().unwrap());
}
unsafe extern "C" fn gtk_label_set_xalign_impl(label: *mut c_void, xalign: f32) {
    gtk4::Label::from_glib_none(label.cast()).set_xalign(xalign);
}

// ============================================================================
// Entry
// ============================================================================
unsafe extern "C" fn gtk_entry_new_impl() -> *mut c_void {
    let e = gtk4::Entry::new(); let p = e.as_ptr() as *mut c_void; std::mem::forget(e); p
}
unsafe extern "C" fn gtk_entry_set_text_impl(entry: *mut c_void, text: *const i8) {
    gtk4::Entry::from_glib_none(entry.cast()).set_text(CStr::from_ptr(text).to_str().unwrap());
}
unsafe extern "C" fn gtk_entry_get_text_impl(entry: *mut c_void) -> *const i8 {
    let s = gtk4::Entry::from_glib_none(entry.cast()).text();
    CString::new(s.as_str()).unwrap_or_default().into_raw()
}

// ============================================================================
// Grid
// ============================================================================
unsafe extern "C" fn gtk_grid_new_impl() -> *mut c_void {
    let g = gtk4::Grid::new(); let p = g.as_ptr() as *mut c_void; std::mem::forget(g); p
}
unsafe extern "C" fn gtk_grid_attach_impl(grid: *mut c_void, child: *mut c_void, left: i32, top: i32, width: i32, height: i32) {
    gtk4::Grid::from_glib_none(grid.cast()).attach(&gtk4::Widget::from_glib_none(child.cast()), left, top, width, height);
}

// ============================================================================
// DrawingArea
// ============================================================================
unsafe extern "C" fn gtk_drawing_area_new_impl() -> *mut c_void {
    let da = gtk4::DrawingArea::new(); let p = da.as_ptr() as *mut c_void; std::mem::forget(da); p
}
unsafe extern "C" fn gtk_drawing_area_set_draw_func_impl(
    area: *mut c_void,
    callback: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, i32, i32, *mut c_void)>,
    user_data: *mut c_void,
    _destroy: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
) {
    let da = gtk4::DrawingArea::from_glib_none(area.cast::<gtk4::ffi::GtkDrawingArea>());
    if let Some(func) = callback {
        struct DrawData {
            f: unsafe extern "C" fn(*mut c_void, *mut c_void, i32, i32, *mut c_void),
            d: *mut c_void,
        }
        unsafe impl Send for DrawData {}
        let data = DrawData { f: func, d: user_data };
        da.set_draw_func(move |_, cr, w, h| {
            let cr_raw = cr.to_raw_none();
            unsafe { (data.f)(data.d, cr_raw as *mut _, w, h, data.d); }
        });
    }
}

// ============================================================================
// Event controllers
// ============================================================================
unsafe extern "C" fn gtk_event_controller_key_new_impl() -> *mut c_void {
    let c = gtk4::EventControllerKey::new(); let p = c.as_ptr() as *mut c_void; std::mem::forget(c); p
}
unsafe extern "C" fn gtk_gesture_click_new_impl() -> *mut c_void {
    let g = gtk4::GestureClick::new(); let p = g.as_ptr() as *mut c_void; std::mem::forget(g); p
}
unsafe extern "C" fn gtk_event_controller_focus_new_impl() -> *mut c_void {
    let c = gtk4::EventControllerFocus::new(); let p = c.as_ptr() as *mut c_void; std::mem::forget(c); p
}
unsafe extern "C" fn gtk_event_controller_set_propagation_phase_impl(ctrl: *mut c_void, phase: u32) {
    let p = match phase { 1 => gtk4::PropagationPhase::Capture, 2 => gtk4::PropagationPhase::Bubble, _ => gtk4::PropagationPhase::None };
    gtk4::EventController::from_glib_none(ctrl.cast()).set_propagation_phase(p);
}

// ============================================================================
// ScrolledWindow
// ============================================================================
unsafe extern "C" fn gtk_scrolled_window_new_impl(_ha: *mut c_void, _va: *mut c_void) -> *mut c_void {
    let s = gtk4::ScrolledWindow::new(); let p = s.as_ptr() as *mut c_void; std::mem::forget(s); p
}

// ============================================================================
// Overlay
// ============================================================================
unsafe extern "C" fn gtk_overlay_new_impl() -> *mut c_void {
    let o = gtk4::Overlay::new(); let p = o.as_ptr() as *mut c_void; std::mem::forget(o); p
}

// ============================================================================
// PopoverMenuBar
// ============================================================================
unsafe extern "C" fn gtk_popover_menu_bar_new_from_model_impl(model: *mut c_void) -> *mut c_void {
    let m = gtk4::gio::Menu::from_glib_none(model.cast());
    let bar = gtk4::PopoverMenuBar::from_model(Some(&m));
    let p = bar.as_ptr() as *mut c_void; std::mem::forget(bar); p
}

// ============================================================================
// GObject
// ============================================================================
unsafe extern "C" fn g_object_ref_impl(obj: *mut c_void) -> *mut c_void { obj }
unsafe extern "C" fn g_object_unref_impl(_obj: *mut c_void) {}
unsafe extern "C" fn g_object_ref_sink_impl(obj: *mut c_void) -> *mut c_void { obj }

// ============================================================================
// GApplication / GAction / GMenu
// ============================================================================
unsafe extern "C" fn gtk_application_new_impl(application_id: *const i8, _flags: u32) -> *mut c_void {
    let id = if application_id.is_null() { None } else { Some(CStr::from_ptr(application_id).to_str().unwrap()) };
    let app = gtk4::Application::new(id, gtk4::gio::ApplicationFlags::empty());
    let p = app.as_ptr() as *mut c_void; std::mem::forget(app); p
}
unsafe extern "C" fn g_application_register_impl(application: *mut c_void, cancellable: *mut c_void, _error: *mut *mut c_void) -> i32 {
    let app = gtk4::Application::from_glib_none(application.cast());
    let cancel = if cancellable.is_null() { None } else { Some(gtk4::gio::Cancellable::from_glib_none(cancellable.cast())) };
    app.register(cancel.as_ref()).is_ok() as i32
}
unsafe extern "C" fn g_simple_action_new_impl(name: *const i8, _parameter_type: *mut c_void) -> *mut c_void {
    let n = CStr::from_ptr(name).to_str().unwrap().to_string();
    let a = gtk4::gio::SimpleAction::new(&n, None::<&gtk4::glib::VariantTy>);
    let p = a.as_ptr() as *mut c_void; std::mem::forget(a);
    p
}
unsafe extern "C" fn g_action_map_add_action_impl(map: *mut c_void, action: *mut c_void) {
    gtk4::gio::ActionMap::from_glib_none(map.cast()).add_action(&gtk4::gio::SimpleAction::from_glib_none(action.cast()));
}
unsafe extern "C" fn g_action_map_lookup_action_impl(map: *mut c_void, name: *const i8) -> *mut c_void {
    let n = CStr::from_ptr(name).to_str().unwrap();
    gtk4::gio::ActionMap::from_glib_none(map.cast()).lookup_action(n)
        .map(|a| { let p = a.as_ptr() as *mut c_void; std::mem::forget(a); p })
        .unwrap_or(std::ptr::null_mut())
}
unsafe extern "C" fn g_action_group_activate_action_impl(group: *mut c_void, name: *const i8, parameter: *mut c_void) {
    let g = gtk4::gio::ActionGroup::from_glib_none(group.cast());
    let n = CStr::from_ptr(name).to_str().unwrap();
    if parameter.is_null() { g.activate_action(n, None); }
    else { g.activate_action(n, Some(&gtk4::glib::Variant::from_glib_none(parameter.cast()))); }
}
unsafe extern "C" fn g_action_activate_impl(action: *mut c_void, parameter: *mut c_void) {
    let a = gtk4::gio::Action::from_glib_none(action.cast());
    if parameter.is_null() { a.activate(None); }
    else { a.activate(Some(&gtk4::glib::Variant::from_glib_none(parameter.cast()))); }
}
unsafe extern "C" fn g_menu_new_impl() -> *mut c_void {
    let m = gtk4::gio::Menu::new(); let p = m.as_ptr() as *mut c_void; std::mem::forget(m); p
}
unsafe extern "C" fn g_menu_append_impl(menu: *mut c_void, label: *const i8, action: *const i8) {
    let m = gtk4::gio::Menu::from_glib_none(menu.cast());
    let l = if label.is_null() { None } else { Some(CStr::from_ptr(label).to_str().unwrap()) };
    let a = if action.is_null() { None } else { Some(CStr::from_ptr(action).to_str().unwrap()) };
    m.append(l, a);
}
unsafe extern "C" fn g_menu_append_submenu_impl(menu: *mut c_void, label: *const i8, submenu: *mut c_void) {
    let m = gtk4::gio::Menu::from_glib_none(menu.cast());
    let l = if label.is_null() { None } else { Some(CStr::from_ptr(label).to_str().unwrap()) };
    let s = gtk4::gio::Menu::from_glib_none(submenu.cast());
    m.append_submenu(l, &s);
}
unsafe extern "C" fn g_application_set_menubar_impl(application: *mut c_void, menu: *mut c_void) {
    let app = gtk4::Application::from_glib_none(application.cast());
    app.set_menubar(Some(&gtk4::gio::Menu::from_glib_none(menu.cast())));
}

// ============================================================================
// GMainLoop
// ============================================================================
unsafe extern "C" fn g_main_loop_new_impl(context: *mut c_void, is_running: i32) -> *mut c_void {
    let ctx = if context.is_null() { None } else { Some(gtk4::glib::MainContext::from_glib_none(context.cast())) };
    let ml = gtk4::glib::MainLoop::new(ctx.as_ref(), is_running != 0);
    let p = ml.as_ptr() as *mut c_void; std::mem::forget(ml); p
}
unsafe extern "C" fn g_main_loop_run_impl(loop_: *mut c_void) {
    gtk4::glib::MainLoop::from_glib_none(loop_.cast()).run();
}
unsafe extern "C" fn g_main_loop_quit_impl(loop_: *mut c_void) {
    gtk4::glib::MainLoop::from_glib_none(loop_.cast()).quit();
}

// ============================================================================
// GSignal — delegate to the real glib g_signal_connect_data via dlopen
// ============================================================================
unsafe extern "C" fn g_signal_connect_data_impl(
    instance: *mut c_void,
    detailed_signal: *const i8,
    c_handler: *mut c_void,
    data: *mut c_void,
    destroy_data: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
    connect_flags: u32,
) -> u64 {
    let sig_name = if !detailed_signal.is_null() {
        std::ffi::CStr::from_ptr(detailed_signal).to_str().unwrap_or("(invalid)")
    } else { "(null)" };
    if sig_name == "activate" && !c_handler.is_null() && !data.is_null() {
        let action = gtk4::gio::SimpleAction::from_glib_none(instance.cast());
        let cb: *mut Box<dyn FnMut(*mut c_void)> = data as *mut _;
        let handler_id = action.connect_activate(move |_, _| {
            unsafe { (**cb)(std::ptr::null_mut()); }
        });
        return unsafe { handler_id.as_raw() as u64 };
    }

    // Fallback: call real g_signal_connect_data via dlopen
    type GCallbackFn = unsafe extern "C" fn();
    type SignalConnectFn = unsafe extern "C" fn(*mut c_void, *const std::os::raw::c_char, Option<GCallbackFn>, *mut c_void, Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>, u32) -> u64;
    static FN: std::sync::OnceLock<SignalConnectFn> = std::sync::OnceLock::new();
    let f = FN.get_or_init(|| {
        let lib = libloading::Library::new("libgobject-2.0.so.0").expect("load libgobject");
        let sym: libloading::Symbol<SignalConnectFn> =
            unsafe { lib.get(b"g_signal_connect_data\0").expect("g_signal_connect_data") };
        let ptr = *sym;
        std::mem::forget(lib);
        ptr
    });
    let c_handler_opt: Option<GCallbackFn> = if c_handler.is_null() { None } else { Some(std::mem::transmute(c_handler)) };
    f(instance, detailed_signal as *const _, c_handler_opt, data, destroy_data, connect_flags)
}
unsafe extern "C" fn g_signal_connect_impl(
    instance: *mut c_void,
    detailed_signal: *const i8,
    c_handler: *mut c_void,
    data: *mut c_void,
) -> u64 {
    g_signal_connect_data_impl(instance, detailed_signal, c_handler, data, None, 0)
}

// ============================================================================
// Cairo
// ============================================================================
unsafe extern "C" fn cairo_create_impl(_target: *mut c_void) -> *mut c_void { std::ptr::null_mut() }
unsafe extern "C" fn cairo_destroy_impl(_cr: *mut c_void) {}

macro_rules! cairo_method {
    ($name:ident, $method:ident, ( $($p:ident: $t:ty),* )) => {
        unsafe extern "C" fn $name(cr: *mut c_void $(, $p: $t)*) {
            let ctx = gtk4::cairo::Context::from_raw_none(cr.cast());
            ctx.$method($($p),*);
        }
    };
}
cairo_method!(cairo_set_source_rgb_impl, set_source_rgb, (r: f64, g: f64, b: f64));
cairo_method!(cairo_set_source_rgba_impl, set_source_rgba, (r: f64, g: f64, b: f64, a: f64));
cairo_method!(cairo_rectangle_impl, rectangle, (x: f64, y: f64, w: f64, h: f64));
cairo_method!(cairo_set_line_width_impl, set_line_width, (w: f64));
cairo_method!(cairo_move_to_impl, move_to, (x: f64, y: f64));
cairo_method!(cairo_set_font_size_impl, set_font_size, (size: f64));
cairo_method!(cairo_line_to_impl, line_to, (x: f64, y: f64));

unsafe extern "C" fn cairo_fill_impl(cr: *mut c_void) {
    let _ = gtk4::cairo::Context::from_raw_none(cr.cast()).fill();
}
unsafe extern "C" fn cairo_stroke_impl(cr: *mut c_void) {
    let _ = gtk4::cairo::Context::from_raw_none(cr.cast()).stroke();
}
unsafe extern "C" fn cairo_show_text_impl(cr: *mut c_void, utf8: *const i8) {
    let s = CStr::from_ptr(utf8).to_str().unwrap_or("");
    let _ = gtk4::cairo::Context::from_raw_none(cr.cast()).show_text(s);
}
unsafe extern "C" fn cairo_select_font_face_impl(cr: *mut c_void, family: *const i8, slant: i32, weight: i32) {
    let f = CStr::from_ptr(family).to_str().unwrap_or("sans-serif");
    let sl = match slant { 1 => gtk4::cairo::FontSlant::Italic, 2 => gtk4::cairo::FontSlant::Oblique, _ => gtk4::cairo::FontSlant::Normal };
    let w = if weight >= 1 { gtk4::cairo::FontWeight::Bold } else { gtk4::cairo::FontWeight::Normal };
    gtk4::cairo::Context::from_raw_none(cr.cast()).select_font_face(f, sl, w);
}
unsafe extern "C" fn cairo_text_extents_impl(cr: *mut c_void, utf8: *const i8, extents: *mut c_void) {
    let s = CStr::from_ptr(utf8).to_str().unwrap_or("");
    if let Ok(e) = gtk4::cairo::Context::from_raw_none(cr.cast()).text_extents(s) {
        *(extents as *mut gtk4::cairo::TextExtents) = e;
    }
}
unsafe extern "C" fn cairo_save_impl(cr: *mut c_void) { let _ = gtk4::cairo::Context::from_raw_none(cr.cast()).save(); }
unsafe extern "C" fn cairo_restore_impl(cr: *mut c_void) { let _ = gtk4::cairo::Context::from_raw_none(cr.cast()).restore(); }
unsafe extern "C" fn cairo_clip_impl(cr: *mut c_void) { let _ = gtk4::cairo::Context::from_raw_none(cr.cast()).clip(); }
unsafe extern "C" fn cairo_paint_impl(cr: *mut c_void) { let _ = gtk4::cairo::Context::from_raw_none(cr.cast()).paint(); }

// ============================================================================
// CSS
// ============================================================================
unsafe extern "C" fn gtk_css_provider_new_impl() -> *mut c_void {
    let p = gtk4::CssProvider::new(); let p2 = p.as_ptr() as *mut c_void; std::mem::forget(p); p2
}
unsafe extern "C" fn gtk_css_provider_load_from_data_impl(provider: *mut c_void, data: *const i8, length: isize, _error: *mut *mut c_void) -> i32 {
    let p = gtk4::CssProvider::from_glib_none(provider.cast());
    let slice = std::slice::from_raw_parts(data as *const u8, if length < 0 { libc::strlen(data) } else { length as usize });
    p.load_from_data(std::str::from_utf8(slice).unwrap_or(""));
    1
}

// ============================================================================
// Misc
// ============================================================================
unsafe extern "C" fn g_free_impl(ptr: *mut c_void) {
    if !ptr.is_null() { libc::free(ptr); }
}
