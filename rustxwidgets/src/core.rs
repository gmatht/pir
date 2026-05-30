use std::sync::Arc;

/// Opaque handler id returned when connecting signals
pub type HandlerId = u64;

/// Top-level error type
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("backend error: {0}")]
    Backend(String),
}

/// Widget trait: minimal escape hatch for raw handles
pub trait Widget {
    /// Return an opaque raw pointer for backend interop. Use unsafe to deref.
    fn raw_handle(&self) -> *mut std::os::raw::c_void;
}

/// Core App wrapper that holds a boxed backend application.
pub struct App {
    inner: Arc<Box<dyn crate::backends::BackendApp>>,
}

impl App {
    /// Initialize the default backend and return an App wrapper.
    /// This calls the platform-specific backend::init() under the hood.
    pub fn init() -> Result<Self, Error> {
        match crate::backends::init() {
            #[cfg(target_os = "linux")]
            Ok(b) => Ok(App { inner: Arc::new(b) }),
            #[cfg(windows)]
            Ok(b) => Ok(App { inner: Arc::new(b) }),
            Err(e) => Err(Error::Backend(format!("{}", e))),
        }
    }

    /// Create a window using the active backend. Returns a backend-specific wrapper.
    #[cfg(target_os = "linux")]
    pub fn create_window(&self) -> Result<crate::backends_gtk_adapter::Window, Error> {
        // delegate to gtk adapter
        crate::backends_gtk_adapter::create_window().map_err(|e| e)
    }

    #[cfg(target_os = "linux")]
    pub fn create_button(&self, label: &str) -> Result<crate::backends_gtk_adapter::Button, Error> {
        crate::backends_gtk_adapter::create_button(label).map_err(|e| e)
    }

    #[cfg(target_os = "linux")]
    pub fn create_label(&self, text: &str) -> Result<crate::backends_gtk_adapter::Label, Error> {
        crate::backends_gtk_adapter::create_label(text).map_err(|e| e)
    }

    #[cfg(target_os = "linux")]
    pub fn create_box(&self, orientation: gtk_dynamic_loader::Orientation, spacing: i32) -> Result<crate::backends_gtk_adapter::BoxWidget, Error> {
        crate::backends_gtk_adapter::create_box(orientation, spacing).map_err(|e| e)
    }

    #[cfg(target_os = "linux")]
    pub fn create_grid(&self) -> Result<crate::backends_gtk_adapter::Grid, Error> {
        crate::backends_gtk_adapter::create_grid().map_err(|e| e)
    }

    #[cfg(target_os = "linux")]
    pub fn create_entry(&self) -> Result<crate::backends_gtk_adapter::Entry, Error> {
        crate::backends_gtk_adapter::create_entry().map_err(|e| e)
    }

    /// Run the backend main loop
    pub fn run(self) -> Result<(), Error> {
        // take ownership if possible
        let boxed = Arc::try_unwrap(self.inner).map_err(|_| Error::Backend("failed to take backend app ownership".into()))?;
        boxed.run().map_err(|e| Error::Backend(format!("{}", e)))
    }
}

impl From<Box<dyn crate::backends::BackendApp>> for App {
    fn from(b: Box<dyn crate::backends::BackendApp>) -> Self {
        App { inner: Arc::new(b) }
    }
}
