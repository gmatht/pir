// ---------------------------------------------------------------------------
// Cross-platform key constants
// ---------------------------------------------------------------------------
pub mod key {
    #[cfg(unix)]
    mod plat {
        pub const RETURN: u32 = 0xFF0D;
        pub const ENTER: u32 = 0xFF8D;
        pub const ESCAPE: u32 = 0xFF1B;
        pub const BACKSPACE: u32 = 0xFF08;
        pub const DELETE: u32 = 0xFFFF;
        pub const LEFT: u32 = 0xFF51;
        pub const UP: u32 = 0xFF52;
        pub const RIGHT: u32 = 0xFF53;
        pub const DOWN: u32 = 0xFF54;
        pub const TAB: u32 = 0xFF09;
        pub const HOME: u32 = 0xFF50;
        pub const END: u32 = 0xFF57;
        pub const PAGE_UP: u32 = 0xFF55;
        pub const PAGE_DOWN: u32 = 0xFF56;
        pub const F1: u32 = 0xFFBE;
        pub const F2: u32 = 0xFFBF;
        pub const ALT_L: u32 = 0xFFE9;
        pub const ALT_R: u32 = 0xFFEA;
    }
    #[cfg(windows)]
    mod plat {
        pub const RETURN: u32 = 0x0D;
        pub const ENTER: u32 = 0x6C;
        pub const ESCAPE: u32 = 0x1B;
        pub const BACKSPACE: u32 = 0x08;
        pub const DELETE: u32 = 0x2E;
        pub const LEFT: u32 = 0x25;
        pub const UP: u32 = 0x26;
        pub const RIGHT: u32 = 0x27;
        pub const DOWN: u32 = 0x28;
        pub const TAB: u32 = 0x09;
        pub const HOME: u32 = 0x24;
        pub const END: u32 = 0x23;
        pub const PAGE_UP: u32 = 0x21;
        pub const PAGE_DOWN: u32 = 0x22;
        pub const F1: u32 = 0x70;
        pub const F2: u32 = 0x71;
        pub const ALT_L: u32 = 0x12;
        pub const ALT_R: u32 = 0x12;
    }
    #[cfg(target_arch = "wasm32")]
    mod plat {
        pub const RETURN: u32 = 0x0D;
        pub const ENTER: u32 = 0x0D;
        pub const ESCAPE: u32 = 0x1B;
        pub const BACKSPACE: u32 = 0x08;
        pub const DELETE: u32 = 0x2E;
        pub const LEFT: u32 = 0x25;
        pub const UP: u32 = 0x26;
        pub const RIGHT: u32 = 0x27;
        pub const DOWN: u32 = 0x28;
        pub const TAB: u32 = 0x09;
        pub const HOME: u32 = 0x24;
        pub const END: u32 = 0x23;
        pub const PAGE_UP: u32 = 0x21;
        pub const PAGE_DOWN: u32 = 0x22;
        pub const F1: u32 = 0x70;
        pub const F2: u32 = 0x71;
        pub const ALT_L: u32 = 0x12;
        pub const ALT_R: u32 = 0x12;
    }
    pub use plat::*;

    /// Normalize a platform key value so it can be compared with the
    /// constants above. On Windows this masks with 0xFF to strip the
    /// extended-key flag. On Unix the keyval is returned as-is.
    pub fn normalize(keyval: u32) -> u32 {
        #[cfg(windows)]
        { keyval & 0xFF }
        #[cfg(not(windows))]
        { keyval }
    }
}

use std::cell::RefCell;
#[cfg(windows)]
use std::collections::HashMap;
use std::os::raw::c_void;
use std::rc::Rc;

/// Install SIGABRT and SIGSEGV handlers that print a backtrace
/// to stderr, then re-raise with the default handler.
/// On non-Unix platforms this is a no-op.
pub fn install_debug_crash_handlers() {
    #[cfg(unix)]
    unsafe {
        extern "C" fn sigabrt_handler(_sig: i32) {
            unsafe {
                write_stderr(b"\nSIGABRT\n");
                write_backtrace_to_stderr();
                libc::signal(libc::SIGABRT, libc::SIG_DFL);
                libc::raise(libc::SIGABRT);
            }
        }
        extern "C" fn sigsegv_handler(_sig: i32) {
            unsafe {
                write_stderr(b"\nSIGSEGV\n");
                write_backtrace_to_stderr();
                libc::signal(libc::SIGSEGV, libc::SIG_DFL);
                libc::raise(libc::SIGSEGV);
            }
        }
        libc::signal(libc::SIGABRT, sigabrt_handler as *const () as usize);
        libc::signal(libc::SIGSEGV, sigsegv_handler as *const () as usize);
    }
    #[cfg(not(unix))]
    {
        let _ = ();
    }
}

#[cfg(unix)]
unsafe fn write_stderr(msg: &[u8]) {
    libc::write(libc::STDERR_FILENO, msg.as_ptr() as *const libc::c_void, msg.len());
}

