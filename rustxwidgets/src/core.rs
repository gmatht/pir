use std::cell::RefCell;
use std::collections::HashMap;
use std::os::raw::c_void;
use std::rc::Rc;
use std::sync::Arc;

/// Opaque handler id returned when connecting signals
pub type HandlerId = u64;

/// Cross-platform 2D drawing surface.
/// Each backend implements this trait with its own drawing primitives.
pub trait DrawContext {
    fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, g: f64, b: f64, a: f64);
    fn stroke_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, g: f64, b: f64, a: f64, lw: f64);
    fn draw_text(&mut self, x: f64, y: f64, text: &str, font: &str, size: f64, r: f64, g: f64, b: f64, a: f64);
    fn text_extents(&self, text: &str, font: &str, size: f64) -> (f64, f64, f64, f64);
    fn clear(&mut self, r: f64, g: f64, b: f64, a: f64);
    fn save(&mut self);
    fn restore(&mut self);
    fn clip(&mut self, x: f64, y: f64, w: f64, h: f64);
}

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
        let b = match crate::backends::init() {
            Ok(b) => b,
            Err(e) => return Err(Error::Backend(format!("{}", e))),
        };
        #[cfg(not(feature = "pancurses"))]
        return Ok(App {
            inner: Arc::new(b),
            #[cfg(windows)]
            parent_cell: Rc::new(RefCell::new(None)),
            #[cfg(windows)]
            action_registry: Rc::new(RefCell::new(HashMap::new())),
        });
        #[cfg(feature = "pancurses")]
        return Ok(App { inner: Arc::new(b) });
    }

    // -- Linux paths --

    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_window(&self) -> Result<crate::backends_gtk_adapter::Window, Error> {
        crate::backends_gtk_adapter::create_window().map_err(|e| e)
    }

    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_button(&self, label: &str) -> Result<crate::backends_gtk_adapter::Button, Error> {
        crate::backends_gtk_adapter::create_button(label).map_err(|e| e)
    }

    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_label(&self, text: &str) -> Result<crate::backends_gtk_adapter::Label, Error> {
        crate::backends_gtk_adapter::create_label(text).map_err(|e| e)
    }

    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_box(&self, orientation: gtk_dynamic_loader::Orientation, spacing: i32) -> Result<crate::backends_gtk_adapter::BoxWidget, Error> {
        crate::backends_gtk_adapter::create_box(orientation, spacing).map_err(|e| e)
    }

    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_grid(&self) -> Result<crate::backends_gtk_adapter::Grid, Error> {
        crate::backends_gtk_adapter::create_grid().map_err(|e| e)
    }

    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_entry(&self) -> Result<crate::backends_gtk_adapter::Entry, Error> {
        crate::backends_gtk_adapter::create_entry().map_err(|e| e)
    }

    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_menu(&self) -> Result<crate::backends_gtk_adapter::Menu, Error> {
        crate::backends_gtk_adapter::create_menu().map_err(|e| e)
    }

    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "pancurses"), not(feature = "zork")))]
    /// # Safety
    /// `action_group` must be a valid GActionGroup pointer or null.
    pub unsafe fn create_menubar(&self, model: &crate::backends_gtk_adapter::Menu, action_group: *mut c_void) -> Result<crate::backends_gtk_adapter::MenuBar, Error> {
        crate::backends_gtk_adapter::create_menubar(model, action_group).map_err(|e| e)
    }

    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_simple_action(&self, name: &str) -> Result<crate::backends_gtk_adapter::SimpleAction, Error> {
        crate::backends_gtk_adapter::create_simple_action(name).map_err(|e| e)
    }

    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_dialog(&self) -> Result<crate::backends_gtk_adapter::Dialog, Error> {
        crate::backends_gtk_adapter::create_dialog().map_err(|e| e)
    }

    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_dropdown(&self, items: &[&str]) -> Result<crate::backends_gtk_adapter::DropDown, Error> {
        crate::backends_gtk_adapter::create_dropdown(items).map_err(|e| e)
    }

    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_checkbutton(&self, label: &str) -> Result<crate::backends_gtk_adapter::CheckButton, Error> {
        crate::backends_gtk_adapter::create_checkbutton(label).map_err(|e| e)
    }

    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_radiobutton(&self, label: &str) -> Result<crate::backends_gtk_adapter::RadioButton, Error> {
        crate::backends_gtk_adapter::create_radiobutton(None, label).map_err(|e| e)
    }

    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_textview(&self) -> Result<crate::backends_gtk_adapter::TextView, Error> {
        crate::backends_gtk_adapter::create_textview().map_err(|e| e)
    }

    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_canvas(&self) -> Result<crate::backends_gtk_adapter::Canvas, Error> {
        crate::backends_gtk_adapter::create_canvas()
    }

    // -- Windows paths --

    #[cfg(all(windows, not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_window(&self) -> Result<crate::backends_nwg_adapter::Window, Error> {
        crate::backends_nwg_adapter::create_window(&self.parent_cell)
    }

    #[cfg(all(windows, not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_button(&self, _label: &str) -> Result<crate::backends_nwg_adapter::Button, Error> {
        let parent = self.parent_cell.borrow().copied().unwrap_or(std::ptr::null_mut());
        crate::backends_nwg_adapter::create_button(parent)
    }

    #[cfg(all(windows, not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_label(&self, text: &str) -> Result<crate::backends_nwg_adapter::Label, Error> {
        let parent = self.parent_cell.borrow().copied().unwrap_or(std::ptr::null_mut());
        let lbl = crate::backends_nwg_adapter::create_label(parent)?;
        lbl.set_text(text);
        Ok(lbl)
    }

    #[cfg(all(windows, not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_box(&self, orientation: crate::backends_nwg_adapter::Orientation, spacing: i32) -> Result<crate::backends_nwg_adapter::BoxWidget, Error> {
        crate::backends_nwg_adapter::create_box(orientation, spacing)
    }

    #[cfg(all(windows, not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_grid(&self) -> Result<crate::backends_nwg_adapter::Grid, Error> {
        crate::backends_nwg_adapter::create_grid()
    }

    #[cfg(all(windows, not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_entry(&self) -> Result<crate::backends_nwg_adapter::Entry, Error> {
        let parent = self.parent_cell.borrow().copied().unwrap_or(std::ptr::null_mut());
        crate::backends_nwg_adapter::create_entry(parent)
    }

    #[cfg(all(windows, not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_menu(&self) -> Result<crate::backends_nwg_adapter::Menu, Error> {
        crate::backends_nwg_adapter::create_menu()
    }

    #[cfg(all(windows, not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_menubar(&self, model: &crate::backends_nwg_adapter::Menu) -> Result<crate::backends_nwg_adapter::MenuBar, Error> {
        let win_hwnd = self.parent_cell.borrow().copied().unwrap_or(std::ptr::null_mut());
        crate::backends_nwg_adapter::create_menubar(model, win_hwnd, self.action_registry.clone())
    }

    #[cfg(all(windows, not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_simple_action(&self, name: &str) -> Result<crate::backends_nwg_adapter::SimpleAction, Error> {
        crate::backends_nwg_adapter::create_simple_action(name, self.action_registry.clone())
    }

    // -- Pancurses paths --

    #[cfg(feature = "pancurses")]
    pub fn create_window(&self) -> Result<crate::backends_pancurses_adapter::Window, Error> {
        crate::backends_pancurses_adapter::create_window()
    }

    #[cfg(feature = "pancurses")]
    pub fn create_button(&self, label: &str) -> Result<crate::backends_pancurses_adapter::Button, Error> {
        crate::backends_pancurses_adapter::create_button(label)
    }

    #[cfg(feature = "pancurses")]
    pub fn create_label(&self, text: &str) -> Result<crate::backends_pancurses_adapter::Label, Error> {
        crate::backends_pancurses_adapter::create_label(text)
    }

    #[cfg(feature = "pancurses")]
    pub fn create_box(&self, orientation: crate::backends_pancurses_adapter::Orientation, spacing: i32) -> Result<crate::backends_pancurses_adapter::BoxWidget, Error> {
        crate::backends_pancurses_adapter::create_box(orientation, spacing)
    }

    #[cfg(feature = "pancurses")]
    pub fn create_grid(&self) -> Result<crate::backends_pancurses_adapter::Grid, Error> {
        crate::backends_pancurses_adapter::create_grid()
    }

    #[cfg(feature = "pancurses")]
    pub fn create_entry(&self) -> Result<crate::backends_pancurses_adapter::Entry, Error> {
        crate::backends_pancurses_adapter::create_entry()
    }

    #[cfg(feature = "pancurses")]
    pub fn create_menu(&self) -> Result<crate::backends_pancurses_adapter::Menu, Error> {
        crate::backends_pancurses_adapter::create_menu()
    }

    #[cfg(feature = "pancurses")]
    pub fn create_menubar(&self, model: &crate::backends_pancurses_adapter::Menu, _action_group: *mut std::os::raw::c_void) -> Result<crate::backends_pancurses_adapter::MenuBar, Error> {
        crate::backends_pancurses_adapter::create_menubar(model, _action_group)
    }

    #[cfg(feature = "pancurses")]
    pub fn create_simple_action(&self, name: &str) -> Result<crate::backends_pancurses_adapter::SimpleAction, Error> {
        crate::backends_pancurses_adapter::create_simple_action(name)
    }

    #[cfg(feature = "pancurses")]
    pub fn create_dialog(&self) -> Result<crate::backends_pancurses_adapter::Dialog, Error> {
        crate::backends_pancurses_adapter::create_dialog()
    }

    #[cfg(feature = "pancurses")]
    pub fn create_dropdown(&self, items: &[&str]) -> Result<crate::backends_pancurses_adapter::DropDown, Error> {
        crate::backends_pancurses_adapter::create_dropdown(items)
    }

    #[cfg(feature = "pancurses")]
    pub fn create_checkbutton(&self, label: &str) -> Result<crate::backends_pancurses_adapter::CheckButton, Error> {
        crate::backends_pancurses_adapter::create_checkbutton(label)
    }

    #[cfg(feature = "pancurses")]
    pub fn create_radiobutton(&self, label: &str) -> Result<crate::backends_pancurses_adapter::RadioButton, Error> {
        crate::backends_pancurses_adapter::create_radiobutton(None, label)
    }

    #[cfg(feature = "pancurses")]
    pub fn create_textview(&self) -> Result<crate::backends_pancurses_adapter::TextView, Error> {
        crate::backends_pancurses_adapter::create_textview()
    }

    // -- Zork paths --

    #[cfg(feature = "zork")]
    pub fn create_window(&self) -> Result<crate::backends_zork_adapter::Window, Error> {
        crate::backends_zork_adapter::create_window()
    }

    #[cfg(feature = "zork")]
    pub fn create_button(&self, label: &str) -> Result<crate::backends_zork_adapter::Button, Error> {
        crate::backends_zork_adapter::create_button(label)
    }

    #[cfg(feature = "zork")]
    pub fn create_label(&self, text: &str) -> Result<crate::backends_zork_adapter::Label, Error> {
        crate::backends_zork_adapter::create_label(text)
    }

    #[cfg(feature = "zork")]
    pub fn create_box(&self, orientation: crate::backends_zork_adapter::Orientation, spacing: i32) -> Result<crate::backends_zork_adapter::BoxWidget, Error> {
        crate::backends_zork_adapter::create_box(orientation, spacing)
    }

    #[cfg(feature = "zork")]
    pub fn create_grid(&self) -> Result<crate::backends_zork_adapter::Grid, Error> {
        crate::backends_zork_adapter::create_grid()
    }

    #[cfg(feature = "zork")]
    pub fn create_entry(&self) -> Result<crate::backends_zork_adapter::Entry, Error> {
        crate::backends_zork_adapter::create_entry()
    }

    #[cfg(feature = "zork")]
    pub fn create_menu(&self) -> Result<crate::backends_zork_adapter::Menu, Error> {
        crate::backends_zork_adapter::create_menu()
    }

    #[cfg(feature = "zork")]
    pub fn create_menubar(&self, model: &crate::backends_zork_adapter::Menu, _action_group: *mut std::os::raw::c_void) -> Result<crate::backends_zork_adapter::MenuBar, Error> {
        crate::backends_zork_adapter::create_menubar(model, _action_group)
    }

    #[cfg(feature = "zork")]
    pub fn create_simple_action(&self, name: &str) -> Result<crate::backends_zork_adapter::SimpleAction, Error> {
        crate::backends_zork_adapter::create_simple_action(name)
    }

    #[cfg(feature = "zork")]
    pub fn create_dialog(&self) -> Result<crate::backends_zork_adapter::Dialog, Error> {
        crate::backends_zork_adapter::create_dialog()
    }

    #[cfg(feature = "zork")]
    pub fn create_dropdown(&self, items: &[&str]) -> Result<crate::backends_zork_adapter::DropDown, Error> {
        crate::backends_zork_adapter::create_dropdown(items)
    }

    #[cfg(feature = "zork")]
    pub fn create_checkbutton(&self, label: &str) -> Result<crate::backends_zork_adapter::CheckButton, Error> {
        crate::backends_zork_adapter::create_checkbutton(label)
    }

    #[cfg(feature = "zork")]
    pub fn create_radiobutton(&self, label: &str) -> Result<crate::backends_zork_adapter::RadioButton, Error> {
        crate::backends_zork_adapter::create_radiobutton(None, label)
    }

    #[cfg(feature = "zork")]
    pub fn create_textview(&self) -> Result<crate::backends_zork_adapter::TextView, Error> {
        crate::backends_zork_adapter::create_textview()
    }

    // -- WASM paths --

    #[cfg(all(target_arch = "wasm32", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_window(&self) -> Result<crate::backends_wasm_adapter::Window, Error> {
        crate::backends_wasm_adapter::create_window()
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_button(&self, label: &str) -> Result<crate::backends_wasm_adapter::Button, Error> {
        crate::backends_wasm_adapter::create_button(label)
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_label(&self, text: &str) -> Result<crate::backends_wasm_adapter::Label, Error> {
        crate::backends_wasm_adapter::create_label(text)
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_box(&self, orientation: crate::backends_wasm_adapter::Orientation, spacing: i32) -> Result<crate::backends_wasm_adapter::BoxWidget, Error> {
        crate::backends_wasm_adapter::create_box(orientation, spacing)
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_grid(&self) -> Result<crate::backends_wasm_adapter::Grid, Error> {
        crate::backends_wasm_adapter::create_grid()
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_entry(&self) -> Result<crate::backends_wasm_adapter::Entry, Error> {
        crate::backends_wasm_adapter::create_entry()
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_menu(&self) -> Result<crate::backends_wasm_adapter::Menu, Error> {
        crate::backends_wasm_adapter::create_menu()
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_menubar(&self, model: &crate::backends_wasm_adapter::Menu, action_group: *mut c_void) -> Result<crate::backends_wasm_adapter::MenuBar, Error> {
        crate::backends_wasm_adapter::create_menubar(model, action_group)
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_simple_action(&self, name: &str) -> Result<crate::backends_wasm_adapter::SimpleAction, Error> {
        crate::backends_wasm_adapter::create_simple_action(name)
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_dialog(&self) -> Result<crate::backends_wasm_adapter::Dialog, Error> {
        crate::backends_wasm_adapter::create_dialog()
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_dropdown(&self, items: &[&str]) -> Result<crate::backends_wasm_adapter::DropDown, Error> {
        crate::backends_wasm_adapter::create_dropdown(items)
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_checkbutton(&self, label: &str) -> Result<crate::backends_wasm_adapter::CheckButton, Error> {
        crate::backends_wasm_adapter::create_checkbutton(label)
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "pancurses"), not(feature = "zork")))]
    pub fn create_radiobutton(&self, label: &str) -> Result<crate::backends_wasm_adapter::RadioButton, Error> {
        crate::backends_wasm_adapter::create_radiobutton(None, label)
    }

#[cfg(all(target_arch = "wasm32", not(feature = "pancurses")))]
pub fn create_textview(&self) -> Result<crate::backends_wasm_adapter::TextView, Error> {
    crate::backends_wasm_adapter::create_textview()
}

// -- Android paths --

#[cfg(all(target_os = "android", not(feature = "zork")))]
pub fn create_window(&self) -> Result<crate::backends_android_adapter::Window, Error> {
    crate::backends_android_adapter::create_window()
}

#[cfg(all(target_os = "android", not(feature = "zork")))]
pub fn create_button(&self, label: &str) -> Result<crate::backends_android_adapter::Button, Error> {
    crate::backends_android_adapter::create_button(label)
}

#[cfg(all(target_os = "android", not(feature = "zork")))]
pub fn create_label(&self, text: &str) -> Result<crate::backends_android_adapter::Label, Error> {
    crate::backends_android_adapter::create_label(text)
}

#[cfg(all(target_os = "android", not(feature = "zork")))]
pub fn create_grid(&self) -> Result<crate::backends_android_adapter::Grid, Error> {
    crate::backends_android_adapter::create_grid()
}

#[cfg(all(target_os = "android", not(feature = "zork")))]
pub fn create_dropdown(&self, items: &[&str]) -> Result<crate::backends_android_adapter::DropDown, Error> {
    crate::backends_android_adapter::create_dropdown(items)
}

#[cfg(all(target_os = "android", not(feature = "zork")))]
pub fn create_checkbutton(&self, label: &str) -> Result<crate::backends_android_adapter::CheckButton, Error> {
    crate::backends_android_adapter::create_checkbutton(label)
}

#[cfg(all(target_os = "android", not(feature = "zork")))]
pub fn create_radiobutton(&self, label: &str) -> Result<crate::backends_android_adapter::RadioButton, Error> {
    crate::backends_android_adapter::create_radiobutton(None, label)
}

#[cfg(all(target_os = "android", not(feature = "zork")))]
pub fn create_dialog(&self) -> Result<crate::backends_android_adapter::Dialog, Error> {
    crate::backends_android_adapter::create_dialog()
}

#[cfg(all(target_os = "android", not(feature = "zork")))]
pub fn create_textview(&self) -> Result<crate::backends_android_adapter::TextView, Error> {
    crate::backends_android_adapter::create_textview()
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
