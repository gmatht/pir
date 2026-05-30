// High-level ergonomic wrappers over gtk_compat for the rustxwidgets API
#[cfg(target_os = "linux")]
mod gtk_adapter {
    use std::sync::Arc;
    use crate::core::{App, Error, Widget};
    use gtk_dynamic_loader::{Loader, Window as GWindow, Button as GButton, Label as GLabel};

    /// A thin transparent wrapper around gtk_compat::Window
    #[repr(transparent)]
    pub struct Window(pub GWindow);

    impl Widget for Window {
        fn raw_handle(&self) -> *mut std::os::raw::c_void { *self.0.as_ref() }
    }

    #[repr(transparent)]
    pub struct Button(pub GButton);

    impl Button {
        pub fn on_click(&self, f: impl FnMut() + 'static) -> Result<u64, Error> {
            self.0.connect_clicked(f).map_err(|e| Error::Backend(format!("{}", e)))
        }
    }

    #[repr(transparent)]
    pub struct Label(pub GLabel);

    impl Label {
        pub fn set_text(&self, text: &str) {
            self.0.set_text(text);
        }
    }

    pub fn create_window_from_loader(loader: &Arc<Loader>) -> Result<Window, Error> {
        let gw = crate::backends::gtk::create_window(loader).map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Window(gw))
    }

    pub fn create_button_from_loader(loader: &Arc<Loader>, label: &str) -> Result<Button, Error> {
        let btn = crate::backends::gtk::create_button(loader, label).map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Button(btn))
    }

    pub fn create_label_from_loader(loader: &Arc<Loader>, text: &str) -> Result<Label, Error> {
        let l = crate::backends::gtk::create_label(loader, text).map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Label(l))
    }
}

#[cfg(target_os = "linux")]
pub use gtk_adapter::*;