#[cfg(unix)]
unsafe fn write_backtrace_to_stderr() {
    const SIZE: usize = 128;
    let mut buf: [*mut libc::c_void; SIZE] = std::mem::zeroed();
    write_stderr(b"===== backtrace =====\n");
    let n = libc::backtrace(buf.as_mut_ptr(), SIZE as i32);
    for i in 0..n.min(SIZE as i32) {
        let addr = buf[i as usize] as usize;
        if addr == 0 { break; }
        let mut hex = [0u8; 19];
        hex[0] = b' '; hex[1] = b' '; hex[18] = b'\n';
        let mut v = addr;
        let mut pos = 17;
        loop {
            hex[pos] = b"0123456789abcdef"[v & 0xf];
            v >>= 4;
            if v == 0 || pos == 1 { break; }
            pos -= 1;
        }
        write_stderr(&hex);
    }
    write_stderr(b"===== end backtrace =====\n");
}

/// Detect terminal size. On Unix uses `ioctl(TIOCGWINSZ)`;
/// on Windows uses `GetConsoleScreenBufferInfo`.
pub fn terminal_size() -> Option<(usize, usize)> {
    #[cfg(unix)]
    {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0
        {
            return Some((ws.ws_col as usize, ws.ws_row as usize));
        }
    }
    #[cfg(windows)]
    {
        use winapi::um::processenv::GetStdHandle;
        use winapi::um::winbase::STD_OUTPUT_HANDLE;
        use winapi::um::wincon::GetConsoleScreenBufferInfo;
        use winapi::um::wincon::CONSOLE_SCREEN_BUFFER_INFO;
        unsafe {
            let handle = GetStdHandle(STD_OUTPUT_HANDLE);
            if handle != winapi::um::handleapi::INVALID_HANDLE_VALUE {
                let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
                if GetConsoleScreenBufferInfo(handle, &mut info) != 0 {
                    let cols = info.srWindow.Right - info.srWindow.Left + 1;
                    let rows = info.srWindow.Bottom - info.srWindow.Top + 1;
                    if cols > 0 && rows > 0 {
                        return Some((cols as usize, rows as usize));
                    }
                }
            }
        }
    }
    None
}

/// Opaque handler id returned when connecting signals
pub type HandlerId = u64;

/// Cross-platform 2D drawing surface.
/// Each backend implements this trait with its own drawing primitives.
pub trait DrawContext {
    fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, g: f64, b: f64, a: f64);
    fn stroke_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, g: f64, b: f64, a: f64, lw: f64);
    /// Draw text with normal (non-bold, non-italic) style.
    fn draw_text(&mut self, x: f64, y: f64, text: &str, font: &str, size: f64, r: f64, g: f64, b: f64, a: f64) {
        self.draw_text_styled(x, y, text, font, size, r, g, b, a, 0, 0)
    }
    /// Draw text with explicit Cairo slant (0=normal, 1=italic, 2=oblique) and
    /// weight (0=normal, 1=bold).
    fn draw_text_styled(&mut self, x: f64, y: f64, text: &str, font: &str, size: f64,
                        r: f64, g: f64, b: f64, a: f64, slant: i32, weight: i32);
    /// Measure text extents with normal (non-bold) weight.
    fn text_extents(&self, text: &str, font: &str, size: f64) -> (f64, f64, f64, f64) {
        self.text_extents_styled(text, font, size, 0, 0)
    }
    /// Measure text extents with explicit Cairo slant and weight.
    fn text_extents_styled(&self, text: &str, font: &str, size: f64, slant: i32, weight: i32) -> (f64, f64, f64, f64);
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
#[derive(Clone)]
pub struct App {
    inner: Rc<RefCell<Option<Box<dyn crate::backends::BackendApp>>>>,
    #[cfg(all(windows, not(feature = "zork")))]
    parent_cell: Rc<RefCell<Option<*mut c_void>>>,
    #[cfg(all(windows, not(feature = "zork")))]
    action_registry: Rc<RefCell<HashMap<String, Box<dyn FnMut()>>>>,
    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    action_group: Rc<RefCell<Option<crate::backends_gtk_adapter::Application>>>,
}

impl App {
    /// Initialize the default backend and return an App wrapper.
    /// Uses the priority chain from `backends::init()` (gtk > nwg > wasm > android > pancurses).
    /// When compiled with both `gui` and `pancurses`, this uses the GUI backend path;
    /// the pancurses backend initializes separately via `backends::pancurses::init()`.
    pub fn init() -> Result<Self, Error> {
        let _ = std::fs::write("/tmp/corro_init.txt", "App::init() called\n");
        let b = match crate::backends::init() {
            Ok(b) => b,
            Err(e) => return Err(Error::Backend(format!("{}", e))),
        };
        #[cfg(all(windows, not(feature = "zork")))]
        {
            // Create a hidden parent window for child controls
            let parent_hwnd = crate::backends::nwg::create_hidden_parent()?;
            return Ok(App {
                inner: Rc::new(RefCell::new(Some(b))),
                parent_cell: Rc::new(RefCell::new(Some(parent_hwnd))),
                action_registry: Rc::new(RefCell::new(HashMap::new())),
            });
        }
        #[cfg(not(all(windows, not(feature = "zork"))))]
        return Ok(App {
            inner: Rc::new(RefCell::new(Some(b))),
            #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
            action_group: Rc::new(RefCell::new(None)),
        });
    }

