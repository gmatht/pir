// High-level ergonomic wrappers over gtk_compat for the rustxwidgets API
#[cfg(target_os = "linux")]
mod gtk_adapter {
    use std::os::raw::c_void;
    use std::cell::RefCell;
    use std::rc::Rc;
    use crate::core::{Error, Widget};
    use gtk_dynamic_loader::{Window as GWindow, Button as GButton, Label as GLabel, BoxWidget as GBox, Grid as GGrid, Entry as GEntry, Dialog as GDialog, DropDown as GDropDown, CheckButton as GCheckButton, RadioButton as GRadioButton, TextView as GTextView};

    /// A thin transparent wrapper around gtk_compat::Window
    #[repr(transparent)]
    pub struct Window(pub GWindow);

    impl Widget for Window {
        fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() }
    }

    impl AsRef<*mut c_void> for Window { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }

    impl Window {
        pub fn set_title(&self, title: &str) {
            self.0.set_title(title);
        }

        pub fn set_child(&self, child: &impl AsRef<*mut c_void>) {
            self.0.set_child(child);
        }

        pub fn present(&self) {
            self.0.present();
        }

        /// # Safety
        /// `group_ptr` must be a valid GActionGroup pointer or null.
        pub unsafe fn insert_action_group(&self, name: &str, group_ptr: *mut std::os::raw::c_void) {
            self.0.insert_action_group(name, group_ptr);
        }

        pub fn set_default_size(&self, width: i32, height: i32) {
            self.0.set_default_size(width, height);
        }
    }

    #[repr(transparent)]
    pub struct Button(pub GButton);

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
    }

    impl Clone for Button { fn clone(&self) -> Self { Button(self.0.clone()) } }

    #[repr(transparent)]
    pub struct Label(pub GLabel);

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
    }

    impl Clone for Label { fn clone(&self) -> Self { Label(self.0.clone()) } }
    impl Widget for Label { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }

    #[repr(transparent)]
    pub struct BoxWidget(pub GBox);

    impl Widget for BoxWidget { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }
    impl AsRef<*mut c_void> for BoxWidget { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }

    impl BoxWidget {
        pub fn append(&self, child: &impl AsRef<*mut c_void>) {
            self.0.append(child);
        }

        pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
        pub fn set_vexpand(&self, expand: bool) { self.0.set_vexpand(expand); }
        pub fn set_hexpand(&self, expand: bool) { self.0.set_hexpand(expand); }
        pub fn set_visible(&self, visible: bool) { self.0.set_visible(visible); }
    }

    #[repr(transparent)]
    pub struct Grid(pub GGrid);
    impl Widget for Grid { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }
    impl AsRef<*mut c_void> for Grid { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }

    impl Grid {
        pub fn attach(&self, child: &impl AsRef<*mut c_void>, left: i32, top: i32, width: i32, height: i32) {
            self.0.attach(child, left, top, width, height);
        }
        pub fn set_visible(&self, visible: bool) { self.0.set_visible(visible); }
        pub fn set_hexpand(&self, expand: bool) { self.0.set_hexpand(expand); }
        pub fn set_vexpand(&self, expand: bool) { self.0.set_vexpand(expand); }
        pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
    }

    #[repr(transparent)]
    pub struct Entry(pub GEntry);
    impl Widget for Entry { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }
    impl AsRef<*mut c_void> for Entry { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }

    impl Entry {
        pub fn set_text(&self, text: &str) { self.0.set_text(text); }
        pub fn get_text(&self) -> Option<String> { self.0.get_text() }
        pub fn set_width_chars(&self, n: i32) { self.0.set_width_chars(n); }
        pub fn set_size_request(&self, w: i32, h: i32) { self.0.set_size_request(w, h); }
        pub fn connect_changed(&self, f: impl FnMut() + 'static) -> Result<u64, Error> { self.0.connect_changed(f).map_err(|e| Error::Backend(format!("{}", e))) }
        pub fn connect_activate<F: FnMut(*mut c_void) + 'static>(&self, f: F) -> Result<u64, Error> {
            self.0.connect_activate(f).map_err(|e| Error::Backend(format!("{}", e)))
        }
        pub fn connect_button_press(&self, f: impl FnMut() + 'static) -> Result<u64, Error> { self.0.connect_button_press(f).map_err(|e| Error::Backend(format!("{}", e))) }
        pub fn add_class(&self, class_name: &str) { self.0.add_class(class_name); }
        pub fn remove_class(&self, class_name: &str) { self.0.remove_class(class_name); }
        pub fn grab_focus(&self) { self.0.grab_focus(); }
        pub fn connect_focus_in_event<F: FnMut(*mut c_void) -> i32 + 'static>(&self, f: F) -> Result<u64, Error> {
            self.0.connect_focus_in_event(f).map_err(|e| Error::Backend(format!("{}", e)))
        }
        pub fn connect_focus_out_event<F: FnMut(*mut c_void) -> i32 + 'static>(&self, f: F) -> Result<u64, Error> {
            self.0.connect_focus_out_event(f).map_err(|e| Error::Backend(format!("{}", e)))
        }
        pub fn set_margin_start(&self, margin: i32) { self.0.set_margin_start(margin); }
        pub fn set_margin_top(&self, margin: i32) { self.0.set_margin_top(margin); }
        pub fn set_halign(&self, align: i32) { self.0.set_halign(align); }
        pub fn set_valign(&self, align: i32) { self.0.set_valign(align); }
        pub fn set_visible(&self, visible: bool) { self.0.set_visible(visible); }
        pub fn set_hexpand(&self, expand: bool) { self.0.set_hexpand(expand); }
        pub fn set_vexpand(&self, expand: bool) { self.0.set_vexpand(expand); }
    }

    impl Clone for Entry { fn clone(&self) -> Self { Entry(self.0.clone()) } }
    impl Clone for DropDown { fn clone(&self) -> Self { DropDown(self.0.clone()) } }
    impl Clone for CheckButton { fn clone(&self) -> Self { CheckButton(self.0.clone()) } }
    impl Clone for RadioButton { fn clone(&self) -> Self { RadioButton(self.0.clone()) } }
    impl Clone for TextView { fn clone(&self) -> Self { TextView(self.0.clone()) } }

    // Factories delegate to backend so they share the App-owned loader
    pub fn create_window() -> Result<Window, Error> {
        let gw = crate::backends::gtk::create_window().map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Window(gw))
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
        Ok(Entry(e))
    }

    // ---- Menu types ----

    #[repr(transparent)]
    pub struct Menu(pub gtk_dynamic_loader::Menu);

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

    impl Widget for MenuBar { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }
    impl AsRef<*mut c_void> for MenuBar { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }

    /// # Safety
    /// `action_group` must be a valid GActionGroup pointer or null.
    pub unsafe fn create_menubar(model: &Menu, action_group: *mut std::os::raw::c_void) -> Result<MenuBar, Error> {
        let b = crate::backends::gtk::create_menubar(&model.0, action_group).map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(MenuBar(b))
    }

    #[repr(transparent)]
    pub struct SimpleAction(pub gtk_dynamic_loader::SimpleAction);

    impl SimpleAction {
        pub fn connect_activate<F: FnMut(*mut c_void) + 'static>(&self, f: F) -> Result<u64, Error> {
            self.0.connect_activate(f).map_err(|e| Error::Backend(format!("{}", e)))
        }
    }

    pub fn create_simple_action(name: &str) -> Result<SimpleAction, Error> {
        let a = crate::backends::gtk::create_simple_action(name).map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(SimpleAction(a))
    }

    // ---- Dialog ----

    #[repr(transparent)]
    pub struct Dialog(pub GDialog);
    impl Widget for Dialog { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }
    impl AsRef<*mut c_void> for Dialog { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }
    impl Clone for Dialog { fn clone(&self) -> Self { Dialog(self.0.clone()) } }

    impl Dialog {
        pub fn set_title(&self, title: &str) { self.0.set_title(title); }
        pub fn set_default_size(&self, w: i32, h: i32) { self.0.set_default_size(w, h); }
        pub fn add_button(&self, text: &str, response_id: i32) { self.0.add_button(text, response_id); }
        pub fn get_content_area(&self) -> *mut c_void { self.0.get_content_area() }
        pub fn append_content_area(&self, child: &impl AsRef<*mut c_void>) { self.0.append_content_area(child); }
        pub fn present(&self) { self.0.present(); }
        pub fn close(&self) { self.0.close(); }
        pub fn connect_response<F: FnMut(i32) + 'static>(&self, f: F) -> Result<u64, Error> {
            self.0.connect_response(f).map_err(|e| Error::Backend(format!("{}", e)))
        }
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
        pub fn set_active(&self, index: u32) { self.0.set_active(index); }
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
            self.cc.set_source_rgba(r, g, b, a);
            self.cc.select_font_face(font, slant, weight);
            self.cc.set_font_size(size);
            self.cc.move_to(x, y);
            self.cc.show_text(text);
        }
        fn text_extents(&self, text: &str, font: &str, size: f64) -> (f64, f64, f64, f64) {
            self.text_extents_styled(text, font, size, 0, 0)
        }
        fn text_extents_styled(&self, text: &str, font: &str, size: f64, slant: i32, weight: i32) -> (f64, f64, f64, f64) {
            self.cc.select_font_face(font, slant, weight);
            self.cc.set_font_size(size);
            let e = self.cc.text_extents(text);
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
    pub struct Canvas {
        pub drawing_area: gtk_dynamic_loader::DrawingArea,
        _controllers: Rc<RefCell<Vec<Box<dyn std::any::Any>>>>,
    }

    impl Clone for Canvas {
        fn clone(&self) -> Self {
            Canvas {
                drawing_area: self.drawing_area.clone(),
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
            let loader = crate::backends::gtk::loader()
                .expect("GTK loader not initialized after Canvas creation");
            let mut cb = cb;
            let symbols = &loader.symbols;
            if symbols.gtk_drawing_area_set_draw_func.is_some() {
                // GTK4 path
                let _ = self.drawing_area.set_draw_func(Box::new(move |cr: *mut c_void, w: i32, h: i32| {
                    let mut ctx = GtkDrawContext::new(cr, &loader);
                    cb(&mut ctx, w, h);
                }));
            } else {
                // GTK3 path
                let _ = self.drawing_area.connect_draw_gtk3(Box::new(move |_widget: *mut c_void, cr: *mut c_void| -> i32 {
                    let mut ctx = GtkDrawContext::new(cr, &loader);
                    cb(&mut ctx, 0, 0);
                    0
                }));
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
            let is_gtk4 = symbols.gtk_gesture_click_new.is_some();
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

        pub fn on_key(&self, cb: Box<dyn FnMut(u32) -> bool>) {
            let loader = crate::backends::gtk::loader()
                .expect("GTK loader not initialized after Canvas creation");
            let symbols = &loader.symbols;
            let inner = *self.drawing_area.as_ref();
            let is_gtk4 = symbols.gtk_gesture_click_new.is_some();
            if is_gtk4 {
                // GTK4: use EventControllerKey — store pointer to keep alive
                if let Ok(ctrl) = gtk_dynamic_loader::EventControllerKey::new(loader.clone()) {
                    let mut cb = cb;
                    let _ = ctrl.connect_key_pressed(Box::new(move |keyval: u32| -> i32 {
                        if cb(keyval) { 1 } else { 0 }
                    }));
                    ctrl.add_to_widget(&self.drawing_area);
                    self._controllers.borrow_mut().push(Box::new(ctrl));
                }
            } else {
                // GTK3: use key-press-event signal (no controller lifetime issue)
                let mut cb = cb;
                let l2 = loader.clone();
                let l3 = l2.clone();
                unsafe {
                    let _ = gtk_dynamic_loader::widget_connect_signal_bool(
                        &l3, inner, "key-press-event",
                        Box::new(move |ev: *mut c_void| -> i32 {
                            let keyval = gtk_dynamic_loader::EventControllerKey::get_keyval_static(&l2, ev);
                            if cb(keyval) { 1 } else { 0 }
                        }),
                    );
                }
            }
        }
    }

    pub fn create_canvas() -> Result<Canvas, Error> {
        let da = crate::backends::gtk::create_drawing_area().map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Canvas {
            drawing_area: da,
            _controllers: Rc::new(RefCell::new(Vec::new())),
        })
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
        pub fn set_hexpand(&self, expand: bool) { self.0.set_hexpand(expand); }
        pub fn set_vexpand(&self, expand: bool) { self.0.set_vexpand(expand); }
    }

    pub fn create_overlay() -> Result<Overlay, Error> {
        let loader = crate::backends::gtk::loader()
            .ok_or_else(|| Error::Backend("GTK loader not initialized".into()))?;
        let o = gtk_dynamic_loader::Overlay::new(loader.clone())
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Overlay(o))
    }

    // ---- File dialogs ----

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

    // ---- Spreadsheet (cross-platform grid widget, rendered via Canvas DrawContext) ----

    pub struct Spreadsheet(pub Canvas, pub Overlay, pub crate::spreadsheet::SharedModel);

    impl Clone for Spreadsheet {
        fn clone(&self) -> Self { Spreadsheet(self.0.clone(), self.1.clone(), self.2.clone()) }
    }
    impl AsRef<*mut c_void> for Spreadsheet { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }
    impl Widget for Spreadsheet { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }

    impl Spreadsheet {
        pub fn id(&self) -> usize { std::rc::Rc::as_ptr(&self.2) as usize }
        pub fn set_cell(&self, row: u32, col: u32, text: &str) {
            self.2.borrow_mut().set_cell(row, col, text);
            self.0.queue_redraw();
        }
        pub fn get_cell(&self, row: u32, col: u32) -> Option<String> { self.2.borrow().get_cell(row, col) }
        pub fn set_raw_cell(&self, row: u32, col: u32, text: &str) {
            self.2.borrow_mut().set_raw_cell(row, col, text);
            self.0.queue_redraw();
        }
        pub fn set_cell_style(&self, row: u32, col: u32, style: u8) {
            self.2.borrow_mut().set_cell_style(row, col, style);
            self.0.queue_redraw();
        }
        pub fn cursor_position(&self) -> Option<(u32, u32)> { self.2.borrow().cursor_position() }
        pub fn set_cursor(&self, row: u32, col: u32) {
            self.2.borrow_mut().set_cursor(row, col);
            self.0.queue_redraw();
        }
        pub fn set_editing(&self, editing: bool, edit_buf: &str, edit_pos: usize) {
            self.2.borrow_mut().set_editing(editing, edit_buf, edit_pos);
            self.0.queue_redraw();
        }
        pub fn set_grid_config(&self, margin_cols: u32, main_cols: u32) {
            self.2.borrow_mut().set_grid_config(margin_cols, main_cols);
            self.0.queue_redraw();
        }
        pub fn set_row_counts(&self, header_rows: u32, main_rows: u32) {
            self.2.borrow_mut().set_row_counts(header_rows, main_rows);
            self.0.queue_redraw();
        }
        pub fn set_column_layout(&self, layout: Vec<(u32, u32, String)>) {
            self.2.borrow_mut().set_column_layout(layout);
            self.0.queue_redraw();
        }
        pub fn set_row_labels(&self, labels: Vec<(u32, String)>) {
            self.2.borrow_mut().set_row_labels(labels);
            self.0.queue_redraw();
        }
        pub fn set_menu_text(&self, text: &str) {
            self.2.borrow_mut().set_menu_text(text);
            self.0.queue_redraw();
        }
        pub fn set_border_title(&self, text: &str) {
            self.2.borrow_mut().set_border_title(text);
            self.0.queue_redraw();
        }
        pub fn set_status_text(&self, text: &str) {
            self.2.borrow_mut().set_status_text(text);
            self.0.queue_redraw();
        }
        pub fn set_formula_bar_trailing(&self, text: &str) {
            self.2.borrow_mut().set_formula_bar_trailing(text);
            self.0.queue_redraw();
        }
        pub fn set_tab_data(&self, titles: &[String], active: usize) {
            self.2.borrow_mut().set_tab_data(titles, active);
            self.0.queue_redraw();
        }
        pub fn set_formula_bar(&self, address_label: &Label, entry: &Entry) {
            let addr = address_label.get_text().unwrap_or_default();
            let txt = entry.get_text().unwrap_or_default();
            self.2.borrow_mut().set_formula_bar(&addr, &txt);
            self.0.queue_redraw();
        }
        pub fn commit_formula_bar(&self) {
            self.2.borrow_mut().commit_formula_bar();
            self.0.queue_redraw();
        }
        pub fn queue_redraw(&self) { self.0.queue_redraw(); }
        pub fn set_draw_callback(&self, cb: Box<dyn FnMut(&mut dyn crate::core::DrawContext, i32, i32)>) {
            self.0.set_draw_callback(cb);
        }
        pub fn on_click(&self, mut cb: Box<dyn FnMut(f64, f64)>) {
            let model = self.2.clone();
            self.0.on_click(Box::new(move |x, y| {
                let (r, c) = match crate::spreadsheet::cell_at(&model.borrow(), x, y) {
                    Some((r, c)) => (r as f64, c as f64),
                    None => (x, y),
                };
                cb(r, c);
            }));
        }
        pub fn on_key(&self, cb: Box<dyn FnMut(u32, u32) -> bool>) {
            let mut cb = cb;
            self.0.on_key(Box::new(move |key: u32| { cb(key, 0u32) }));
        }
        pub fn canvas(&self) -> &Canvas { &self.0 }
        pub fn overlay(&self) -> &Overlay { &self.1 }
    }

    pub fn create_spreadsheet(rows: usize, cols: usize) -> Result<Spreadsheet, Error> {
        let canvas = create_canvas()?;
        let overlay = create_overlay()?;
        let model = crate::spreadsheet::new_shared_model(rows as u32, cols as u32);
        let mdraw = model.clone();
        canvas.set_draw_callback(Box::new(move |dc: &mut dyn crate::core::DrawContext, w: i32, h: i32| {
            crate::spreadsheet::paint(&mdraw.borrow(), dc, w, h);
        }));
        let cw = 150i32; let ch = 28i32; let chw = 46i32;
        let total_w = chw + cols as i32 * cw;
        let total_h = ch + rows as i32 * ch;
        canvas.set_size_request(total_w, total_h);
        canvas.set_content_size(total_w, total_h);
        overlay.set_child(&canvas);
        Ok(Spreadsheet(canvas, overlay, model))
    }

    pub fn add_cursor_move_callback<F: FnMut(u32, u32) + 'static>(f: F) {
        crate::spreadsheet::add_global_cursor_move_callback(Box::new(f));
    }
    pub fn add_commit_edit_callback<F: FnMut(u32, u32, String) + 'static>(f: F) {
        crate::spreadsheet::add_global_commit_edit_callback(Box::new(f));
    }

    pub fn quit_main_loop() -> Result<(), Error> {
        crate::backends::gtk::quit_main_loop().map_err(|e| Error::Backend(format!("{}", e)))
    }

}

#[cfg(target_os = "linux")]
pub use gtk_adapter::*;

#[cfg(target_os = "linux")]
pub use gtk_dynamic_loader::Orientation;
