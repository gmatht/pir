// High-level ergonomic wrappers over gtk_compat for the rustxwidgets API
#[cfg(target_os = "linux")]
mod gtk_adapter {
    use std::os::raw::c_void;
    use crate::core::{Error, Widget};
    use gtk_dynamic_loader::{Window as GWindow, Button as GButton, Label as GLabel, BoxWidget as GBox, Grid as GGrid, Entry as GEntry};

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

        pub fn insert_action_group(&self, name: &str, group_ptr: *mut std::os::raw::c_void) {
            self.0.insert_action_group(name, group_ptr);
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
        /// Set the x alignment of the label's text (0.0 left .. 1.0 right)
        pub fn set_xalign(&self, x: f32) { self.0.set_xalign(x); }
    }

    impl Clone for Label { fn clone(&self) -> Self { Label(self.0.clone()) } }

    #[repr(transparent)]
    pub struct BoxWidget(pub GBox);

    impl Widget for BoxWidget { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }
    impl AsRef<*mut c_void> for BoxWidget { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }

    impl BoxWidget {
        pub fn append(&self, child: &impl AsRef<*mut c_void>) {
            self.0.append(child);
        }
    }

    #[repr(transparent)]
    pub struct Grid(pub GGrid);
    impl Widget for Grid { fn raw_handle(&self) -> *mut c_void { *self.0.as_ref() } }
    impl AsRef<*mut c_void> for Grid { fn as_ref(&self) -> &*mut c_void { self.0.as_ref() } }

    impl Grid {
        pub fn attach(&self, child: &impl AsRef<*mut c_void>, left: i32, top: i32, width: i32, height: i32) {
            self.0.attach(child, left, top, width, height);
        }
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
        pub fn connect_button_press(&self, f: impl FnMut() + 'static) -> Result<u64, Error> { self.0.connect_button_press(f).map_err(|e| Error::Backend(format!("{}", e))) }
        pub fn add_class(&self, class_name: &str) { self.0.add_class(class_name); }
        pub fn remove_class(&self, class_name: &str) { self.0.remove_class(class_name); }
        pub fn grab_focus(&self) { self.0.grab_focus(); }
    }

    impl Clone for Entry { fn clone(&self) -> Self { Entry(self.0.clone()) } }

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

    pub fn create_menubar(model: &Menu, action_group: *mut std::os::raw::c_void) -> Result<MenuBar, Error> {
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

}

#[cfg(target_os = "linux")]
pub use gtk_adapter::*;

#[cfg(target_os = "linux")]
pub use gtk_dynamic_loader::Orientation;
