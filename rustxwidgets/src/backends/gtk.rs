#[cfg(target_os = "linux")]
mod gtk_backend {
    use std::sync::Arc;
    use std::error::Error as StdError;
    use once_cell::sync::OnceCell;
    use gtk_dynamic_loader::Loader;

    static LOADER: OnceCell<Arc<Loader>> = OnceCell::new();
    static MAIN_LOOP: OnceCell<usize> = OnceCell::new();

    pub struct GtkApp {
        loader: Arc<Loader>,
    }

    impl GtkApp {
    pub fn new_with_loader(loader: Arc<Loader>) -> Result<Box<dyn crate::backends::BackendApp>, gtk_dynamic_loader::Error> {
        let _ = LOADER.set(loader.clone());
        Ok(Box::new(GtkApp { loader }))
    }

    pub fn new_default() -> Result<Box<dyn crate::backends::BackendApp>, gtk_dynamic_loader::Error> {
        let loader = Loader::new()?;
            let arc = loader.clone();
            // ensure LOADER is set for factories that read it
            let _ = LOADER.set(arc.clone());
            Self::new_with_loader(arc)
    }
    }

    impl crate::backends::BackendApp for GtkApp {
        fn run(self: Box<Self>) -> Result<(), Box<dyn StdError + Send + Sync>> {
            let symbols = &self.loader.symbols;
            let loop_new = symbols.g_main_loop_new.ok_or("missing g_main_loop_new")?;
            let loop_run = symbols.g_main_loop_run.ok_or("missing g_main_loop_run")?;
            unsafe {
                let loop_ptr = loop_new(std::ptr::null_mut(), 0);
                let _ = MAIN_LOOP.set(loop_ptr as usize);
                loop_run(loop_ptr);
            }
            Ok(())
        }
    }

    pub fn init() -> Result<Box<dyn crate::backends::BackendApp>, Box<dyn StdError + Send + Sync>> {
        match GtkApp::new_default() {
            Ok(b) => Ok(b),
            Err(e) => Err(Box::new(e)),
        }
    }

    // Factories read loader from LOADER cell
    pub fn create_window() -> Result<gtk_dynamic_loader::Window, gtk_dynamic_loader::Error> {
        let loader = LOADER.get().ok_or(gtk_dynamic_loader::Error::Other("loader not initialized".into()))?;
        gtk_dynamic_loader::Window::new(loader.clone())
    }

    pub fn create_button(label: &str) -> Result<gtk_dynamic_loader::Button, gtk_dynamic_loader::Error> {
        let loader = LOADER.get().ok_or(gtk_dynamic_loader::Error::Other("loader not initialized".into()))?;
        gtk_dynamic_loader::Button::with_label(loader.clone(), label)
    }

    pub fn create_label(text: &str) -> Result<gtk_dynamic_loader::Label, gtk_dynamic_loader::Error> {
        let loader = LOADER.get().ok_or(gtk_dynamic_loader::Error::Other("loader not initialized".into()))?;
        gtk_dynamic_loader::Label::new(loader.clone(), text)
    }

    pub fn create_box(orientation: gtk_dynamic_loader::Orientation, spacing: i32) -> Result<gtk_dynamic_loader::BoxWidget, gtk_dynamic_loader::Error> {
        let loader = LOADER.get().ok_or(gtk_dynamic_loader::Error::Other("loader not initialized".into()))?;
        gtk_dynamic_loader::BoxWidget::new(loader.clone(), orientation, spacing)
    }

    pub fn create_grid() -> Result<gtk_dynamic_loader::Grid, gtk_dynamic_loader::Error> {
        let loader = LOADER.get().ok_or(gtk_dynamic_loader::Error::Other("loader not initialized".into()))?;
        gtk_dynamic_loader::Grid::new(loader.clone())
    }

    pub fn create_entry() -> Result<gtk_dynamic_loader::Entry, gtk_dynamic_loader::Error> {
        let loader = LOADER.get().ok_or(gtk_dynamic_loader::Error::Other("loader not initialized".into()))?;
        gtk_dynamic_loader::Entry::new(loader.clone())
    }

    pub fn create_menu() -> Result<gtk_dynamic_loader::Menu, gtk_dynamic_loader::Error> {
        let loader = LOADER.get().ok_or(gtk_dynamic_loader::Error::Other("loader not initialized".into()))?;
        gtk_dynamic_loader::Menu::new(loader.clone())
    }

    pub fn create_simple_action(name: &str) -> Result<gtk_dynamic_loader::SimpleAction, gtk_dynamic_loader::Error> {
        let loader = LOADER.get().ok_or(gtk_dynamic_loader::Error::Other("loader not initialized".into()))?;
        gtk_dynamic_loader::SimpleAction::new(loader.clone(), name)
    }

