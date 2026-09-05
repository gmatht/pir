// High-level ergonomic wrappers over gtk_compat for the rustxwidgets API
#[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
mod gtk_adapter {
    use std::os::raw::c_void;
    use std::cell::RefCell;
    use std::rc::Rc;
    use crate::core::{Error, Widget};
    use gtk_dynamic_loader::{Window as GWindow, Button as GButton, Label as GLabel, BoxWidget as GBox, Grid as GGrid, Entry as GEntry, Dialog as GDialog, DropDown as GDropDown, CheckButton as GCheckButton, RadioButton as GRadioButton, TextView as GTextView, ScrolledWindow as GScrolledWindow};

    /// Window wrapper around gtk_compat::Window.
    /// Stores event controllers in _controllers so that on_event_key
    /// (which adds a GtkEventControllerKey to the window) keeps the
    /// Rust-side wrapper alive.  Without this the controller is dropped
    /// while still owned by the window, causing a segfault later.
    pub struct Window(pub GWindow, pub Rc<RefCell<Vec<Box<dyn std::any::Any>>>>);

    impl Widget for Window {
        fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() }
    }

    impl AsRef<*mut c_void> for Window { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }

    impl Clone for Window { fn clone(&self) -> Self { Window(self.0.clone(), self.1.clone()) } }

    impl Window {
        pub fn set_title(&self, title: &str) {
            self.0.set_title(title);
        }

        pub fn set_default_size(&self, w: i32, h: i32) {
            self.0.set_default_size(w, h);
        }

        pub fn set_child(&self, child: &impl AsRef<*mut c_void>) {
            self.0.set_child(child);
        }
        pub fn set_child_box(&self, bx: &BoxWidget) { self.set_child(bx); }

        pub fn present(&self) {
            self.0.present();
        }

        /// Queue a redraw of the entire window.  On GTK4 the DrawingArea
        /// may have its own draw function, but forcing a window-level
        /// queue_draw cascades to all children (including the canvas),
        /// ensuring the draw callback fires even when the canvas-level
        /// queue_draw alone doesn't trigger the frame clock.
        pub fn queue_redraw(&self) {
            if let Some(loader) = crate::backends::gtk::loader() {
                let win_ptr = *self.0.as_ref();
                if !win_ptr.is_null() {
                    if let Some(qd) = loader.symbols.gtk_widget_queue_draw {
                        unsafe { qd(win_ptr); }
                    }
                }
            }
        }

        /// # Safety
        /// `group_ptr` must be a valid GActionGroup pointer or null.
        pub unsafe fn insert_action_group(&self, name: &str, group_ptr: *mut std::os::raw::c_void) {
            self.0.insert_action_group(name, group_ptr);
        }

        pub fn hwnd(&self) -> *mut c_void {
            *self.0.as_ref()
        }
        pub fn on_event(&self, cb: Box<dyn FnMut(*mut c_void) -> i32>) {
            if let Some(loader) = crate::backends::gtk::loader() {
                let win_ptr = *self.0.as_ref();
                if !win_ptr.is_null() {
                    let l = loader.clone();
                    unsafe {
                        let _ = gtk_dynamic_loader::widget_connect_signal_bool(
                            &l, win_ptr, "event", cb,
                        );
                    }
                }
            }
        }

        pub fn on_close(&self, cb: Box<dyn FnMut()>) {
            if let Some(loader) = crate::backends::gtk::loader() {
                let win_ptr = *self.0.as_ref();
                if !win_ptr.is_null() {
                    let l = loader.clone();
                    let is_gtk4 = l.symbols.gtk_drawing_area_set_draw_func.is_some();
                    let signal = if is_gtk4 { "close-request" } else { "delete-event" };
                    let mut cb = cb;
                    unsafe {
                        let _ = gtk_dynamic_loader::widget_connect_signal_bool(
                            &l, win_ptr, signal,
                            Box::new(move |_ev: *mut c_void| -> i32 {
                                cb();
                                0
                            }),
                        );
                    }
                }
            }
        }
        pub fn on_event_key(&self, mut cb: Box<dyn FnMut(u32, u32) -> i32>) {
            if let Some(loader) = crate::backends::gtk::loader() {
                let win_ptr = *self.0.as_ref();
                if !win_ptr.is_null() {
                    let l = loader.clone();
                    let is_gtk4 = l.symbols.gtk_drawing_area_set_draw_func.is_some();
                    // Shared callback wrapper used by both GTK4 and GTK3 paths.
                    let shared_cb: Rc<RefCell<Option<Box<dyn FnMut(u32, u32) -> i32>>>> = Rc::new(RefCell::new(Some(cb)));
                    if is_gtk4 {
                        // CAPTURE-phase controller: handles ALL keys (not just navigation keys).
                        // On GTK4/WSLg, when the formula entry doesn't have keyboard focus,
                        // keyboard events are silently dropped because there's no focused
                        // widget to receive them.  By processing ALL keys in the window
                        // CAPTURE phase, we ensure every keystroke reaches our key handler
                        // regardless of focus state.
                        //
                        // We always call the application callback and return its result
                        // (GDK_EVENT_STOP for handled keys, GDK_EVENT_PROPAGATE otherwise).
                        // For printable characters, the application's callback now returns
                        // STOP (1) because start_edit_with/set_text updates the entry widget
                        // directly — there's no need for the event to reach the entry widget.
                        //
                        // Mutual-exclusion flag: the CAPTURE controller sets this before
                        // returning STOP; the BUBBLE controller checks it to avoid
                        // processing the same event twice.  On some GTK4/WSLg versions
                        // the BUBBLE controller fires even when CAPTURE returns STOP,
                        // so a simple GDK_EVENT_STOP return is not sufficient — we need
                        // this explicit handshake.
                        let capture_handled: std::cell::Cell<bool> = std::cell::Cell::new(false);
                        let capture_handled = std::rc::Rc::new(capture_handled);
                        if let Ok(ctrl) = gtk_dynamic_loader::EventControllerKey::new(l.clone()) {
                            ctrl.set_propagation_phase_capture();
                            let sc = shared_cb.clone();
                            let ch = capture_handled.clone();
                            let _ = ctrl.connect_key_pressed(Box::new(move |keyval: u32, state: u32| -> i32 {
                                ch.set(false);
                                let result = if let Some(ref mut f) = *sc.borrow_mut() {
                                    f(keyval, state)
                                } else {
                                    0
                                };
                                if result != 0 {
                                    ch.set(true);
                                }
                                result
                            }));
                            ctrl.add_to_widget(&self.0);
                            self.1.borrow_mut().push(Box::new(ctrl));
                        }
                        // BUBBLE-phase controller: safety net.  The CAPTURE-phase controller
                        // above handles all keys and sets capture_handled=true.  If BUBBLE
                        // fires despite CAPTURE having returned STOP (a known issue on some
                        // GTK4/WSLg versions), we check the flag and skip.
                        if let Ok(ctrl) = gtk_dynamic_loader::EventControllerKey::new(l.clone()) {
                            let sc = shared_cb.clone();
                            let ch = capture_handled.clone();
                            let _ = ctrl.connect_key_pressed(Box::new(move |keyval: u32, state: u32| -> i32 {
                                if ch.get() {
                                    ch.set(false);
                                    return 0;
                                }
                                if let Some(ref mut f) = *sc.borrow_mut() {
                                    f(keyval, state)
                                } else {
                                    0
                                }
                            }));
                            ctrl.add_to_widget(&self.0);
                            self.1.borrow_mut().push(Box::new(ctrl));
                        }
                    } else {
                        unsafe {
                            let sc = shared_cb.clone();
                            let _ = gtk_dynamic_loader::widget_connect_signal_bool(
                                &l.clone(), win_ptr, "event",
                                Box::new(move |ev: *mut c_void| -> i32 {
                                    let mut keyval: u32 = 0;
                                    if let Some(get_kv) = l.symbols.gdk_event_get_keyval {
                                        if get_kv(ev, &mut keyval) == 0 {
                                            return 0;
                                        }
                                    } else {
                                        return 0;
                                    }
                                    let mut state: u32 = 0;
                                    if let Some(get_st) = l.symbols.gdk_event_get_state {
                                        get_st(ev, &mut state);
                                    }
                                    if let Some(ref mut f) = *sc.borrow_mut() {
                                        f(keyval, state)
                                    } else {
                                        0
                                    }
                                }),
                            );
                        }
                    }
                }
            }
        }
    }

    #[repr(transparent)]
    pub struct Button(pub GButton);

    impl Widget for Button { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }
    impl AsRef<*mut c_void> for Button { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }

    impl Button {
        pub fn on_click(&self, f: impl FnMut() + 'static) -> Result<u64, Error> {
            self.0.connect_clicked(f).map_err(|e| Error::Backend(format!("{}", e)))
        }

        pub fn emit_clicked(&self) -> Result<u64, Error> {
            self.0.emit_clicked().map_err(|e| Error::Backend(format!("{}", e)))
        }

        pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
        pub fn set_visible(&self, visible: bool) { self.0.set_visible(visible); }
        pub fn set_hexpand(&self, expand: bool) { self.0.set_hexpand(expand); }
        pub fn set_vexpand(&self, expand: bool) { self.0.set_vexpand(expand); }
        pub fn set_font_style(&self, weight: i32, italic: bool) { self.0.set_font_style(weight, italic); }
        pub fn add_class(&self, class_name: &str) { self.0.add_class(class_name); }
        pub fn remove_class(&self, class_name: &str) { self.0.remove_class(class_name); }
    }

    impl Clone for Button { fn clone(&self) -> Self { Button(self.0.clone()) } }

    #[repr(transparent)]
    pub struct Label(pub GLabel);

    impl Widget for Label { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }
    impl AsRef<*mut c_void> for Label { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }

    impl Label {
        pub fn set_text(&self, text: &str) {
            self.0.set_text(text);
        }

        pub fn get_text(&self) -> Option<String> {
            self.0.get_text()
        }
        pub fn add_class(&self, class_name: &str) { self.0.add_class(class_name); }
        pub fn remove_class(&self, class_name: &str) { self.0.remove_class(class_name); }
        pub fn set_markup(&self, markup: &str) { self.0.set_markup(markup); }
        pub fn set_visible(&self, visible: bool) { self.0.set_visible(visible); }
        pub fn set_hexpand(&self, expand: bool) { self.0.set_hexpand(expand); }
        pub fn set_vexpand(&self, expand: bool) { self.0.set_vexpand(expand); }
        pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
        /// Set the x alignment of the label's text (0.0 left .. 1.0 right)
        pub fn set_xalign(&self, x: f32) { self.0.set_xalign(x); }
        pub fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() }
    }

    impl Clone for Label { fn clone(&self) -> Self { Label(self.0.clone()) } }

    #[repr(transparent)]
    pub struct BoxWidget(pub GBox);

    impl Widget for BoxWidget { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }
    impl AsRef<*mut c_void> for BoxWidget { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }

    impl Clone for BoxWidget { fn clone(&self) -> Self { BoxWidget(self.0.clone()) } }

    impl BoxWidget {
        pub fn append(&self, child: &impl AsRef<*mut c_void>) {
            self.0.append(child);
        }

        pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
        pub fn set_vexpand(&self, expand: bool) { self.0.set_vexpand(expand); }
        pub fn set_hexpand(&self, expand: bool) { self.0.set_hexpand(expand); }
        pub fn set_visible(&self, visible: bool) { self.0.set_visible(visible); }
        pub fn set_child_hexpand(&self, child: &impl AsRef<*mut c_void>, expand: bool) {
            if let Some(loader) = crate::backends::gtk::loader() {
                let child_ptr = *child.as_ref();
                if !child_ptr.is_null() {
                    unsafe { gtk_dynamic_loader::widget_set_hexpand(&loader, child_ptr, expand); }
                }
            }
        }
        pub fn set_child_vexpand(&self, child: &impl AsRef<*mut c_void>, expand: bool) {
            if let Some(loader) = crate::backends::gtk::loader() {
                let child_ptr = *child.as_ref();
                if !child_ptr.is_null() {
                    unsafe { gtk_dynamic_loader::widget_set_vexpand(&loader, child_ptr, expand); }
                }
            }
        }
    }

    #[repr(transparent)]
    pub struct Grid(pub GGrid);
    impl Widget for Grid { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }
    impl AsRef<*mut c_void> for Grid { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }
    impl Clone for Grid { fn clone(&self) -> Self { Grid(self.0.clone()) } }

    impl Grid {
        pub fn attach(&self, child: &impl AsRef<*mut c_void>, left: i32, top: i32, width: i32, height: i32) {
            self.0.attach(child, left, top, width, height);
        }
        pub fn set_visible(&self, visible: bool) { self.0.set_visible(visible); }
        pub fn set_hexpand(&self, expand: bool) { self.0.set_hexpand(expand); }
        pub fn set_vexpand(&self, expand: bool) { self.0.set_vexpand(expand); }
        pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
    }

    pub struct Entry {
        inner: GEntry,
        _controllers: Rc<RefCell<Vec<Box<dyn std::any::Any>>>>,
    }
    impl Widget for Entry { fn raw_handle(&self) -> *mut c_void { *self.inner.as_ref() } }
    impl AsRef<*mut c_void> for Entry { fn as_ref(&self) -> &*mut c_void { self.inner.as_ref() } }

    impl Entry {
        pub fn set_text(&self, text: &str) { self.inner.set_text(text); }
        pub fn get_text(&self) -> Option<String> { self.inner.get_text() }
        pub fn set_width_chars(&self, n: i32) { self.inner.set_width_chars(n); }
        pub fn set_size_request(&self, w: i32, h: i32) { self.inner.set_size_request(w, h); }
        pub fn connect_changed(&self, f: impl FnMut() + 'static) -> Result<u64, Error> { self.inner.connect_changed(f).map_err(|e| Error::Backend(format!("{}", e))) }
        pub fn connect_activate<F: FnMut(*mut c_void) + 'static>(&self, f: F) -> Result<u64, Error> {
            self.inner.connect_activate(f).map_err(|e| Error::Backend(format!("{}", e)))
        }
        pub fn connect_button_press(&self, f: impl FnMut() + 'static) -> Result<u64, Error> { self.inner.connect_button_press(f).map_err(|e| Error::Backend(format!("{}", e))) }
        pub fn add_class(&self, class_name: &str) { self.inner.add_class(class_name); }
        pub fn remove_class(&self, class_name: &str) { self.inner.remove_class(class_name); }
        pub fn grab_focus(&self) { self.inner.grab_focus(); }
        pub fn connect_focus_in_event<F: FnMut(*mut c_void) -> i32 + 'static>(&self, f: F) -> Result<u64, Error> {
            self.inner.connect_focus_in_event(f).map_err(|e| Error::Backend(format!("{}", e)))
        }
        pub fn connect_focus_out_event<F: FnMut(*mut c_void) -> i32 + 'static>(&self, f: F) -> Result<u64, Error> {
            self.inner.connect_focus_out_event(f).map_err(|e| Error::Backend(format!("{}", e)))
        }
        pub fn set_margin_start(&self, margin: i32) { self.inner.set_margin_start(margin); }
        pub fn set_margin_top(&self, margin: i32) { self.inner.set_margin_top(margin); }
        pub fn set_halign(&self, align: i32) { self.inner.set_halign(align); }
        pub fn set_valign(&self, align: i32) { self.inner.set_valign(align); }
        pub fn set_visible(&self, visible: bool) { self.inner.set_visible(visible); }
        pub fn set_hexpand(&self, expand: bool) { self.inner.set_hexpand(expand); }
        pub fn set_vexpand(&self, expand: bool) { self.inner.set_vexpand(expand); }
        pub fn on_key_raw(&self, cb: Box<dyn FnMut(u32, u32) -> bool>) {
            if let Some(loader) = crate::backends::gtk::loader() {
                let entry_ptr = *self.inner.as_ref();
                if !entry_ptr.is_null() {
                    let symbols = &loader.symbols;
                    let is_gtk4 = symbols.gtk_drawing_area_set_draw_func.is_some();
                    if is_gtk4 {
                        // GTK4: GtkEntry's internal EventControllerKey (CAPTURE phase) consumes
                        // RETURN/TAB/ESCAPE/arrows before a BUBBLE-phase EventControllerKey can
                        // fire.  We use CAPTURE phase here so our controller fires BEFORE the
                        // internal handler, intercepting RETURN directly instead of relying on
                        // the "activate" signal (which doesn't fire reliably on WSLg/WSL).
                        //
                        // For printable characters our callback returns GDK_EVENT_PROPAGATE (false),
                        // letting the internal handler insert the character normally.
                        // The "changed" signal then fires on_formula_entry_changed as before.
                        let shared_cb = std::rc::Rc::new(std::cell::RefCell::new(Some(cb)));
                        self._controllers.borrow_mut().push(Box::new(shared_cb.clone()));
                        if let Ok(ctrl) = gtk_dynamic_loader::EventControllerKey::new(loader.clone()) {
                            ctrl.set_propagation_phase_capture();
                            let sc = shared_cb.clone();
                            let _ = ctrl.connect_key_pressed(Box::new(move |keyval: u32, state: u32| -> i32 {
                                if let Some(ref mut f) = *sc.borrow_mut() {
                                    if f(keyval, state) { 1 } else { 0 }
                                } else { 0 }
                            }));
                            ctrl.add_to_widget(&self.inner);
                            self._controllers.borrow_mut().push(Box::new(ctrl));
                        }
                        // NOTE: The application-level connect_activate callback in gui_backend.rs
                        // is retained as a secondary fallback.  With CAPTURE phase we handle RETURN
                        // directly and stop propagation, so "activate" is never emitted — but if
                        // the CAPTURE controller somehow doesn't fire (e.g., older GTK), the
                        // connect_activate path still works.
                    } else {
                        // GTK3 path: connect to raw "key-press-event" signal
                        let l = loader.clone();
                        let mut cb = cb;
                        unsafe {
                            let _ = gtk_dynamic_loader::widget_connect_signal_bool(
                                &l.clone(), entry_ptr, "key-press-event",
                                Box::new(move |ev: *mut c_void| -> i32 {
                                    let keyval = gtk_dynamic_loader::EventControllerKey::get_keyval_static(&l, ev);
                                    if keyval == 0 { return 0; }
                                    let state = gtk_dynamic_loader::EventControllerKey::get_state_static(&l, ev);
                                    if cb(keyval, state) { 1 } else { 0 }
                                }),
                            );
                        }
                    }
                }
            }
        }
    }

    impl Clone for Entry { fn clone(&self) -> Self { Entry { inner: self.inner.clone(), _controllers: self._controllers.clone() } } }
    impl Clone for DropDown { fn clone(&self) -> Self { DropDown(self.0.clone()) } }
    impl Clone for CheckButton { fn clone(&self) -> Self { CheckButton(self.0.clone()) } }
    impl Clone for RadioButton { fn clone(&self) -> Self { RadioButton(self.0.clone()) } }
    impl Clone for TextView { fn clone(&self) -> Self { TextView(self.0.clone()) } }

    // Factories delegate to backend so they share the App-owned loader
    pub fn create_window() -> Result<Window, Error> {
        let gw = crate::backends::gtk::create_window().map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Window(gw, Rc::new(RefCell::new(Vec::new()))))
    }

    pub fn create_button(label: &str) -> Result<Button, Error> {
        let btn = crate::backends::gtk::create_button(label).map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Button(btn))
    }

    pub fn create_label(text: &str) -> Result<Label, Error> {
        let l = crate::backends::gtk::create_label(text).map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Label(l))
    }

    pub fn create_box(orientation: gtk_dynamic_loader::Orientation, spacing: i32) -> Result<BoxWidget, Error> {
        let b = crate::backends::gtk::create_box(orientation, spacing).map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(BoxWidget(b))
    }

    pub fn create_grid() -> Result<Grid, Error> {
        let g = crate::backends::gtk::create_grid().map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Grid(g))
    }

    pub fn create_entry() -> Result<Entry, Error> {
        let e = crate::backends::gtk::create_entry().map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Entry { inner: e, _controllers: Rc::new(RefCell::new(Vec::new())) })
    }

    // ---- Menu types ----

    #[repr(transparent)]
    pub struct Menu(pub gtk_dynamic_loader::Menu);

    impl Clone for Menu { fn clone(&self) -> Self { Menu(self.0.clone()) } }

    impl Menu {
        pub fn append(&mut self, label: &str, detailed_action: &str) {
            self.0.append(label, detailed_action);
        }

        pub fn append_submenu(&mut self, label: &str, submenu: &Menu) {
            self.0.append_submenu(label, &submenu.0);
        }
    }

    pub fn create_menu() -> Result<Menu, Error> {
        let m = crate::backends::gtk::create_menu().map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Menu(m))
    }

    #[repr(transparent)]
    pub struct MenuBar(pub gtk_dynamic_loader::MenuBar);

    impl Clone for MenuBar { fn clone(&self) -> Self { MenuBar(self.0.clone()) } }
    impl Widget for MenuBar { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }
    impl AsRef<*mut c_void> for MenuBar { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }
    impl MenuBar {
        pub fn activate_submenu_by_mnemonic(&self, keyval: u32) -> bool {
            self.0.activate_submenu_by_mnemonic(keyval)
        }
        pub fn activate_submenu_item_by_mnemonic(&self, keyval: u32) -> bool {
            self.0.activate_submenu_item_by_mnemonic(keyval)
        }
        pub unsafe fn insert_action_group(&self, name: &str, group_ptr: *mut std::os::raw::c_void) {
            self.0.insert_action_group(name, group_ptr);
        }
        pub fn handle_mnemonic_key(&self, keyval: u32) -> bool {
            self.0.handle_mnemonic_key(keyval)
        }
        pub fn handle_menu_key(&self, keyval: u32, modifiers: u32) -> bool {
            self.0.handle_menu_key(keyval, modifiers)
        }
        pub fn menu_active(&self) -> bool {
            self.0.menu_active()
        }
        pub fn menu_close(&self) {
            self.0.menu_close();
        }
    }

    /// # Safety
    /// `action_group` must be a valid GActionGroup pointer or null.
    pub unsafe fn create_menubar(model: &Menu, action_group: *mut std::os::raw::c_void) -> Result<MenuBar, Error> {
        let b = crate::backends::gtk::create_menubar(&model.0, action_group).map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(MenuBar(b))
    }

    #[repr(transparent)]
    pub struct SimpleAction(pub gtk_dynamic_loader::SimpleAction);

    impl Clone for SimpleAction { fn clone(&self) -> Self { SimpleAction(self.0.clone()) } }

    impl SimpleAction {
        pub fn connect_activate<F: FnMut(*mut c_void) + 'static>(&self, f: F) -> Result<u64, Error> {
            self.0.connect_activate(f).map_err(|e| Error::Backend(format!("{}", e)))
        }
    }

    pub fn create_simple_action(name: &str) -> Result<SimpleAction, Error> {
        let a = crate::backends::gtk::create_simple_action(name).map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(SimpleAction(a))
    }

    // ---- Application (action group host for menus) ----

    #[repr(transparent)]
    pub struct Application(pub gtk_dynamic_loader::Application);

    impl Application {
        pub fn register(&self) -> Result<(), Error> {
            self.0.register().map_err(|e| Error::Backend(format!("{}", e)))
        }
        pub fn as_ptr(&self) -> *mut c_void {
            self.0.as_ptr()
        }
        pub fn add_action(&self, action: &SimpleAction) -> Result<(), Error> {
            self.0.add_action(&action.0).map_err(|e| Error::Backend(format!("{}", e)))
        }
    }

    pub fn create_application() -> Result<Application, Error> {
        let loader = crate::backends::gtk::loader()
            .ok_or_else(|| Error::Backend("GTK loader not initialized".into()))?;
        let app = gtk_dynamic_loader::Application::new(loader, Some("org.corro.Corro"))
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Application(app))
    }

    // ---- Dialog ----

    #[repr(transparent)]
    pub struct Dialog(pub GDialog);
    impl Clone for Dialog { fn clone(&self) -> Self { Dialog(self.0.clone()) } }
    impl Widget for Dialog { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }
    impl AsRef<*mut c_void> for Dialog { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }

    impl Dialog {
        pub fn set_title(&self, title: &str) { self.0.set_title(title); }
        pub fn set_default_size(&self, w: i32, h: i32) { self.0.set_default_size(w, h); }
        pub fn add_button(&self, text: &str, response_id: i32) { self.0.add_button(text, response_id); }
        pub fn get_content_area(&self) -> *mut c_void { self.0.get_content_area() }
        pub fn append_content_area(&self, child: &impl AsRef<*mut c_void>) { self.0.append_content_area(child); }
        pub fn present(&self) { self.0.present(); }
        pub fn connect_response<F: FnMut(i32) + 'static>(&self, f: F) -> Result<u64, Error> {
            self.0.connect_response(f).map_err(|e| Error::Backend(format!("{}", e)))
        }
        pub fn close(&self) { self.0.close(); }
        pub fn mark_destroyed(&self) { self.0.mark_destroyed(); }

    }

    pub fn create_dialog() -> Result<Dialog, Error> {
        let d = crate::backends::gtk::create_dialog().map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Dialog(d))
    }

    // ---- DropDown ----

    #[repr(transparent)]
    pub struct DropDown(pub GDropDown);
    impl Widget for DropDown { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }
    impl AsRef<*mut c_void> for DropDown { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }

    impl DropDown {
        pub fn set_active(&self, index: Option<u32>) {
            if let Some(idx) = index { self.0.set_active(idx); }
        }
        pub fn get_active(&self) -> i32 { self.0.get_active() }
        pub fn connect_changed<F: FnMut() + 'static>(&self, f: F) -> Result<u64, Error> {
            self.0.connect_changed(f).map_err(|e| Error::Backend(format!("{}", e)))
        }
        pub fn set_visible(&self, visible: bool) { self.0.set_visible(visible); }
        pub fn set_hexpand(&self, expand: bool) { self.0.set_hexpand(expand); }
        pub fn set_vexpand(&self, expand: bool) { self.0.set_vexpand(expand); }
        pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
    }

    pub fn create_dropdown(items: &[&str]) -> Result<DropDown, Error> {
        let d = crate::backends::gtk::create_dropdown(items).map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(DropDown(d))
    }

    // ---- CheckButton ----

    #[repr(transparent)]
    pub struct CheckButton(pub GCheckButton);
    impl Widget for CheckButton { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }
    impl AsRef<*mut c_void> for CheckButton { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }

    impl CheckButton {
        pub fn is_active(&self) -> bool { self.0.is_active() }
        pub fn set_active(&self, active: bool) { self.0.set_active(active); }
        pub fn connect_toggled<F: FnMut() + 'static>(&self, f: F) -> Result<u64, Error> {
            self.0.connect_toggled(f).map_err(|e| Error::Backend(format!("{}", e)))
        }
        pub fn set_visible(&self, visible: bool) { self.0.set_visible(visible); }
        pub fn set_hexpand(&self, expand: bool) { self.0.set_hexpand(expand); }
        pub fn set_vexpand(&self, expand: bool) { self.0.set_vexpand(expand); }
        pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
    }

    pub fn create_checkbutton(label: &str) -> Result<CheckButton, Error> {
        let c = crate::backends::gtk::create_checkbutton(label).map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(CheckButton(c))
    }

    // ---- RadioButton ----

    #[repr(transparent)]
    pub struct RadioButton(pub GRadioButton);
    impl Widget for RadioButton { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }
    impl AsRef<*mut c_void> for RadioButton { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }

    impl RadioButton {
        pub fn is_active(&self) -> bool { self.0.is_active() }
        pub fn set_active(&self, active: bool) { self.0.set_active(active); }
        pub fn connect_toggled<F: FnMut() + 'static>(&self, f: F) -> Result<u64, Error> {
            self.0.connect_toggled(f).map_err(|e| Error::Backend(format!("{}", e)))
        }
        pub fn set_visible(&self, visible: bool) { self.0.set_visible(visible); }
        pub fn set_hexpand(&self, expand: bool) { self.0.set_hexpand(expand); }
        pub fn set_vexpand(&self, expand: bool) { self.0.set_vexpand(expand); }
        pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
    }

    pub fn create_radiobutton(group: Option<&RadioButton>, label: &str) -> Result<RadioButton, Error> {
        let inner_group = group.map(|g| &g.0);
        let r = crate::backends::gtk::create_radiobutton(inner_group, label).map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(RadioButton(r))
    }

    // ---- TextView ----

    #[repr(transparent)]
    pub struct TextView(pub GTextView);
    impl Widget for TextView { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }
    impl AsRef<*mut c_void> for TextView { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }

    impl TextView {
        pub fn set_text(&self, text: &str) { self.0.set_text(text); }
        pub fn get_text(&self) -> Option<String> { self.0.get_text() }
        pub fn set_wrap_mode(&self, wrap_mode: i32) { self.0.set_wrap_mode(wrap_mode); }
        pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
        pub fn set_hexpand(&self, expand: bool) { self.0.set_hexpand(expand); }
        pub fn set_vexpand(&self, expand: bool) { self.0.set_vexpand(expand); }
        pub fn set_visible(&self, visible: bool) { self.0.set_visible(visible); }
    }

    pub fn create_textview() -> Result<TextView, Error> {
        let t = crate::backends::gtk::create_textview().map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(TextView(t))
    }

    // ---- Canvas (cross-platform drawing surface) ----

    pub struct GtkDrawContext<'a> {
        cc: gtk_dynamic_loader::CairoContext<'a>,
    }

    impl<'a> GtkDrawContext<'a> {
        pub fn new(cr: *mut c_void, loader: &'a std::sync::Arc<gtk_dynamic_loader::Loader>) -> Self {
            GtkDrawContext { cc: gtk_dynamic_loader::CairoContext::new(loader, cr) }
        }
    }

    impl crate::core::DrawContext for GtkDrawContext<'_> {
        fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, g: f64, b: f64, a: f64) {
            self.cc.set_source_rgba(r, g, b, a);
            self.cc.rectangle(x, y, w, h);
            self.cc.fill();
        }
        fn stroke_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, g: f64, b: f64, a: f64, lw: f64) {
            self.cc.set_line_width(lw);
            self.cc.set_source_rgba(r, g, b, a);
            self.cc.rectangle(x, y, w, h);
            self.cc.stroke();
        }
        fn draw_text(&mut self, x: f64, y: f64, text: &str, font: &str, size: f64, r: f64, g: f64, b: f64, a: f64) {
            self.draw_text_styled(x, y, text, font, size, r, g, b, a, 0, 0)
        }
        fn draw_text_styled(&mut self, x: f64, y: f64, text: &str, font: &str, size: f64, r: f64, g: f64, b: f64, a: f64, slant: i32, weight: i32) {
            self.cc.save();
            self.cc.set_source_rgba(r, g, b, a);
            self.cc.select_font_face(font, slant, weight);
            self.cc.set_font_size(size);
            // Cairo's move_to(x, y) treats y as the text BASELINE.
            // The callers pass y as the TEXT TOP (matching GDI semantics).
            // Convert: baseline = top - y_bearing (y_bearing is negative,
            // so this ADDS the ascent to top).
            let e = self.cc.text_extents(text);
            let baseline = y - e.y_bearing;
            self.cc.move_to(x, baseline);
            self.cc.show_text(text);
            self.cc.restore();
        }
        fn text_extents(&self, text: &str, font: &str, size: f64) -> (f64, f64, f64, f64) {
            self.text_extents_styled(text, font, size, 0, 0)
        }
        fn text_extents_styled(&self, text: &str, font: &str, size: f64, slant: i32, weight: i32) -> (f64, f64, f64, f64) {
            self.cc.save();
            self.cc.select_font_face(font, slant, weight);
            self.cc.set_font_size(size);
            let e = self.cc.text_extents(text);
            self.cc.restore();
            (e.x_bearing, e.y_bearing, e.width, e.height)
        }
        fn clear(&mut self, r: f64, g: f64, b: f64, a: f64) {
            self.cc.set_source_rgba(r, g, b, a);
            self.cc.paint();
        }
        fn save(&mut self) { self.cc.save(); }
        fn restore(&mut self) { self.cc.restore(); }
        fn clip(&mut self, x: f64, y: f64, w: f64, h: f64) {
            self.cc.rectangle(x, y, w, h);
            self.cc.clip();
        }
    }

    /// Canvas wraps a DrawingArea into the cross-platform Canvas API.
    /// Stores event controllers in a reference-counted slot so they outlive
    /// the constructor scope (GTK4 controllers are freed if dropped).
    /// Also stores a copy of the draw callback for the `force_draw` fallback
    /// that renders directly to the window surface (bypassing the frame clock).
    pub struct Canvas {
        pub drawing_area: gtk_dynamic_loader::DrawingArea,
        draw_cb: Rc<RefCell<Option<Box<dyn FnMut(&mut dyn crate::core::DrawContext, i32, i32)>>>>,
        _controllers: Rc<RefCell<Vec<Box<dyn std::any::Any>>>>,
    }

    impl Clone for Canvas {
        fn clone(&self) -> Self {
            Canvas {
                drawing_area: self.drawing_area.clone(),
                draw_cb: self.draw_cb.clone(),
                _controllers: self._controllers.clone(),
            }
        }
    }
    impl AsRef<*mut c_void> for Canvas {
        fn as_ref(&self) -> &*mut c_void { self.drawing_area.as_ref() }
    }
    impl Widget for Canvas {
        fn raw_handle(&self) -> *mut c_void { *self.drawing_area.as_ref() }
    }

    impl Canvas {
        pub fn set_draw_callback(&self, cb: Box<dyn FnMut(&mut dyn crate::core::DrawContext, i32, i32)>) {
            // Store the callback for force_draw fallback
            *self.draw_cb.borrow_mut() = Some(cb);

            let loader = crate::backends::gtk::loader()
                .expect("GTK loader not initialized after Canvas creation");
            let symbols = &loader.symbols;
            if symbols.gtk_drawing_area_set_draw_func.is_some() {
                // GTK4 path — use draw_cb so force_draw can also invoke it
                let cb_stored = self.draw_cb.clone();
                let loader_clone = loader.clone();
                let _ = self.drawing_area.set_draw_func(Box::new(move |cr: *mut c_void, w: i32, h: i32| {
                    let mut ctx = GtkDrawContext::new(cr, &loader_clone);
                    if let Some(ref mut cb) = *cb_stored.borrow_mut() {
                        cb(&mut ctx, w, h);
                    }
                }));
                // Request an immediate initial redraw.  On GTK4 the frame clock
                // may not tick immediately (especially with the Cairo renderer
                // or on virtual displays such as WSL), so we explicitly queue a
                // redraw after setting the draw func to kickstart the first frame.
                self.drawing_area.queue_draw();
            } else {
                // GTK3 path — use widget allocation to provide real w/h
                let cb_stored = self.draw_cb.clone();
                let loader_clone = loader.clone();
                let _ = self.drawing_area.connect_draw_gtk3(Box::new(move |widget: *mut c_void, cr: *mut c_void| -> i32 {
                    let w = if let Some(f) = loader_clone.symbols.gtk_widget_get_allocated_width {
                        unsafe { f(widget) }
                    } else { 0 };
                    let h = if let Some(f) = loader_clone.symbols.gtk_widget_get_allocated_height {
                        unsafe { f(widget) }
                    } else { 0 };
                    let mut ctx = GtkDrawContext::new(cr, &loader_clone);
                    if let Some(ref mut cb) = *cb_stored.borrow_mut() {
                        cb(&mut ctx, w, h);
                    }
                    0
                }));
            }
        }

        /// Force an immediate draw of the canvas content directly to the window
        /// surface, bypassing the GTK4 frame clock.  This is a fallback for
        /// virtual displays (WSL, Xvfb) where the frame clock may never tick.
        /// `window_ptr` must be a valid GtkWindow pointer.
        ///
        /// `fallback_w`/`fallback_h` are used when the surface reports zero
        /// dimensions (the X11/Wayland surface hasn't been configured yet
        /// even though GTK widget allocation already reflects the requested
        /// default size from `set_default_size`).
        ///
        /// Two rendering paths are tried:
        /// 1. `gdk_surface_create_cairo_context` (GTK 4.14+) — preferred, creates
        ///    a GdkCairoContext that draws directly to the surface buffer.
        /// 2. `gdk_surface_begin_draw_frame` + `gdk_draw_context_get_cairo_context`
        ///    + `gdk_surface_end_draw_frame` (GTK 4.0-4.14) — deprecated but
        ///    present on older GTK4 (e.g. Ubuntu 24.04 with GTK 4.12).
        ///
        /// After rendering, `gdk_display_sync` is called to flush the display.
        pub fn force_draw(&self, window_ptr: *mut c_void, fallback_w: i32, fallback_h: i32) {
            let loader = match crate::backends::gtk::loader() {
                Some(l) => l,
                None => return,
            };
            let symbols = &loader.symbols;
            let get_surface = match symbols.gtk_native_get_surface {
                Some(f) => f,
                None => return,
            };
            let get_w = match symbols.gdk_surface_get_width {
                Some(f) => f,
                None => return,
            };
            let get_h = match symbols.gdk_surface_get_height {
                Some(f) => f,
                None => return,
            };

            let surface = unsafe { get_surface(window_ptr) };
            if surface.is_null() { return; }
            let mut w = unsafe { get_w(surface) };
            let mut h = unsafe { get_h(surface) };
            if w <= 0 || h <= 0 {
                // Surface not yet configured by display server.  Use caller-provided
                // fallback dimensions so the draw callback still runs and the canvas
                // claims focus despite the absent server-side configuration.
                w = fallback_w;
                h = fallback_h;
                if w <= 0 || h <= 0 { return; }
            }

            // Approach A (GTK 4.14+): gdk_surface_create_cairo_context
            if let Some(create_cairo) = symbols.gdk_surface_create_cairo_context {
                let cairo_destroy = match symbols.cairo_destroy {
                    Some(f) => f,
                    None => return,
                };
                let cr = unsafe { create_cairo(surface) };
                if cr.is_null() { return; }
                let mut ctx = GtkDrawContext::new(cr, &loader);
                if let Some(ref mut cb) = *self.draw_cb.borrow_mut() {
                    cb(&mut ctx, w, h);
                }
                unsafe { cairo_destroy(cr); }
            } else {
                // Approach B (GTK 4.0-4.14): begin_draw_frame + end_draw_frame
                let begin_frame = match symbols.gdk_surface_begin_draw_frame {
                    Some(f) => f,
                    None => return,
                };
                let get_cr = match symbols.gdk_draw_context_get_cairo_context {
                    Some(f) => f,
                    None => return,
                };
                let end_frame = match symbols.gdk_surface_end_draw_frame {
                    Some(f) => f,
                    None => return,
                };
                let cairo_destroy = match symbols.cairo_destroy {
                    Some(f) => f,
                    None => return,
                };
                let context = unsafe { begin_frame(surface, std::ptr::null_mut()) };
                if context.is_null() { return; }
                let cr = unsafe { get_cr(context) };
                if cr.is_null() {
                    unsafe { end_frame(surface, context); }
                    return;
                }
                let mut ctx = GtkDrawContext::new(cr, &loader);
                if let Some(ref mut cb) = *self.draw_cb.borrow_mut() {
                    cb(&mut ctx, w, h);
                }
                unsafe { cairo_destroy(cr); }
                unsafe { end_frame(surface, context); }
            }

            // Sync the display to ensure the rendered content reaches the
            // display server (X11: XFlush; Wayland: wl_display_flush).
            if let (Some(get_disp), Some(disp_sync)) = (
                symbols.gtk_widget_get_display,
                symbols.gdk_display_sync,
            ) {
                let display = unsafe { get_disp(window_ptr) };
                if !display.is_null() {
                    unsafe { disp_sync(display); }
                }
            }
        }

        pub fn queue_redraw(&self) {
            self.drawing_area.queue_draw();
        }

        pub fn set_size_request(&self, w: i32, h: i32) {
            self.drawing_area.set_size_request(w, h);
        }

        pub fn set_content_size(&self, w: i32, h: i32) {
            self.drawing_area.set_content_width(w);
            self.drawing_area.set_content_height(h);
        }

        pub fn on_click(&self, cb: Box<dyn FnMut(f64, f64)>) {
            let loader = crate::backends::gtk::loader()
                .expect("GTK loader not initialized after Canvas creation");
            let symbols = &loader.symbols;
            let inner = *self.drawing_area.as_ref();
            // gtk_drawing_area_set_draw_func is GTK4-only; gtk_gesture_click_new exists in GTK3 >= 3.24
            let is_gtk4 = symbols.gtk_drawing_area_set_draw_func.is_some();
            if is_gtk4 {
                // GTK4: use GestureClick — store in _controllers to keep alive
                if let Ok(gesture) = gtk_dynamic_loader::GestureClick::new(loader.clone()) {
                    let mut cb = cb;
                    let _ = gesture.connect_pressed(Box::new(move |_n: i32, x: f64, y: f64| {
                        cb(x, y);
                    }));
                    gesture.add_to_widget(&self.drawing_area);
                    self._controllers.borrow_mut().push(Box::new(gesture));
                }
            } else {
                // GTK3: use button-press-event signal (no controller lifetime issue)
                let mask = 1 << 8; // GDK_BUTTON_PRESS_MASK
                unsafe { gtk_dynamic_loader::widget_add_events(&loader, inner, mask); }
                let mut cb = cb;
                let l2 = loader.clone();
                let l3 = l2.clone();
                unsafe {
                    let _ = gtk_dynamic_loader::widget_connect_signal_bool(
                        &l3, inner, "button-press-event",
                        Box::new(move |ev: *mut c_void| -> i32 {
                            if let Some((x, y)) = gtk_dynamic_loader::gdk_event_get_coords(&l2, ev) {
                                cb(x, y);
                                return 1;
                            }
                            0
                        }),
                    );
                }
            }
        }

        pub fn on_key(&self, cb: Box<dyn FnMut(u32, u32) -> bool>) {
            let loader = crate::backends::gtk::loader()
                .expect("GTK loader not initialized after Canvas creation");
            let symbols = &loader.symbols;
            let inner = *self.drawing_area.as_ref();
            // gtk_drawing_area_set_draw_func is GTK4-only; gtk_gesture_click_new exists in GTK3 >= 3.24
            let is_gtk4 = symbols.gtk_drawing_area_set_draw_func.is_some();
            if is_gtk4 {
                if let Ok(ctrl) = gtk_dynamic_loader::EventControllerKey::new(loader.clone()) {
                    let mut cb = cb;
                    let _ = ctrl.connect_key_pressed(Box::new(move |keyval: u32, state: u32| -> i32 {
                        if cb(keyval, state) { 1 } else { 0 }
                    }));
                    ctrl.add_to_widget(&self.drawing_area);
                    self._controllers.borrow_mut().push(Box::new(ctrl));
                }
            } else {
                let mut cb = cb;
                let l2 = loader.clone();
                let l3 = l2.clone();
                unsafe {
                    let _ = gtk_dynamic_loader::widget_connect_signal_bool(
                        &l3, inner, "key-press-event",
                        Box::new(move |ev: *mut c_void| -> i32 {
                            let keyval = gtk_dynamic_loader::EventControllerKey::get_keyval_static(&l2, ev);
                            if keyval == 0 { return 0; }
                            let state = gtk_dynamic_loader::EventControllerKey::get_state_static(&l2, ev);
                            if cb(keyval, state) { 1 } else { 0 }
                        }),
                    );
                }
            }
        }

        pub fn on_key_raw(&self, cb: Box<dyn FnMut(u32, u32) -> bool>) {
            self.on_key(cb);
        }

        pub fn grab_focus(&self) {
            self.drawing_area.grab_focus();
        }

        pub fn set_can_focus(&self, can: bool) {
            self.drawing_area.set_can_focus(can);
        }
    }

    pub fn create_canvas() -> Result<Canvas, Error> {
        let da = crate::backends::gtk::create_drawing_area().map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Canvas {
            drawing_area: da,
            draw_cb: Rc::new(RefCell::new(None)),
            _controllers: Rc::new(RefCell::new(Vec::new())),
        })
    }

    // ---- ScrolledWindow ----

    #[repr(transparent)]
    pub struct ScrolledWindow(pub GScrolledWindow);

    impl Clone for ScrolledWindow { fn clone(&self) -> Self { ScrolledWindow(self.0.clone()) } }

    impl ScrolledWindow {
        pub fn set_child(&self, child: &impl AsRef<*mut c_void>) {
            self.0.set_child(child);
        }
        pub fn set_policy(&self, hscroll: u32, vscroll: u32) {
            self.0.set_policy(hscroll, vscroll);
        }
        pub fn set_hexpand(&self, expand: bool) { self.0.set_hexpand(expand); }
        pub fn set_vexpand(&self, expand: bool) { self.0.set_vexpand(expand); }
        pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
    }

    impl AsRef<*mut c_void> for ScrolledWindow { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }
    impl Widget for ScrolledWindow { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }

    pub fn create_scrolled_window() -> Result<ScrolledWindow, Error> {
        let loader = crate::backends::gtk::loader()
            .ok_or_else(|| Error::Backend("GTK loader not initialized".into()))?;
        let sw = gtk_dynamic_loader::ScrolledWindow::new(loader.clone())
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(ScrolledWindow(sw))
    }

    // ---- Overlay (cross-platform stacking container) ----

    #[repr(transparent)]
    pub struct Overlay(pub gtk_dynamic_loader::Overlay);

    impl Clone for Overlay { fn clone(&self) -> Self { Overlay(self.0.clone()) } }
    impl AsRef<*mut c_void> for Overlay { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }
    impl Widget for Overlay { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }

    impl Overlay {
        pub fn set_child(&self, child: &impl AsRef<*mut c_void>) {
            self.0.set_child(child);
        }
        pub fn add_overlay(&self, child: &impl AsRef<*mut c_void>) {
            self.0.add_overlay(child);
        }
        pub fn set_overlay_pass_through(&self, child: &impl AsRef<*mut c_void>, pass: bool) {
            self.0.set_overlay_pass_through(child, pass);
        }
        pub fn remove(&self, child: &impl AsRef<*mut c_void>) {
            self.0.remove(child);
        }
        pub fn show_all(&self) {
            self.0.show_all();
        }
        pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
        pub fn set_hexpand(&self, expand: bool) { self.0.set_hexpand(expand); }
        pub fn set_vexpand(&self, expand: bool) { self.0.set_vexpand(expand); }
    }

    /// Creates a new GTK Overlay widget.
    pub fn create_overlay() -> Result<Overlay, Error> {
        let loader = crate::backends::gtk::loader()
            .ok_or_else(|| Error::Backend("GTK loader not initialized".into()))?;
        let o = gtk_dynamic_loader::Overlay::new(loader.clone())
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Overlay(o))
    }

    // ---- File dialogs ----

    /// Opens a file dialog and returns the selected file path.
    pub fn open_file(title: &str) -> Result<Option<String>, Error> {
        let loader = crate::backends::gtk::loader()
            .ok_or_else(|| Error::Backend("GTK loader not initialized".into()))?;
        if let Ok(chooser) = unsafe { gtk_dynamic_loader::FileChooserNative::open(loader.clone(), title, std::ptr::null_mut()) } {
            if chooser.run() == -3 {
                return Ok(chooser.get_filename());
            }
        }
        Ok(None)
    }

    /// Opens a file save dialog and returns the selected file path.
    pub fn save_file(title: &str) -> Result<Option<String>, Error> {
        let loader = crate::backends::gtk::loader()
            .ok_or_else(|| Error::Backend("GTK loader not initialized".into()))?;
        if let Ok(chooser) = unsafe { gtk_dynamic_loader::FileChooserNative::save(loader.clone(), title, std::ptr::null_mut()) } {
            if chooser.run() == -3 {
                return Ok(chooser.get_filename());
            }
        }
        Ok(None)
    }

    // ---- Spreadsheet (cross-platform grid widget) ----

    /// A spreadsheet widget that combines a canvas with an overlay for cross-platform grid rendering.
    pub struct Spreadsheet(pub Canvas, pub Overlay);

    impl Clone for Spreadsheet { fn clone(&self) -> Self { Spreadsheet(self.0.clone(), self.1.clone()) } }
    // The overlay is the outer container; as_ref/Widget must return its handle
    // so that adding the spreadsheet to a parent container adds the overlay
    // (which wraps the canvas), not the canvas itself.
    impl AsRef<*mut c_void> for Spreadsheet { fn as_ref(&self) -> &*mut c_void { self.1.as_ref() } }
    impl Widget for Spreadsheet { fn raw_handle(&self) -> *mut c_void { *self.1.as_ref() } }

    impl Spreadsheet {
        /// Sets the text content of a cell. User manages data via callbacks.
        pub fn set_cell(&self, _row: usize, _col: usize, _text: &str) { /* user manages data via callbacks */ }
        /// Gets the text content of a cell. Returns None as user manages data via callbacks.
        pub fn get_cell(&self, _row: usize, _col: usize) -> Option<String> { None }
        /// Queues the canvas for a redraw.
        pub fn queue_redraw(&self) { self.0.queue_redraw(); }

        /// Sets a callback for drawing the spreadsheet content.
        pub fn set_draw_callback(&self, cb: Box<dyn FnMut(&mut dyn crate::core::DrawContext, i32, i32)>) {
            self.0.set_draw_callback(cb);
        }

        /// Sets a callback for handling keyboard input.
        pub fn on_key(&self, cb: Box<dyn FnMut(u32, u32) -> bool>) {
            self.0.on_key(cb);
        }

        /// Sets a callback for handling mouse click events.
        pub fn on_click(&self, cb: Box<dyn FnMut(f64, f64)>) {
            self.0.on_click(cb);
        }

        /// Sets whether the spreadsheet should expand horizontally.
        pub fn set_hexpand(&self, expand: bool) { self.1.set_hexpand(expand); }
        /// Sets whether the spreadsheet should expand vertically.
        pub fn set_vexpand(&self, expand: bool) { self.1.set_vexpand(expand); }

        /// Returns a reference to the underlying canvas widget.
        pub fn canvas(&self) -> &Canvas {
            &self.0
        }

        /// Returns a reference to the overlay widget.
        pub fn overlay(&self) -> &Overlay {
            &self.1
        }
    }

    /// Creates a new spreadsheet widget with the specified number of rows and columns.
    pub fn create_spreadsheet(rows: usize, cols: usize) -> Result<Spreadsheet, Error> {
        let canvas = create_canvas()?;
        let overlay = create_overlay()?;
        let cw = 150i32; let ch = 28i32; let chw = 46i32;
        let total_w = chw + cols as i32 * cw;
        let total_h = ch + rows as i32 * ch;
        canvas.set_size_request(total_w, total_h);
        canvas.set_content_size(total_w, total_h);
        overlay.set_child(&canvas);
        Ok(Spreadsheet(canvas, overlay))
    }

    /// Quits the GTK main event loop.
    pub fn quit_main_loop() -> Result<(), Error> {
        crate::backends::gtk::quit_main_loop().map_err(|e| Error::Backend(format!("{}", e)))
    }

    /// Pump the GTK main context for `count` iterations.
    /// All iterations use blocking waits so poll() returns as soon as
    /// the X11 server or frame clock timer fires.  This ensures frame
    /// clock ticks are actually waited for rather than skipped.
    ///
    /// On a 60fps display each blocking iteration may take up to ~16ms
    /// (the frame clock interval).  500 blocking iterations = ~8s max.
    /// Callers should use a reasonable count (e.g. 500) to cover slow
    /// virtual displays (WSLg, Xvfb) without excessive delay.
    pub fn pump_main_context(count: usize) {
        if let Some(loader) = crate::backends::gtk::loader() {
            if let Some(glib_lib) = loader.libs.get("libglib") {
                type Iteration = unsafe extern "C" fn(*mut std::ffi::c_void, i32) -> i32;
                if let Ok(iter_fn) = unsafe { glib_lib.get::<Iteration>(b"g_main_context_iteration") } {
                    let iter = *iter_fn;
                    unsafe {
                        // All blocking iterations: on virtual displays (WSLg, Xvfb)
                        // the frame clock timer only fires during blocking waits.
                        // Non-blocking iterations return immediately and skip timer
                        // sources, so the draw callback never fires.  500 blocking
                        // iterations = ~8s max wait at 16ms/tick, which covers even
                        // the slowest virtual compositors.
                        for _ in 0..count {
                            iter(std::ptr::null_mut(), 1);
                        }
                    }
                }
            }
        }
    }

    // ---------- TabView (horizontal tabs; button bar + overlaid show/hide panels) ----------
    // GTK auto-lays-out hidden children, so inactive panels (hidden) take no
    // space and the single visible panel fills `content`.
    use std::cell::Cell;

    pub struct TabView {
        pub(crate) outer: BoxWidget,
        pub(crate) tab_bar: BoxWidget,
        pub(crate) content: BoxWidget,
        pub(crate) panels: Rc<RefCell<Vec<BoxWidget>>>,
        pub(crate) buttons: Rc<RefCell<Vec<Button>>>,
        /// Per-tab current index, kept in a cell so click handlers survive
        /// renumbering when a tab is closed.
        pub(crate) indices: Rc<RefCell<Vec<Rc<Cell<usize>>>>>,
        pub(crate) active: Rc<Cell<usize>>,
        pub(crate) tab_changed: Rc<RefCell<Option<Box<dyn FnMut(usize)>>>>,
    }

    impl Clone for TabView {
        fn clone(&self) -> Self {
            TabView {
                outer: self.outer.clone(),
                tab_bar: self.tab_bar.clone(),
                content: self.content.clone(),
                panels: self.panels.clone(),
                buttons: self.buttons.clone(),
                indices: self.indices.clone(),
                active: self.active.clone(),
                tab_changed: self.tab_changed.clone(),
            }
        }
    }

    impl AsRef<*mut c_void> for TabView {
        fn as_ref(&self) -> &*mut c_void { self.outer.as_ref() }
    }

    fn tv_set_active(
        panels: &Rc<RefCell<Vec<BoxWidget>>>,
        active: &Rc<Cell<usize>>,
        idx: usize,
        tab_changed: &Rc<RefCell<Option<Box<dyn FnMut(usize)>>>>,
    ) {
        let count = panels.borrow().len();
        if count == 0 { return; }
        let idx = idx.min(count - 1);
        active.set(idx);
        for (i, p) in panels.borrow().iter().enumerate() {
            p.set_visible(i == idx);
        }
        if let Some(cb) = tab_changed.borrow_mut().as_mut() {
            cb(idx);
        }
    }

    impl TabView {
        pub fn new(_parent: *mut c_void) -> Result<Self, Error> {
            let outer = create_box(gtk_dynamic_loader::Orientation::Vertical, 0)?;
            let tab_bar = create_box(gtk_dynamic_loader::Orientation::Horizontal, 4)?;
            let content = create_box(gtk_dynamic_loader::Orientation::Vertical, 0)?;
            outer.append(&tab_bar);
            outer.append(&content);
            outer.set_child_hexpand(&tab_bar, true);
            outer.set_child_hexpand(&content, true);
            outer.set_child_vexpand(&content, true);
            Ok(TabView {
                outer,
                tab_bar,
                content,
                panels: Rc::new(RefCell::new(Vec::new())),
                buttons: Rc::new(RefCell::new(Vec::new())),
                indices: Rc::new(RefCell::new(Vec::new())),
                active: Rc::new(Cell::new(0)),
                tab_changed: Rc::new(RefCell::new(None)),
            })
        }
        pub fn add_tab(&self, title: &str) -> Result<usize, Error> {
            let idx = self.panels.borrow().len();
            let btn = create_button(title)?;
            self.tab_bar.append(&btn);
            self.tab_bar.set_child_hexpand(&btn, true);
            let panel = create_box(gtk_dynamic_loader::Orientation::Vertical, 0)?;
            panel.set_visible(false);
            let my_index = Rc::new(Cell::new(idx));
            let panels = self.panels.clone();
            let active = self.active.clone();
            let tab_changed = self.tab_changed.clone();
            let _ = btn.on_click({
                let my_index = my_index.clone();
                move || { tv_set_active(&panels, &active, my_index.get(), &tab_changed); }
            });
            self.buttons.borrow_mut().push(btn);
            self.panels.borrow_mut().push(panel.clone());
            self.indices.borrow_mut().push(my_index);
            if idx == 0 {
                tv_set_active(&self.panels, &self.active, 0, &self.tab_changed);
            }
            Ok(idx)
        }
        pub fn tab_box(&self, idx: usize) -> Result<BoxWidget, Error> {
            Ok(self.panels.borrow()
                .get(idx)
                .ok_or_else(|| Error::Backend(format!("tab index {idx} out of range")))?
                .clone())
        }
        pub fn set_on_tab_changed(&self, cb: Box<dyn FnMut(usize)>) {
            *self.tab_changed.borrow_mut() = Some(cb);
        }
        pub fn set_active(&self, idx: usize) {
            tv_set_active(&self.panels, &self.active, idx, &self.tab_changed);
        }
        pub fn current_tab(&self) -> usize { self.active.get() }
        pub fn tab_count(&self) -> usize { self.panels.borrow().len() }
        pub fn tab_title(&self, idx: usize) -> Option<String> {
            self.buttons.borrow().get(idx).and_then(|b| b.get_text())
        }
        pub fn set_tab_title(&self, idx: usize, title: &str) {
            if let Some(b) = self.buttons.borrow().get(idx) { b.set_text(title); }
        }
        pub fn close_tab(&self, idx: usize) {
            let count = self.panels.borrow().len();
            if idx >= count { return; }
            if let Some(b) = self.buttons.borrow().get(idx) { b.set_visible(false); }
            if let Some(p) = self.panels.borrow().get(idx) { p.set_visible(false); }
            self.buttons.borrow_mut().remove(idx);
            self.panels.borrow_mut().remove(idx);
            self.indices.borrow_mut().remove(idx);
            for (i, cell) in self.indices.borrow().iter().enumerate() {
                cell.set(i);
            }
            if self.panels.borrow().is_empty() {
                self.active.set(0);
                return;
            }
            let new_active = if idx >= count - 1 { count - 2 } else { idx };
            self.set_active(new_active);
        }
    }
}

#[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
pub use gtk_adapter::*;

#[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
pub use gtk_dynamic_loader::Orientation;
