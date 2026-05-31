use std::cell::RefCell;
use std::collections::HashMap;
use std::os::raw::c_void;
use std::rc::Rc;
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
    fn raw_handle(&self) -> *mut c_void;
}

/// Core App wrapper that holds a boxed backend application.
pub struct App {
    inner: Arc<Box<dyn crate::backends::BackendApp>>,
    #[cfg(windows)]
    parent_cell: Rc<RefCell<Option<*mut c_void>>>,
    #[cfg(windows)]
    action_registry: Rc<RefCell<HashMap<String, Box<dyn FnMut()>>>>,
}

impl App {
    /// Initialize the default backend and return an App wrapper.
    pub fn init() -> Result<Self, Error> {
        match crate::backends::init() {
            #[cfg(target_os = "linux")]
            Ok(b) => Ok(App { inner: Arc::new(b) }),
            #[cfg(windows)]
            Ok(b) => Ok(App { inner: Arc::new(b), parent_cell: Rc::new(RefCell::new(None)), action_registry: Rc::new(RefCell::new(HashMap::new())) }),
            Err(e) => Err(Error::Backend(format!("{}", e))),
        }
    }

    // -- Linux paths --

    #[cfg(target_os = "linux")]
    pub fn create_window(&self) -> Result<crate::backends_gtk_adapter::Window, Error> {
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

    #[cfg(target_os = "linux")]
    pub fn create_menu(&self) -> Result<crate::backends_gtk_adapter::Menu, Error> {
        crate::backends_gtk_adapter::create_menu().map_err(|e| e)
    }

    #[cfg(target_os = "linux")]
    pub fn create_menubar(&self, model: &crate::backends_gtk_adapter::Menu, action_group: *mut c_void) -> Result<crate::backends_gtk_adapter::MenuBar, Error> {
        crate::backends_gtk_adapter::create_menubar(model, action_group).map_err(|e| e)
    }

    #[cfg(target_os = "linux")]
    pub fn create_simple_action(&self, name: &str) -> Result<crate::backends_gtk_adapter::SimpleAction, Error> {
        crate::backends_gtk_adapter::create_simple_action(name).map_err(|e| e)
    }

    #[cfg(target_os = "linux")]
    pub fn create_dialog(&self) -> Result<crate::backends_gtk_adapter::Dialog, Error> {
        crate::backends_gtk_adapter::create_dialog().map_err(|e| e)
    }

    #[cfg(target_os = "linux")]
    pub fn create_dropdown(&self, items: &[&str]) -> Result<crate::backends_gtk_adapter::DropDown, Error> {
        crate::backends_gtk_adapter::create_dropdown(items).map_err(|e| e)
    }

    #[cfg(target_os = "linux")]
    pub fn create_checkbutton(&self, label: &str) -> Result<crate::backends_gtk_adapter::CheckButton, Error> {
        crate::backends_gtk_adapter::create_checkbutton(label).map_err(|e| e)
    }

    #[cfg(target_os = "linux")]
    pub fn create_radiobutton(&self, label: &str) -> Result<crate::backends_gtk_adapter::RadioButton, Error> {
        crate::backends_gtk_adapter::create_radiobutton(None, label).map_err(|e| e)
    }

    #[cfg(target_os = "linux")]
    pub fn create_textview(&self) -> Result<crate::backends_gtk_adapter::TextView, Error> {
        crate::backends_gtk_adapter::create_textview().map_err(|e| e)
    }

    // -- Windows paths --

    #[cfg(windows)]
    pub fn create_window(&self) -> Result<crate::backends_nwg_adapter::Window, Error> {
        crate::backends_nwg_adapter::create_window(&self.parent_cell)
    }

    #[cfg(windows)]
    pub fn create_button(&self, _label: &str) -> Result<crate::backends_nwg_adapter::Button, Error> {
        let parent = self.parent_cell.borrow().copied().unwrap_or(std::ptr::null_mut());
        crate::backends_nwg_adapter::create_button(parent)
    }

    #[cfg(windows)]
    pub fn create_label(&self, text: &str) -> Result<crate::backends_nwg_adapter::Label, Error> {
        let parent = self.parent_cell.borrow().copied().unwrap_or(std::ptr::null_mut());
        let lbl = crate::backends_nwg_adapter::create_label(parent)?;
        lbl.set_text(text);
        Ok(lbl)
    }

    #[cfg(windows)]
    pub fn create_box(&self, orientation: crate::backends_nwg_adapter::Orientation, spacing: i32) -> Result<crate::backends_nwg_adapter::BoxWidget, Error> {
        crate::backends_nwg_adapter::create_box(orientation, spacing)
    }

    #[cfg(windows)]
    pub fn create_grid(&self) -> Result<crate::backends_nwg_adapter::Grid, Error> {
        crate::backends_nwg_adapter::create_grid()
    }

    #[cfg(windows)]
    pub fn create_entry(&self) -> Result<crate::backends_nwg_adapter::Entry, Error> {
        let parent = self.parent_cell.borrow().copied().unwrap_or(std::ptr::null_mut());
        crate::backends_nwg_adapter::create_entry(parent)
    }

    #[cfg(windows)]
    pub fn create_menu(&self) -> Result<crate::backends_nwg_adapter::Menu, Error> {
        crate::backends_nwg_adapter::create_menu()
    }

    #[cfg(windows)]
    pub fn create_menubar(&self, model: &crate::backends_nwg_adapter::Menu) -> Result<crate::backends_nwg_adapter::MenuBar, Error> {
        let win_hwnd = self.parent_cell.borrow().copied().unwrap_or(std::ptr::null_mut());
        crate::backends_nwg_adapter::create_menubar(model, win_hwnd, self.action_registry.clone())
    }

    #[cfg(windows)]
    pub fn create_simple_action(&self, name: &str) -> Result<crate::backends_nwg_adapter::SimpleAction, Error> {
        crate::backends_nwg_adapter::create_simple_action(name, self.action_registry.clone())
    }

    /// Run the backend main loop
    pub fn run(self) -> Result<(), Error> {
        let boxed = Arc::try_unwrap(self.inner).map_err(|_| Error::Backend("failed to take backend app ownership".into()))?;
        boxed.run().map_err(|e| Error::Backend(format!("{}", e)))
    }
}

impl From<Box<dyn crate::backends::BackendApp>> for App {
    fn from(b: Box<dyn crate::backends::BackendApp>) -> Self {
        App {
            inner: Arc::new(b),
            #[cfg(windows)]
            parent_cell: Rc::new(RefCell::new(None)),
            #[cfg(windows)]
            action_registry: Rc::new(RefCell::new(HashMap::new())),
        }
    }
}