    /// # Safety
    /// `action_group` must be a valid GActionGroup pointer or null.
    pub unsafe fn create_menubar(model: &gtk_dynamic_loader::Menu, action_group: *mut std::os::raw::c_void) -> Result<gtk_dynamic_loader::MenuBar, gtk_dynamic_loader::Error> {
        let loader = LOADER.get().ok_or(gtk_dynamic_loader::Error::Other("loader not initialized".into()))?;
        gtk_dynamic_loader::MenuBar::new(loader.clone(), model, action_group)
    }

    pub fn create_dialog() -> Result<gtk_dynamic_loader::Dialog, gtk_dynamic_loader::Error> {
        let loader = LOADER.get().ok_or(gtk_dynamic_loader::Error::Other("loader not initialized".into()))?;
        gtk_dynamic_loader::Dialog::new(loader.clone())
    }

    pub fn create_dropdown(items: &[&str]) -> Result<gtk_dynamic_loader::DropDown, gtk_dynamic_loader::Error> {
        let loader = LOADER.get().ok_or(gtk_dynamic_loader::Error::Other("loader not initialized".into()))?;
        gtk_dynamic_loader::DropDown::new(loader.clone(), items)
    }

    pub fn create_checkbutton(label: &str) -> Result<gtk_dynamic_loader::CheckButton, gtk_dynamic_loader::Error> {
        let loader = LOADER.get().ok_or(gtk_dynamic_loader::Error::Other("loader not initialized".into()))?;
        gtk_dynamic_loader::CheckButton::new(loader.clone(), label)
    }

    pub fn create_radiobutton(group: Option<&gtk_dynamic_loader::RadioButton>, label: &str) -> Result<gtk_dynamic_loader::RadioButton, gtk_dynamic_loader::Error> {
        let loader = LOADER.get().ok_or(gtk_dynamic_loader::Error::Other("loader not initialized".into()))?;
        gtk_dynamic_loader::RadioButton::new(loader.clone(), group, label)
    }

    pub fn create_textview() -> Result<gtk_dynamic_loader::TextView, gtk_dynamic_loader::Error> {
        let loader = LOADER.get().ok_or(gtk_dynamic_loader::Error::Other("loader not initialized".into()))?;
        gtk_dynamic_loader::TextView::new(loader.clone())
    }
    pub fn create_scrolled_window() -> Result<gtk_dynamic_loader::ScrolledWindow, gtk_dynamic_loader::Error> {
        let loader = LOADER.get().ok_or(gtk_dynamic_loader::Error::Other("loader not initialized".into()))?;
        gtk_dynamic_loader::ScrolledWindow::new(loader.clone())
    }

    pub fn create_drawing_area() -> Result<gtk_dynamic_loader::DrawingArea, gtk_dynamic_loader::Error> {
        let loader = LOADER.get().ok_or(gtk_dynamic_loader::Error::Other("loader not initialized".into()))?;
        gtk_dynamic_loader::DrawingArea::new(loader.clone())
    }

    pub fn create_overlay() -> Result<gtk_dynamic_loader::Overlay, gtk_dynamic_loader::Error> {
        let loader = LOADER.get().ok_or(gtk_dynamic_loader::Error::Other("loader not initialized".into()))?;
        gtk_dynamic_loader::Overlay::new(loader.clone())
    }

    /// Return the Arc<Loader> if the backend has been initialized.
    pub fn loader() -> Option<Arc<Loader>> {
        LOADER.get().cloned()
    }

    pub fn quit_main_loop() -> Result<(), gtk_dynamic_loader::Error> {
        let loader = LOADER.get().ok_or(gtk_dynamic_loader::Error::Other("loader not initialized".into()))?;
        let loop_ptr = MAIN_LOOP.get().copied().ok_or(gtk_dynamic_loader::Error::Other("main loop not running".into()))?;
        let loop_quit = loader.symbols.g_main_loop_quit.ok_or(gtk_dynamic_loader::Error::MissingSymbol("g_main_loop_quit".into()))?;
        unsafe {
            loop_quit(loop_ptr as *mut std::ffi::c_void);
        }
        Ok(())
    }
}

pub use gtk_backend::{init, create_window, create_button, create_label, create_box, create_grid, create_entry, create_menu, create_simple_action, create_menubar, create_dialog, create_dropdown, create_checkbutton, create_radiobutton, create_textview, create_scrolled_window, create_drawing_area, create_overlay, loader, quit_main_loop};