    // -- Linux paths --

    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    pub fn create_window(&self) -> Result<crate::backends_gtk_adapter::Window, Error> {
        crate::backends_gtk_adapter::create_window().map_err(|e| e)
    }

    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs")))]
    pub fn create_tabview(&self) -> Result<crate::backends_gtk_adapter::TabView, Error> {
        crate::backends_gtk_adapter::TabView::new(std::ptr::null_mut()).map_err(|e| Error::Backend(format!("{}", e)))
    }

    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    pub fn create_button(&self, label: &str) -> Result<crate::backends_gtk_adapter::Button, Error> {
        crate::backends_gtk_adapter::create_button(label).map_err(|e| e)
    }

    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    pub fn create_label(&self, text: &str) -> Result<crate::backends_gtk_adapter::Label, Error> {
        crate::backends_gtk_adapter::create_label(text).map_err(|e| e)
    }

    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    pub fn create_box(&self, orientation: crate::backends_gtk_adapter::Orientation, spacing: i32) -> Result<crate::backends_gtk_adapter::BoxWidget, Error> {
        crate::backends_gtk_adapter::create_box(orientation, spacing).map_err(|e| e)
    }

    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    pub fn create_grid(&self) -> Result<crate::backends_gtk_adapter::Grid, Error> {
        crate::backends_gtk_adapter::create_grid().map_err(|e| e)
    }

    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    pub fn create_entry(&self) -> Result<crate::backends_gtk_adapter::Entry, Error> {
        crate::backends_gtk_adapter::create_entry().map_err(|e| e)
    }

    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    pub fn create_menu(&self) -> Result<crate::backends_gtk_adapter::Menu, Error> {
        crate::backends_gtk_adapter::create_menu().map_err(|e| e)
    }

    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    /// # Safety
    /// `action_group` must be a valid GActionGroup pointer or null.
    pub unsafe fn create_menubar(&self, model: &crate::backends_gtk_adapter::Menu, action_group: *mut c_void) -> Result<crate::backends_gtk_adapter::MenuBar, Error> {
        crate::backends_gtk_adapter::create_menubar(model, action_group).map_err(|e| e)
    }

    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    pub fn create_simple_action(&self, name: &str) -> Result<crate::backends_gtk_adapter::SimpleAction, Error> {
        crate::backends_gtk_adapter::create_simple_action(name).map_err(|e| e)
    }

    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    pub fn create_dialog(&self) -> Result<crate::backends_gtk_adapter::Dialog, Error> {
        crate::backends_gtk_adapter::create_dialog().map_err(|e| e)
    }

    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    pub fn create_dropdown(&self, items: &[&str]) -> Result<crate::backends_gtk_adapter::DropDown, Error> {
        crate::backends_gtk_adapter::create_dropdown(items).map_err(|e| e)
    }

    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    pub fn create_checkbutton(&self, label: &str) -> Result<crate::backends_gtk_adapter::CheckButton, Error> {
        crate::backends_gtk_adapter::create_checkbutton(label).map_err(|e| e)
    }

    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    pub fn create_radiobutton(&self, label: &str) -> Result<crate::backends_gtk_adapter::RadioButton, Error> {
        crate::backends_gtk_adapter::create_radiobutton(None, label).map_err(|e| e)
    }

    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    pub fn create_textview(&self) -> Result<crate::backends_gtk_adapter::TextView, Error> {
        crate::backends_gtk_adapter::create_textview().map_err(|e| e)
    }

    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    pub fn create_canvas(&self) -> Result<crate::backends_gtk_adapter::Canvas, Error> {
        crate::backends_gtk_adapter::create_canvas()
    }

    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    pub fn create_scrolled_window(&self) -> Result<crate::backends_gtk_adapter::ScrolledWindow, Error> {
        crate::backends_gtk_adapter::create_scrolled_window()
    }

    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    pub fn create_overlay(&self) -> Result<crate::backends_gtk_adapter::Overlay, Error> {
        crate::backends_gtk_adapter::create_overlay()
    }

    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    pub fn open_file(&self, title: &str) -> Result<Option<String>, Error> {
        crate::backends_gtk_adapter::open_file(title)
    }

    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    pub fn save_file(&self, title: &str) -> Result<Option<String>, Error> {
        crate::backends_gtk_adapter::save_file(title)
    }

    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    pub fn create_spreadsheet(&self, rows: usize, cols: usize) -> Result<crate::backends_gtk_adapter::Spreadsheet, Error> {
        crate::backends_gtk_adapter::create_spreadsheet(rows, cols)
    }

    // -- Windows paths --

    #[cfg(all(windows, not(feature = "zork")))]
    pub fn create_window(&self) -> Result<crate::backends_nwg_adapter::Window, Error> {
        crate::backends_nwg_adapter::create_window(&self.parent_cell)
    }

    #[cfg(all(windows, not(feature = "zork")))]
    pub fn create_button(&self, label: &str) -> Result<crate::backends_nwg_adapter::Button, Error> {
        let parent = self.parent_cell.borrow().as_ref().copied().unwrap_or(std::ptr::null_mut());
        crate::backends_nwg_adapter::create_button(parent, label)
    }

    #[cfg(all(windows, not(feature = "zork")))]
    pub fn create_label(&self, text: &str) -> Result<crate::backends_nwg_adapter::Label, Error> {
        let parent = self.parent_cell.borrow().as_ref().copied().unwrap_or(std::ptr::null_mut());
        let lbl = crate::backends_nwg_adapter::create_label(parent)?;
        lbl.set_text(text);
        Ok(lbl)
    }

