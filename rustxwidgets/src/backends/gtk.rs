#[cfg(target_os = "linux")]
mod gtk_backend {
    use std::sync::Arc;
    use std::error::Error as StdError;
    use once_cell::sync::OnceCell;
    use gtk_dynamic_loader::Loader;

    static LOADER: OnceCell<Arc<Loader>> = OnceCell::new();

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

    /// Return the Arc<Loader> if the backend has been initialized.
    pub fn loader() -> Option<Arc<Loader>> {
        LOADER.get().cloned()
    }
}

pub use gtk_backend::{init, create_window, create_button, create_label, create_box, create_grid, create_entry, loader};
