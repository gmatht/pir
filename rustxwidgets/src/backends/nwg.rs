#[cfg(windows)]
mod nwg_backend {
    // Placeholder adapter for native-windows-gui (NWG). For now we keep minimal types
    use native_windows_gui as nwg;
    use crate::core::Widget;
    use std::os::raw::c_void;

    pub struct App;

    pub fn init() -> Result<Box<dyn crate::backends::BackendApp>, nwg::NwgError> {
        nwg::init()?;
        Ok(Box::new(App))
    }

    #[repr(transparent)]
    pub struct Window(pub nwg::Window);

    impl Widget for Window {
        fn raw_handle(&self) -> *mut c_void { std::ptr::null_mut() }
    }
}

#[cfg(windows)]
pub use nwg_backend::*;