    #[cfg(all(windows, not(feature = "zork")))]
    pub fn create_box(&self, orientation: crate::backends::nwg::Orientation, spacing: i32) -> Result<crate::backends_nwg_adapter::BoxWidget, Error> {
        let parent = self.parent_cell.borrow().as_ref().copied().unwrap_or(std::ptr::null_mut());
        crate::backends_nwg_adapter::create_box(orientation, spacing, parent)
    }
    pub fn create_tabview(&self) -> Result<crate::backends_nwg_adapter::TabView, Error> {
        let parent = self.parent_cell.borrow().as_ref().copied().unwrap_or(std::ptr::null_mut());
        crate::backends_nwg_adapter::TabView::new(parent)
    }

    #[cfg(all(windows, not(feature = "zork")))]
    pub fn create_grid(&self) -> Result<crate::backends_nwg_adapter::Grid, Error> {
        crate::backends_nwg_adapter::create_grid()
    }

    #[cfg(all(windows, not(feature = "zork")))]
    pub fn create_entry(&self) -> Result<crate::backends_nwg_adapter::Entry, Error> {
        let parent = self.parent_cell.borrow().as_ref().copied().unwrap_or(std::ptr::null_mut());
        crate::backends_nwg_adapter::create_entry(parent)
    }

    #[cfg(all(windows, not(feature = "zork")))]
    pub fn create_menu(&self) -> Result<crate::backends_nwg_adapter::Menu, Error> {
        crate::backends_nwg_adapter::create_menu()
    }

    #[cfg(all(windows, not(feature = "zork")))]
    /// # Safety
    /// `window_hwnd` must be a valid HWND.
    pub unsafe fn create_menubar(&self, model: &crate::backends_nwg_adapter::Menu, window_hwnd: *mut c_void) -> Result<crate::backends_nwg_adapter::MenuBar, Error> {
        crate::backends_nwg_adapter::create_menubar(model, window_hwnd, self.action_registry.clone())
    }

    #[cfg(all(windows, not(feature = "zork")))]
    pub fn create_simple_action(&self, name: &str) -> Result<crate::backends_nwg_adapter::SimpleAction, Error> {
        crate::backends_nwg_adapter::create_simple_action(name, self.action_registry.clone())
    }

    #[cfg(all(windows, not(feature = "zork")))]
    pub fn create_dialog(&self) -> Result<crate::backends_nwg_adapter::Dialog, Error> {
        crate::backends_nwg_adapter::create_dialog(&self.parent_cell)
    }

    #[cfg(all(windows, not(feature = "zork")))]
    pub fn create_dropdown(&self, items: &[&str]) -> Result<crate::backends_nwg_adapter::DropDown, Error> {
        let parent = self.parent_cell.borrow().as_ref().copied().unwrap_or(std::ptr::null_mut());
        crate::backends_nwg_adapter::create_dropdown(parent, items)
    }

    #[cfg(all(windows, not(feature = "zork")))]
    pub fn create_checkbutton(&self, label: &str) -> Result<crate::backends_nwg_adapter::CheckButton, Error> {
        let parent = self.parent_cell.borrow().as_ref().copied().unwrap_or(std::ptr::null_mut());
        let cb = crate::backends_nwg_adapter::create_checkbutton(parent)?;
        cb.set_label(label);
        Ok(cb)
    }

    #[cfg(all(windows, not(feature = "zork")))]
    pub fn create_radiobutton(&self, label: &str) -> Result<crate::backends_nwg_adapter::RadioButton, Error> {
        let parent = self.parent_cell.borrow().as_ref().copied().unwrap_or(std::ptr::null_mut());
        let rb = crate::backends_nwg_adapter::create_radiobutton(parent)?;
        rb.set_label(label);
        Ok(rb)
    }

    #[cfg(all(windows, not(feature = "zork")))]
    pub fn create_textview(&self) -> Result<crate::backends_nwg_adapter::TextView, Error> {
        let parent = self.parent_cell.borrow().as_ref().copied().unwrap_or(std::ptr::null_mut());
        crate::backends_nwg_adapter::create_textview(parent)
    }

    #[cfg(all(windows, not(feature = "zork")))]
    pub fn create_canvas(&self) -> Result<crate::backends_nwg_adapter::Canvas, Error> {
        let parent = self.parent_cell.borrow().as_ref().copied().unwrap_or(std::ptr::null_mut());
        crate::backends_nwg_adapter::create_canvas(parent)
    }

    #[cfg(all(windows, not(feature = "zork")))]
    pub fn create_overlay(&self) -> Result<crate::backends_nwg_adapter::Overlay, Error> {
        let parent = self.parent_cell.borrow().as_ref().copied().unwrap_or(std::ptr::null_mut());
        crate::backends_nwg_adapter::create_overlay(parent)
    }

    #[cfg(all(windows, not(feature = "zork")))]
    pub fn create_scrolled_window(&self) -> Result<crate::backends_nwg_adapter::ScrolledWindow, Error> {
        let parent = self.parent_cell.borrow().as_ref().copied().unwrap_or(std::ptr::null_mut());
        crate::backends_nwg_adapter::create_scrolled_window(parent)
    }

    #[cfg(all(windows, not(feature = "zork")))]
    pub fn open_file(&self, title: &str) -> Result<Option<String>, Error> {
        let parent = self.parent_cell.borrow().as_ref().copied().unwrap_or(std::ptr::null_mut());
        crate::backends_nwg_adapter::open_file(title, parent)
    }

    #[cfg(all(windows, not(feature = "zork")))]
    pub fn save_file(&self, title: &str) -> Result<Option<String>, Error> {
        let parent = self.parent_cell.borrow().as_ref().copied().unwrap_or(std::ptr::null_mut());
        crate::backends_nwg_adapter::save_file(title, parent)
    }

    // -- Pancurses paths --

    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn create_window(&self) -> Result<crate::backends_pancurses_adapter::Window, Error> {
        crate::backends_pancurses_adapter::create_window()
    }

    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn create_button(&self, label: &str) -> Result<crate::backends_pancurses_adapter::Button, Error> {
        crate::backends_pancurses_adapter::create_button(label)
    }

    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn create_label(&self, text: &str) -> Result<crate::backends_pancurses_adapter::Label, Error> {
        crate::backends_pancurses_adapter::create_label(text)
    }

    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn create_box(&self, orientation: crate::backends_pancurses_adapter::Orientation, spacing: i32) -> Result<crate::backends_pancurses_adapter::BoxWidget, Error> {
        crate::backends_pancurses_adapter::create_box(orientation, spacing)
    }
    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn create_tabview(&self) -> Result<crate::backends_pancurses_adapter::TabView, Error> {
        crate::backends_pancurses_adapter::TabView::new(std::ptr::null_mut())
    }

    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn create_grid(&self) -> Result<crate::backends_pancurses_adapter::Grid, Error> {
        crate::backends_pancurses_adapter::create_grid()
    }

    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn create_entry(&self) -> Result<crate::backends_pancurses_adapter::Entry, Error> {
        crate::backends_pancurses_adapter::create_entry()
    }

    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn create_menu(&self) -> Result<crate::backends_pancurses_adapter::Menu, Error> {
        crate::backends_pancurses_adapter::create_menu()
    }

    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn create_menubar(&self, model: &crate::backends_pancurses_adapter::Menu, _action_group: *mut std::os::raw::c_void) -> Result<crate::backends_pancurses_adapter::MenuBar, Error> {
        crate::backends_pancurses_adapter::create_menubar(model, _action_group)
    }

    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn create_simple_action(&self, name: &str) -> Result<crate::backends_pancurses_adapter::SimpleAction, Error> {
        crate::backends_pancurses_adapter::create_simple_action(name)
    }

    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn create_dialog(&self) -> Result<crate::backends_pancurses_adapter::Dialog, Error> {
        crate::backends_pancurses_adapter::create_dialog()
    }

    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn create_dropdown(&self, items: &[&str]) -> Result<crate::backends_pancurses_adapter::DropDown, Error> {
        crate::backends_pancurses_adapter::create_dropdown(items)
    }

    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn create_checkbutton(&self, label: &str) -> Result<crate::backends_pancurses_adapter::CheckButton, Error> {
        crate::backends_pancurses_adapter::create_checkbutton(label)
    }

    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn create_radiobutton(&self, label: &str) -> Result<crate::backends_pancurses_adapter::RadioButton, Error> {
        crate::backends_pancurses_adapter::create_radiobutton(None, label)
    }

    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn create_textview(&self) -> Result<crate::backends_pancurses_adapter::TextView, Error> {
        crate::backends_pancurses_adapter::create_textview()
    }

    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn create_spreadsheet(&self, rows: u32, cols: u32) -> Result<crate::backends_pancurses_adapter::Spreadsheet, Error> {
        crate::backends_pancurses_adapter::create_spreadsheet(rows, cols)
    }

    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn create_canvas(&self) -> Result<crate::backends_pancurses_adapter::Canvas, Error> {
        crate::backends_pancurses_adapter::create_canvas()
    }

    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn create_overlay(&self) -> Result<crate::backends_pancurses_adapter::Overlay, Error> {
        crate::backends_pancurses_adapter::create_overlay()
    }

    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn create_scrolled_window(&self) -> Result<crate::backends_pancurses_adapter::ScrolledWindow, Error> {
        crate::backends_pancurses_adapter::create_scrolled_window()
    }

    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn open_file(&self, title: &str) -> Result<Option<String>, Error> {
        crate::backends_pancurses_adapter::open_file(title)
    }

    #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
    pub fn save_file(&self, title: &str) -> Result<Option<String>, Error> {
        crate::backends_pancurses_adapter::save_file(title)
    }

    // -- Zork paths --

    #[cfg(feature = "zork")]
    pub fn create_canvas(&self) -> Result<crate::backends_zork_adapter::Canvas, Error> {
        crate::backends_zork_adapter::create_canvas()
    }

    #[cfg(feature = "zork")]
    pub fn create_overlay(&self) -> Result<crate::backends_zork_adapter::Overlay, Error> {
        crate::backends_zork_adapter::create_overlay()
    }

    #[cfg(feature = "zork")]
    pub fn create_scrolled_window(&self) -> Result<crate::backends_zork_adapter::ScrolledWindow, Error> {
        crate::backends_zork_adapter::create_scrolled_window()
    }

    #[cfg(feature = "zork")]
    pub fn open_file(&self, title: &str) -> Result<Option<String>, Error> {
        crate::backends_zork_adapter::open_file(title)
    }

    #[cfg(feature = "zork")]
    pub fn save_file(&self, title: &str) -> Result<Option<String>, Error> {
        crate::backends_zork_adapter::save_file(title)
    }

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

    #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
    pub fn create_window(&self) -> Result<crate::backends_wasm_adapter::Window, Error> {
        crate::backends_wasm_adapter::create_window()
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
    pub fn create_button(&self, label: &str) -> Result<crate::backends_wasm_adapter::Button, Error> {
        crate::backends_wasm_adapter::create_button(label)
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
    pub fn create_label(&self, text: &str) -> Result<crate::backends_wasm_adapter::Label, Error> {
        crate::backends_wasm_adapter::create_label(text)
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
    pub fn create_box(&self, orientation: crate::backends_wasm_adapter::Orientation, spacing: i32) -> Result<crate::backends_wasm_adapter::BoxWidget, Error> {
        crate::backends_wasm_adapter::create_box(orientation, spacing)
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
    pub fn create_grid(&self) -> Result<crate::backends_wasm_adapter::Grid, Error> {
        crate::backends_wasm_adapter::create_grid()
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
    pub fn create_entry(&self) -> Result<crate::backends_wasm_adapter::Entry, Error> {
        crate::backends_wasm_adapter::create_entry()
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
    pub fn create_menu(&self) -> Result<crate::backends_wasm_adapter::Menu, Error> {
        crate::backends_wasm_adapter::create_menu()
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
    pub fn create_menubar(&self, model: &crate::backends_wasm_adapter::Menu, action_group: *mut c_void) -> Result<crate::backends_wasm_adapter::MenuBar, Error> {
        crate::backends_wasm_adapter::create_menubar(model, action_group)
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
    pub fn create_simple_action(&self, name: &str) -> Result<crate::backends_wasm_adapter::SimpleAction, Error> {
        crate::backends_wasm_adapter::create_simple_action(name)
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
    pub fn create_dialog(&self) -> Result<crate::backends_wasm_adapter::Dialog, Error> {
        crate::backends_wasm_adapter::create_dialog()
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
    pub fn create_dropdown(&self, items: &[&str]) -> Result<crate::backends_wasm_adapter::DropDown, Error> {
        crate::backends_wasm_adapter::create_dropdown(items)
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
    pub fn create_checkbutton(&self, label: &str) -> Result<crate::backends_wasm_adapter::CheckButton, Error> {
        crate::backends_wasm_adapter::create_checkbutton(label)
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
    pub fn create_radiobutton(&self, label: &str) -> Result<crate::backends_wasm_adapter::RadioButton, Error> {
        crate::backends_wasm_adapter::create_radiobutton(None, label)
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
    pub fn create_textview(&self) -> Result<crate::backends_wasm_adapter::TextView, Error> {
        crate::backends_wasm_adapter::create_textview()
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
    pub fn create_canvas(&self) -> Result<crate::backends_wasm_adapter::Canvas, Error> {
        crate::backends_wasm_adapter::create_canvas()
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
    pub fn create_overlay(&self) -> Result<crate::backends_wasm_adapter::Overlay, Error> {
        crate::backends_wasm_adapter::create_overlay()
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
    pub fn create_scrolled_window(&self) -> Result<crate::backends_wasm_adapter::ScrolledWindow, Error> {
        crate::backends_wasm_adapter::create_scrolled_window()
    }
    #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
    pub fn open_file(&self, _title: &str) -> Result<Option<String>, Error> {
        Ok(None) // File dialogs not available in WASM
    }

    #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
    pub fn save_file(&self, _title: &str) -> Result<Option<String>, Error> {
        Ok(None) // File dialogs not available in WASM
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

// ---------------------------------------------------------------------------
// High-level wrapper creation methods (return common types)
// ---------------------------------------------------------------------------

    /// Create a new Window and return a platform-independent handle.
    pub fn new_window(&self) -> Result<crate::common::Window, Error> {
        #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
        {
            let inner = crate::backends_gtk_adapter::create_window()?;
            return Ok(crate::common::Window { inner });
        }
        #[cfg(all(windows, not(feature = "zork")))]
        {
            let inner = crate::backends_nwg_adapter::create_window(&self.parent_cell)?;
            Ok(crate::common::Window { inner })
        }
        #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
        {
            let inner = crate::backends_wasm_adapter::create_window()?;
            Ok(crate::common::Window { inner })
        }
    }

    /// Create a new layout Box.
    pub fn new_box(&self, orientation: crate::common::Orientation, spacing: i32) -> Result<crate::common::WidgetBox, Error> {
        #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
        {
            let gtk_orient = match orientation {
                crate::common::Orientation::Horizontal => crate::backends_gtk_adapter::Orientation::Horizontal,
                crate::common::Orientation::Vertical => crate::backends_gtk_adapter::Orientation::Vertical,
            };
            let inner = crate::backends_gtk_adapter::create_box(gtk_orient, spacing)?;
            return Ok(crate::common::WidgetBox { inner });
        }
        #[cfg(all(windows, not(feature = "zork")))]
        {
            let nwg_orient = match orientation {
                crate::common::Orientation::Horizontal => crate::backends::nwg::Orientation::Horizontal,
                crate::common::Orientation::Vertical => crate::backends::nwg::Orientation::Vertical,
            };
            let parent = self.parent_cell.borrow().as_ref().copied().unwrap_or(std::ptr::null_mut());
            let inner = crate::backends_nwg_adapter::create_box(nwg_orient, spacing, parent)?;
            Ok(crate::common::WidgetBox { inner })
        }
        #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
        {
            let inner = crate::backends_wasm_adapter::create_box(orientation, spacing)?;
            Ok(crate::common::WidgetBox { inner })
        }
    }

    /// Create a new Label with the given text.
    pub fn new_label(&self, text: &str) -> Result<crate::common::Label, Error> {
        #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
        {
            let inner = crate::backends_gtk_adapter::create_label(text)?;
            return Ok(crate::common::Label { inner });
        }
        #[cfg(all(windows, not(feature = "zork")))]
        {
            let parent = self.parent_cell.borrow().as_ref().copied().unwrap_or(std::ptr::null_mut());
            let inner = crate::backends_nwg_adapter::create_label(parent)?;
            inner.set_text(text);
            Ok(crate::common::Label { inner })
        }
        #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
        {
            let inner = crate::backends_wasm_adapter::create_label(text)?;
            Ok(crate::common::Label { inner })
        }
    }

    /// Create a new text Entry.
    pub fn new_entry(&self) -> Result<crate::common::Entry, Error> {
        #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
        {
            let inner = crate::backends_gtk_adapter::create_entry()?;
            return Ok(crate::common::Entry { inner });
        }
        #[cfg(all(windows, not(feature = "zork")))]
        {
            let parent = self.parent_cell.borrow().as_ref().copied().unwrap_or(std::ptr::null_mut());
            let inner = crate::backends_nwg_adapter::create_entry(parent)?;
            Ok(crate::common::Entry { inner })
        }
        #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
        {
            let inner = crate::backends_wasm_adapter::create_entry()?;
            Ok(crate::common::Entry { inner })
        }
    }

    /// Create a new Canvas (custom drawing surface).
    pub fn new_canvas(&self) -> Result<crate::common::Canvas, Error> {
        #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
        {
            let inner = crate::backends_gtk_adapter::create_canvas()?;
            return Ok(crate::common::Canvas { inner });
        }
        #[cfg(all(windows, not(feature = "zork")))]
        {
            let parent = self.parent_cell.borrow().as_ref().copied().unwrap_or(std::ptr::null_mut());
            let inner = crate::backends_nwg_adapter::create_canvas(parent)?;
            Ok(crate::common::Canvas { inner })
        }
        #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
        {
            let inner = crate::backends_wasm_adapter::create_canvas()?;
            Ok(crate::common::Canvas { inner })
        }
    }

    /// Create a new Menu data model.
    pub fn new_menu(&self) -> Result<crate::common::Menu, Error> {
        #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
        {
            let inner = crate::backends_gtk_adapter::create_menu()?;
            return Ok(crate::common::Menu { inner });
        }
        #[cfg(all(windows, not(feature = "zork")))]
        {
            let inner = crate::backends_nwg_adapter::create_menu()?;
            Ok(crate::common::Menu { inner })
        }
        #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
        {
            let inner = crate::backends_pancurses_adapter::create_menu()?;
            Ok(crate::common::Menu { inner })
        }
        #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
        {
            let inner = crate::backends_wasm_adapter::create_menu()?;
            Ok(crate::common::Menu { inner })
        }
    }

    /// Create a new SimpleAction that will dispatch to the given name.
    /// On Windows the action is registered in the shared action registry.
    pub fn new_simple_action(&self, name: &str) -> Result<crate::common::SimpleAction, Error> {
        #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
        {
            let inner = crate::backends_gtk_adapter::create_simple_action(name)?;
            return Ok(crate::common::SimpleAction { inner });
        }
        #[cfg(all(windows, not(feature = "zork")))]
        {
            let inner = crate::backends_nwg_adapter::create_simple_action(name, self.action_registry.clone())?;
            Ok(crate::common::SimpleAction { inner })
        }
        #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
        {
            let inner = crate::backends_pancurses_adapter::create_simple_action(name)?;
            Ok(crate::common::SimpleAction { inner })
        }
        #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
        {
            let inner = crate::backends_wasm_adapter::create_simple_action(name)?;
            Ok(crate::common::SimpleAction { inner })
        }
    }

    /// Create a MenuBar from a Menu model.
    /// `action_group` – on GTK a `*mut c_void` pointer to a `GActionGroup`
    /// (pass null if not available); on Windows it is unused.
    pub fn new_menubar(&self, model: &crate::common::Menu, _action_group: *mut c_void) -> Result<crate::common::MenuBar, Error> {
        #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
        {
            let inner = unsafe { crate::backends_gtk_adapter::create_menubar(&model.inner, _action_group) }?;
            return Ok(crate::common::MenuBar { inner });
        }
        #[cfg(all(windows, not(feature = "zork")))]
        {
            let hwnd = self.parent_cell.borrow().as_ref().copied().unwrap_or(std::ptr::null_mut());
            let inner = crate::backends_nwg_adapter::create_menubar(&model.inner, hwnd, self.action_registry.clone())?;
            Ok(crate::common::MenuBar { inner })
        }
        #[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
        {
            let inner = crate::backends_pancurses_adapter::create_menubar(&model.inner, _action_group)?;
            Ok(crate::common::MenuBar { inner })
        }
        #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
        {
            let inner = crate::backends_wasm_adapter::create_menubar(&model.inner, action_group)?;
            Ok(crate::common::MenuBar { inner })
        }
    }

    /// Build a Menu tree from declarative SubmenuDef definitions.
    /// `label_prefix` is prepended to each submenu label (e.g. "\u{3164}" for
    /// GTK4 to prevent mnemonic accelerator assignment; pass "" for other backends).
    pub fn build_menu_model(&self, submenus: &[crate::common::SubmenuDef], label_prefix: &str) -> Result<crate::common::Menu, Error> {
        fn build_items(app: &App, items: &[crate::common::MenuItemDef], prefix: &str) -> Result<crate::common::Menu, Error> {
            let mut menu = app.new_menu()?;
            for item in items {
                if let Some(children) = item.submenu {
                    let sub = build_items(app, children, prefix)?;
                    menu.append_submenu(item.label, &sub);
                } else {
                    menu.append(item.label, &format!("{}.{}", prefix, item.action));
                }
            }
            Ok(menu)
        }
        let mut root = self.new_menu()?;
        for sm in submenus {
            let sub = build_items(self, sm.items, sm.prefix)?;
            root.append_submenu(sm.label, &sub);
        }
        Ok(root)
    }

    /// Create a new Dialog.
    pub fn new_dialog(&self) -> Result<crate::common::Dialog, Error> {
        #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
        {
            let inner = crate::backends_gtk_adapter::create_dialog()?;
            return Ok(crate::common::Dialog { inner });
        }
        #[cfg(all(windows, not(feature = "zork")))]
        {
            let inner = crate::backends_nwg_adapter::create_dialog(&self.parent_cell)?;
            Ok(crate::common::Dialog { inner })
        }
        #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
        {
            let inner = crate::backends_wasm_adapter::create_dialog()?;
            Ok(crate::common::Dialog { inner })
        }
    }

    /// Ensure the GTK application / action group exists (no-op on Windows).
    /// Returns an opaque `*mut c_void` that can be passed to `new_menubar`.
    pub fn ensure_action_group(&self) -> Result<*mut c_void, Error> {
        #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
        {
            if self.action_group.borrow().is_none() {
                let app = crate::backends_gtk_adapter::create_application()?;
                app.register()?;
                *self.action_group.borrow_mut() = Some(app);
            }
            Ok(self.action_group.borrow().as_ref().unwrap().as_ptr())
        }
        #[cfg(not(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs")))))]
        Ok(std::ptr::null_mut())
    }

    /// Register a SimpleAction with the action group.
    /// On GTK this adds the action to the GApplication; on Windows it is a no-op.
    pub fn register_action(&self, _action: &crate::common::SimpleAction) -> Result<(), Error> {
        #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
        if let Some(ref app) = *self.action_group.borrow() {
            app.add_action(&_action.inner)?;
        }
        #[cfg(not(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs")))))]
        {}
        Ok(())
    }

/// Run the backend main loop
    pub fn run(self) -> Result<(), Error> {
        let _ = std::fs::write("/tmp/corro_app_run.txt", "App::run() called\n");
        let boxed = self.inner.borrow_mut().take().ok_or_else(|| Error::Backend("App::run already called".into()))?;
        let _ = std::fs::write("/tmp/corro_backend_ptr.txt", &format!("backend={:#p}\n", &*boxed as *const _ as *const u8));
        boxed.run().map_err(|e| Error::Backend(format!("{}", e)))
    }

    /// Post a quit message to the backend's event loop.
    /// Safe to call from signal handlers and event callbacks.
    pub fn quit(&self) {
        #[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
        crate::backends_wasm_adapter::quit_main_loop();
        #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
        let _ = crate::backends_gtk_adapter::quit_main_loop();
        #[cfg(all(windows, not(feature = "zork")))]
        crate::backends_nwg_adapter::quit_main_loop();
    }

    /// Like quit() but returns the backend error, if any.
    #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
    pub fn try_quit(&self) -> Result<(), String> {
        crate::backends_gtk_adapter::quit_main_loop().map_err(|e| format!("{e}"))
    }
    #[cfg(not(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs")))))]
    pub fn try_quit(&self) -> Result<(), String> {
        Ok(())
    }

    /// Pump the backend's event loop for `count` blocking iterations.
    /// On GTK/Linux this processes pending main context events (frame
    /// clock ticks, redraws, configure events).  On other backends this
    /// is a no-op.
    ///
    /// Call after `queue_redraw()` to ensure the draw callback fires
    /// before entering the main loop, especially on virtual displays
    /// (Xvfb, WSL) where the GTK4 frame clock may not tick automatically.
    pub fn pump_events(&self, count: usize) {
        #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
        crate::backends_gtk_adapter::pump_main_context(count);
        let _ = count;
    }
}

impl From<Box<dyn crate::backends::BackendApp>> for App {
    fn from(b: Box<dyn crate::backends::BackendApp>) -> Self {
        App {
            inner: Rc::new(RefCell::new(Some(b))),
            #[cfg(all(windows, not(feature = "zork")))]
            parent_cell: Rc::new(RefCell::new(None)),
            #[cfg(all(windows, not(feature = "zork")))]
            action_registry: Rc::new(RefCell::new(HashMap::new())),
            #[cfg(any(feature = "gtk4-rs", all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs"))))]
            action_group: Rc::new(RefCell::new(None)),
        }
    }
}
