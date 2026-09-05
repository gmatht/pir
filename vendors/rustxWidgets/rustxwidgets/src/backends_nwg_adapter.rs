#[cfg(windows)]
mod nwg_adapter {
    use native_windows_gui as nwg;
    use crate::core::{Error, Widget, DrawContext};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::os::raw::c_void;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn set_window_pos(hwnd: *mut c_void, x: i32, y: i32, w: i32, h: i32) {
        unsafe {
            winapi::um::winuser::SetWindowPos(
                hwnd as winapi::shared::windef::HWND,
                std::ptr::null_mut(), x, y, w, h,
                winapi::um::winuser::SWP_NOZORDER | winapi::um::winuser::SWP_SHOWWINDOW,
            );
        }
    }

    // -- Window --

    pub struct Window {
        pub(crate) inner: Rc<nwg::Window>,
        pub(crate) _handler: Rc<nwg::EventHandler>,
        root_child: Rc<RefCell<Option<*mut c_void>>>,
        layout_cb: Rc<RefCell<Option<Box<dyn FnMut(i32, i32)>>>>,
        event_key_cb: Rc<RefCell<Option<Box<dyn FnMut(u32, u32) -> i32>>>>,
        close_cb: Rc<RefCell<Option<Box<dyn FnMut()>>>>,
    }

    impl Clone for Window {
        fn clone(&self) -> Self {
            Window {
                inner: self.inner.clone(),
                _handler: self._handler.clone(),
                root_child: self.root_child.clone(),
                layout_cb: self.layout_cb.clone(),
                event_key_cb: self.event_key_cb.clone(),
                close_cb: self.close_cb.clone(),
            }
        }
    }

    impl Widget for Window {
        fn raw_handle(&self) -> *mut c_void {
            self.inner.handle.hwnd().unwrap_or(std::ptr::null_mut()) as *mut c_void
        }
    }

    impl AsRef<*mut c_void> for Window {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.inner.handle.hwnd().unwrap_or(std::ptr::null_mut()) as *const _ as *const *mut c_void) }
        }
    }

    impl Window {
        pub fn set_title(&self, title: &str) { self.inner.set_text(title); }
        pub fn set_child(&self, child: &impl AsRef<*mut c_void>) {
            let ptr = *child.as_ref();
            if !ptr.is_null() {
                unsafe {
                    winapi::um::winuser::SetParent(ptr as _, self.hwnd() as _);
                }
                *self.root_child.borrow_mut() = Some(ptr);
                let hwnd = ptr;
                *self.layout_cb.borrow_mut() = Some(Box::new(move |w, h| {
                    set_window_pos(hwnd, 0, 0, w, h);
                }));
            }
        }
        pub fn set_child_box(&self, bx: &BoxWidget) {
            if !bx.hwnd.is_null() {
                unsafe {
                    winapi::um::winuser::SetParent(bx.hwnd as _, self.hwnd() as _);
                }
            }
            let hwnd = bx.hwnd;
            let bx = bx.clone();
            *self.layout_cb.borrow_mut() = Some(Box::new(move |w, h| {
                set_window_pos(hwnd, 0, 0, w, h);
                bx.layout(0, 0, w, h);
            }));
        }
        pub fn set_layout_cb<F: FnMut(i32, i32) + 'static>(&self, f: F) {
            *self.layout_cb.borrow_mut() = Some(Box::new(f));
        }
        pub fn set_default_size(&self, w: i32, h: i32) {
            let hwnd = self.inner.handle.hwnd().unwrap_or(std::ptr::null_mut());
            if hwnd != std::ptr::null_mut() {
                unsafe {
                    winapi::um::winuser::SetWindowPos(
                        hwnd as _,
                        std::ptr::null_mut(), 0, 0, w, h,
                        winapi::um::winuser::SWP_NOZORDER | winapi::um::winuser::SWP_NOMOVE,
                    );
                }
            }
        }
        pub fn present(&self) {
            let hwnd = self.inner.handle.hwnd().unwrap_or(std::ptr::null_mut());
            if hwnd != std::ptr::null_mut() {
                unsafe {
                    // Bring window to foreground so child controls can
                    // receive keyboard focus (required by SetFocus).
                    winapi::um::winuser::SetForegroundWindow(hwnd);
                    winapi::um::winuser::BringWindowToTop(hwnd);
                    let mut rect: winapi::shared::windef::RECT = std::mem::zeroed();
                    winapi::um::winuser::GetClientRect(hwnd, &mut rect);
                    let w = rect.right - rect.left;
                    let h = rect.bottom - rect.top;
                    if let Some(ref mut cb) = *self.layout_cb.borrow_mut() {
                        cb(w, h);
                    }
                }
            }
        }
        pub fn insert_action_group(&self, _name: &str, _group_ptr: *mut c_void) {}
        pub fn hwnd(&self) -> *mut c_void {
            self.inner.handle.hwnd().unwrap_or(std::ptr::null_mut()) as *mut c_void
        }
        pub fn queue_redraw(&self) {}
        pub fn on_event(&self, _cb: Box<dyn FnMut(*mut c_void) -> i32>) {}
        pub fn on_event_key(&self, cb: Box<dyn FnMut(u32, u32) -> i32>) {
            *self.event_key_cb.borrow_mut() = Some(cb);
        }
        pub fn on_close(&self, cb: Box<dyn FnMut()>) {
            *self.close_cb.borrow_mut() = Some(cb);
        }
        /// Schedule `cb` to run roughly every `ms` milliseconds, on the UI
        /// thread (WM_TIMER), until the window is destroyed. Mirrors the GTK
        /// `timeout_add_recurring` used by the GUI's periodic drain.
        pub fn timeout_add_recurring(&self, ms: u32, cb: Box<dyn FnMut()>) {
            let hwnd = self.hwnd();
            if hwnd.is_null() {
                return;
            }
            unsafe {
                winapi::um::winuser::SetTimer(hwnd as _, 1, ms, None);
            }
            let cb = std::rc::Rc::new(std::cell::RefCell::new(Some(cb)));
            let cb2 = cb.clone();
            let _ = nwg::bind_raw_event_handler(
                &nwg::ControlHandle::Hwnd(hwnd as _),
                0x7E000000,
                move |_h, msg, _w, _l| {
                    if msg == winapi::um::winuser::WM_TIMER {
                        if let Some(c) = cb2.borrow_mut().as_mut() {
                            c();
                        }
                    }
                    None
                },
            );
        }
    }

    pub fn create_window(parent_cell: &Rc<RefCell<Option<*mut c_void>>>) -> Result<Window, Error> {
        let (inner, handler) = crate::backends::nwg::create_window(parent_cell)
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        let root_child: Rc<RefCell<Option<*mut c_void>>> = Rc::new(RefCell::new(None));
        let layout_cb: Rc<RefCell<Option<Box<dyn FnMut(i32, i32)>>>> = Rc::new(RefCell::new(None));
        let event_key_cb: Rc<RefCell<Option<Box<dyn FnMut(u32, u32) -> i32>>>> = Rc::new(RefCell::new(None));
        let close_cb: Rc<RefCell<Option<Box<dyn FnMut()>>>> = Rc::new(RefCell::new(None));

        let hwnd = inner.handle.hwnd().unwrap_or(std::ptr::null_mut());
        if hwnd != std::ptr::null_mut() {
            // Bind raw WM_SIZE handler
            let cb = layout_cb.clone();
            static RAW_HANDLER_ID: AtomicUsize = AtomicUsize::new(0x10000000);
            let handler_id = RAW_HANDLER_ID.fetch_add(1, Ordering::SeqCst);
            nwg::bind_raw_event_handler(
                &nwg::ControlHandle::Hwnd(hwnd),
                handler_id,
                move |_h, msg, _w, l| {
                    if msg == winapi::um::winuser::WM_SIZE {
                        let w = (l & 0xFFFF) as i32;
                        let h = ((l >> 16) & 0xFFFF) as i32;
                        if let Some(ref mut cb) = *cb.borrow_mut() {
                            cb(w, h);
                        }
                    }
                    None
                },
            ).map_err(|e| Error::Backend(format!("{}", e)))?;

            // Bind raw WM_KEYDOWN/WM_SYSKEYDOWN handler for on_event_key.
            // The state parameter is a GDK-compatible modifier mask:
            //   bit 3 = Alt (MOD1_MASK)
            // Alt is detected from WM_SYSKEYDOWN.
            // If the callback returns 0 (not consumed), the message is forwarded
            // to the focused child window via PostMessage so the canvas or entry
            // raw handlers can process it.  After forwarding, we consume the
            // message (return Some(0)) so DefWindowProc does NOT process it.
            // This prevents WM_SYSKEYDOWN(Alt) from activating the menu bar,
            // which would steal focus from the window-level quit handler.
            let kcb = event_key_cb.clone();
            static KEY_HANDLER_ID: AtomicUsize = AtomicUsize::new(0x30000000);
            let key_id = KEY_HANDLER_ID.fetch_add(1, Ordering::SeqCst);
            let parent_hwnd = hwnd;
            nwg::bind_raw_event_handler(
                &nwg::ControlHandle::Hwnd(hwnd),
                key_id,
                move |_h, msg, w, l| {
                    if msg != winapi::um::winuser::WM_KEYDOWN && msg != winapi::um::winuser::WM_SYSKEYDOWN {
                        return None;
                    }
                    if let Some(ref mut f) = *kcb.borrow_mut() {
                        let mut state: u32 = 0;
                        if msg == winapi::um::winuser::WM_SYSKEYDOWN {
                            state |= 8; // GDK_MOD1_MASK (Alt)
                        }
                        if f(w as u32, state) != 0 {
                            return Some(0); // consumed, do not forward
                        }
                    }
                    // Forward keyboard messages to the correct child.
                    // The window-level raw handler consumes all key events
                    // to prevent DefWindowProc from activating the menu bar
                    // on WM_SYSKEYDOWN(Alt).  We manually post the message
                    // to the focused child (or a suitable descendant if no
                    // child has focus).
                    unsafe {
                        let focused = winapi::um::winuser::GetFocus();
                        if focused != std::ptr::null_mut() && focused != parent_hwnd {
                            // Forward to the focused child.
                            winapi::um::winuser::PostMessageW(focused, msg, w, l);
                        } else {
                            // No child has focus.  Post to all descendant
                            // windows recursively.  This ensures the formula
                            // entry (great-great-grandchild of the main window
                            // in the VBox -> formula bar -> entry hierarchy)
                            // receives keyboard messages even when no child
                            // has keyboard focus.
                            unsafe fn post_to_descendants(hwnd: winapi::shared::windef::HWND, msg: u32, w: winapi::shared::minwindef::WPARAM, l: winapi::shared::minwindef::LPARAM) {
                                let mut child = winapi::um::winuser::GetWindow(hwnd, winapi::um::winuser::GW_CHILD);
                                while child != std::ptr::null_mut() {
                                    winapi::um::winuser::PostMessageW(child, msg, w, l);
                                    post_to_descendants(child, msg, w, l);
                                    child = winapi::um::winuser::GetWindow(child, winapi::um::winuser::GW_HWNDNEXT);
                                }
                            }
                            post_to_descendants(parent_hwnd, msg, w, l);
                        }
                    }
                    Some(0) // consumed — prevent DefWindowProc from activating menu
                },
            ).map_err(|e| Error::Backend(format!("{}", e)))?;

            // Bind raw WM_CLOSE handler: replayer tests can post WM_CLOSE
            // directly to the main window as a reliable quit mechanism that
            // does not depend on the foreground-window focus state.
            // Calls the registered close callback (save_before_quit) before
            // quitting so that pending edits are committed to the output file.
            {
                let cb = close_cb.clone();
                static CLOSE_ID: AtomicUsize = AtomicUsize::new(0x40000000);
                let cid = CLOSE_ID.fetch_add(1, Ordering::SeqCst);
                nwg::bind_raw_event_handler(
                    &nwg::ControlHandle::Hwnd(hwnd), cid,
                    move |_h, msg, _w, _l| {
                        if msg == winapi::um::winuser::WM_CLOSE {
                            if let Some(ref mut f) = *cb.borrow_mut() {
                                f();
                            }
                            crate::backends::nwg::quit_main_loop();
                            Some(0)
                        } else {
                            None
                        }
                    },
                ).map_err(|e| Error::Backend(format!("{}", e)))?;
            }
        }

        Ok(Window { inner: Rc::new(inner), _handler: Rc::new(handler), root_child, layout_cb, event_key_cb, close_cb })
    }

    // -- Button --

    #[derive(Clone)]
    pub struct Button {
        pub(crate) inner: Rc<nwg::Button>,
        pub(crate) _handler: Rc<nwg::EventHandler>,
        pub(crate) click_cb: Rc<RefCell<Option<Box<dyn FnMut()>>>>,
        pub(crate) _raw_click_handler: Rc<RefCell<Vec<nwg::RawEventHandler>>>,
    }

    impl Button {
        pub fn on_click(&self, f: impl FnMut() + 'static) -> Result<u64, Error> {
            *self.click_cb.borrow_mut() = Some(Box::new(f));
            Ok(0)
        }
        pub fn emit_clicked(&self) -> Result<u64, Error> {
            if let Some(ref mut cb) = *self.click_cb.borrow_mut() { cb(); }
            Ok(0)
        }
        pub fn set_size_request(&self, w: i32, h: i32) {
            if let Some(hwnd) = self.inner.handle.hwnd() {
                unsafe {
                    winapi::um::winuser::SetWindowPos(
                        hwnd as _,
                        std::ptr::null_mut(), 0, 0, w, h,
                        winapi::um::winuser::SWP_NOZORDER | winapi::um::winuser::SWP_NOMOVE | winapi::um::winuser::SWP_SHOWWINDOW,
                    );
                }
            }
        }
        pub fn set_font_style(&self, weight: i32, italic: bool) {
            if let Some(hwnd) = self.inner.handle.hwnd() {
                unsafe {
                    let mut lf: winapi::um::wingdi::LOGFONTW = std::mem::zeroed();
                    lf.lfHeight = -13;
                    lf.lfWeight = weight;
                    lf.lfItalic = italic as u8;
                    lf.lfCharSet = winapi::um::wingdi::ANSI_CHARSET as u8;
                    lf.lfOutPrecision = winapi::um::wingdi::OUT_DEFAULT_PRECIS as u8;
                    lf.lfClipPrecision = winapi::um::wingdi::CLIP_DEFAULT_PRECIS as u8;
                    lf.lfQuality = winapi::um::wingdi::PROOF_QUALITY as u8;
                    lf.lfPitchAndFamily = winapi::um::wingdi::DEFAULT_PITCH as u8;
                    let face = "Segoe UI\0".encode_utf16().collect::<Vec<_>>();
                    let mut i = 0;
                    while i < face.len().min(32) {
                        lf.lfFaceName[i] = face[i];
                        i += 1;
                    }
                    let hfont = winapi::um::wingdi::CreateFontIndirectW(&lf);
                    if !hfont.is_null() {
                        winapi::um::winuser::SendMessageW(
                            hwnd as _,
                            winapi::um::winuser::WM_SETFONT,
                            hfont as usize,
                            1,
                        );
                    }
                }
            }
        }
        pub fn add_class(&self, _class: &str) {}
        pub fn remove_class(&self, _class: &str) {}
        pub fn set_text(&self, text: &str) { self.inner.set_text(text); }
        pub fn get_text(&self) -> Option<String> { Some(self.inner.text()) }
    }

    impl AsRef<*mut c_void> for Button {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.inner.handle.hwnd().unwrap_or(std::ptr::null_mut()) as *const _ as *const *mut c_void) }
        }
    }

    pub fn create_button(parent: *mut c_void, text: &str) -> Result<Button, Error> {
        let (inner, click_cb, handler) = crate::backends::nwg::create_button(parent, text)
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        let hwnd = inner.handle.hwnd().unwrap_or(std::ptr::null_mut());
        let raw_handlers: Rc<RefCell<Vec<nwg::RawEventHandler>>> = Rc::new(RefCell::new(Vec::new()));
        if hwnd != std::ptr::null_mut() {
            static RAW_BTN_CLICK_ID: AtomicUsize = AtomicUsize::new(0x60000000);
            let cb = click_cb.clone();
            let rid = RAW_BTN_CLICK_ID.fetch_add(1, Ordering::SeqCst);
            if let Ok(raw) = nwg::bind_raw_event_handler(
                &nwg::ControlHandle::Hwnd(hwnd), rid,
                move |_h, msg, w, l| {
                    match msg {
                        winapi::um::winuser::WM_LBUTTONUP => {
                            let mut rect = std::mem::MaybeUninit::zeroed();
                            let rc = unsafe {
                                winapi::um::winuser::GetClientRect(hwnd as _, rect.as_mut_ptr());
                                rect.assume_init()
                            };
                            let x = (l & 0xFFFF) as i16;
                            let y = ((l >> 16) & 0xFFFF) as i16;
                            if i32::from(x) <= rc.right && i32::from(y) <= rc.bottom {
                                if let Some(ref mut f) = *cb.borrow_mut() { f(); }
                            }
                            None
                        }
                        winapi::um::winuser::WM_KEYUP => {
                            if w == winapi::um::winuser::VK_SPACE as usize {
                                if let Some(ref mut f) = *cb.borrow_mut() { f(); }
                            }
                            None
                        }
                        _ => None
                    }
                },
            ) {
                raw_handlers.borrow_mut().push(raw);
            }
        }
        Ok(Button { inner: Rc::new(inner), _handler: Rc::new(handler), click_cb, _raw_click_handler: raw_handlers })
    }

    // -- Label --

    #[derive(Clone)]
    pub struct Label(pub(crate) Rc<nwg::Label>);

    impl Label {
        pub fn set_text(&self, text: &str) { self.0.set_text(text); }
        pub fn get_text(&self) -> Option<String> { Some(self.0.text()) }
        pub fn set_visible(&self, visible: bool) { self.0.set_visible(visible); }
        pub fn set_markup(&self, markup: &str) { self.0.set_text(markup); }
        pub fn set_margin_start(&self, _px: i32) {}
        pub fn set_margin_top(&self, _px: i32) {}
        pub fn set_halign(&self, _align: i32) {}
        pub fn set_valign(&self, _align: i32) {}
    }

    impl AsRef<*mut c_void> for Label {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.0.handle.hwnd().unwrap_or(std::ptr::null_mut()) as *const _ as *const *mut c_void) }
        }
    }

    impl Widget for Label {
        fn raw_handle(&self) -> *mut c_void {
            self.0.handle.hwnd().unwrap_or(std::ptr::null_mut()) as *mut c_void
        }
    }

    pub fn create_label(parent: *mut c_void) -> Result<Label, Error> {
        crate::backends::nwg::create_label(parent).map(|l| Label(Rc::new(l))).map_err(|e| Error::Backend(format!("{}", e)))
    }

    // -- BoxWidget --

    pub struct BoxWidget {
        pub(crate) frame: Option<Rc<nwg::Frame>>,
        pub(crate) hwnd: *mut c_void,
        pub(crate) children: Rc<RefCell<Vec<*mut c_void>>>,
        pub(crate) child_vexpand: Rc<RefCell<Vec<bool>>>,
        pub(crate) child_hexpand: Rc<RefCell<Vec<bool>>>,
        pub(crate) orientation: crate::backends::nwg::Orientation,
        pub(crate) spacing: i32,
        // Last known client size, refreshed on every WM_SIZE-driven layout so
        // that structural mutations (e.g. TabView::close_tab removing a child)
        // can re-flow the remaining children without waiting for a resize.
        pub(crate) last_w: std::cell::Cell<i32>,
        pub(crate) last_h: std::cell::Cell<i32>,
    }

    impl Clone for BoxWidget {
        fn clone(&self) -> Self {
            BoxWidget {
                frame: self.frame.clone(),
                hwnd: self.hwnd,
                children: self.children.clone(),
                child_vexpand: self.child_vexpand.clone(),
                child_hexpand: self.child_hexpand.clone(),
                orientation: self.orientation,
                spacing: self.spacing,
                last_w: std::cell::Cell::new(0),
                last_h: std::cell::Cell::new(0),
            }
        }
    }

    impl AsRef<*mut c_void> for BoxWidget {
        fn as_ref(&self) -> &*mut c_void {
            &self.hwnd
        }
    }

    impl Widget for BoxWidget {
        fn raw_handle(&self) -> *mut c_void {
            self.hwnd
        }
    }

    pub trait Appendable {
        fn collect_hwnds(&self) -> Vec<*mut c_void>;
    }

    impl<T: AsRef<*mut c_void>> Appendable for T {
        fn collect_hwnds(&self) -> Vec<*mut c_void> {
            let ptr = *self.as_ref();
            if ptr.is_null() { vec![] } else { vec![ptr] }
        }
    }

    impl BoxWidget {
        pub fn append(&self, child: &impl Appendable) {
            let hwnds = child.collect_hwnds();
            for &ptr in &hwnds {
                if !ptr.is_null() && !self.hwnd.is_null() {
                    unsafe {
                        winapi::um::winuser::SetParent(ptr as _, self.hwnd as _);
                    }
                }
            }
            let mut children = self.children.borrow_mut();
            let mut vex = self.child_vexpand.borrow_mut();
            let mut hex = self.child_hexpand.borrow_mut();
            children.extend(hwnds.into_iter().filter(|&c| !c.is_null()));
            vex.resize(children.len(), false);
            hex.resize(children.len(), false);
        }
        pub fn set_child_vexpand(&self, child: &impl AsRef<*mut c_void>, expand: bool) {
            let ptr = *child.as_ref();
            let children = self.children.borrow();
            let mut vex = self.child_vexpand.borrow_mut();
            if let Some(idx) = children.iter().position(|&c| c == ptr) {
                vex[idx] = expand;
            }
        }
        pub fn set_child_hexpand(&self, child: &impl AsRef<*mut c_void>, expand: bool) {
            let ptr = *child.as_ref();
            let children = self.children.borrow();
            let mut hex = self.child_hexpand.borrow_mut();
            if let Some(idx) = children.iter().position(|&c| c == ptr) {
                hex[idx] = expand;
            }
        }
        pub fn layout(&self, _x: i32, _y: i32, w: i32, h: i32) {
            self.last_w.set(w);
            self.last_h.set(h);
            let children = self.children.borrow();
            let vex = self.child_vexpand.borrow();
            let hex = self.child_hexpand.borrow();
            let n = children.len();
            if n == 0 { return; }
            let spacing_total = self.spacing * (n as i32 - 1).max(0);
            let (fixed_w, fixed_h) = match self.orientation {
                crate::backends::nwg::Orientation::Horizontal => {
                    (0, h - 10)
                }
                crate::backends::nwg::Orientation::Vertical => {
                    (w - 10, 0)
                }
            };
            let expand_count = match self.orientation {
                crate::backends::nwg::Orientation::Horizontal =>
                    hex.iter().filter(|&&e| e).count(),
                crate::backends::nwg::Orientation::Vertical =>
                    vex.iter().filter(|&&e| e).count(),
            };
            let mut desired_sizes: Vec<i32> = Vec::with_capacity(n);
            for i in 0..n {
                let is_expand = match self.orientation {
                    crate::backends::nwg::Orientation::Horizontal => hex[i],
                    crate::backends::nwg::Orientation::Vertical => vex[i],
                };
                if !is_expand {
                    let hardcoded = match self.orientation {
                        crate::backends::nwg::Orientation::Horizontal => 60,
                        crate::backends::nwg::Orientation::Vertical => 28,
                    };
                    unsafe {
                        let mut rect: winapi::shared::windef::RECT = std::mem::zeroed();
                        if winapi::um::winuser::GetWindowRect(children[i] as _, &mut rect) != 0 {
                            let sz = match self.orientation {
                                crate::backends::nwg::Orientation::Horizontal => rect.right - rect.left,
                                crate::backends::nwg::Orientation::Vertical => rect.bottom - rect.top,
                            };
                            desired_sizes.push(if sz > 10 { sz } else { hardcoded });
                        } else {
                            desired_sizes.push(hardcoded);
                        }
                    }
                } else {
                    desired_sizes.push(0);
                }
            }
            let fixed_total: i32 = desired_sizes.iter().sum();
            let mut pos = 5;
            let remaining = match self.orientation {
                crate::backends::nwg::Orientation::Horizontal => (w - 10 - fixed_total - spacing_total).max(0),
                crate::backends::nwg::Orientation::Vertical => (h - 10 - fixed_total - spacing_total).max(0),
            };
            let expand_size = if expand_count > 0 { remaining / expand_count as i32 } else { 0 };

            for i in 0..n {
                let child = children[i];
                let is_expand = match self.orientation {
                    crate::backends::nwg::Orientation::Horizontal => hex[i],
                    crate::backends::nwg::Orientation::Vertical => vex[i],
                };
                let (cw, ch) = match self.orientation {
                    crate::backends::nwg::Orientation::Horizontal => {
                        if is_expand { (expand_size, fixed_h) } else { (desired_sizes[i], fixed_h) }
                    }
                    crate::backends::nwg::Orientation::Vertical => {
                        if is_expand { (fixed_w, expand_size) } else { (fixed_w, desired_sizes[i]) }
                    }
                };
                match self.orientation {
                    crate::backends::nwg::Orientation::Horizontal => {
                        set_window_pos(child, pos, 5, cw, ch);
                        pos += cw + self.spacing;
                    }
                    crate::backends::nwg::Orientation::Vertical => {
                        set_window_pos(child, 5, pos, cw, ch);
                        pos += ch + self.spacing;
                    }
                }
            }
        }
        pub fn set_vexpand(&self, _expand: bool) {}
    pub fn set_visible(&self, v: bool) {
        if let Some(f) = &self.frame { f.set_visible(v); }
    }
        pub fn set_hexpand(&self, _expand: bool) {}
        pub fn remove(&self, child: &impl AsRef<*mut c_void>) {
            let ptr = *child.as_ref();
            if ptr.is_null() { return; }
            // Detach from the OS parent and hide, then drop from the layout bookkeeping.
            if !self.hwnd.is_null() {
                unsafe {
                    winapi::um::winuser::ShowWindow(ptr as _, winapi::um::winuser::SW_HIDE);
                    winapi::um::winuser::SetParent(ptr as _, std::ptr::null_mut());
                }
            }
            if let Some(idx) = self.children.borrow().iter().position(|&c| c == ptr) {
                self.children.borrow_mut().remove(idx);
                self.child_vexpand.borrow_mut().remove(idx);
                self.child_hexpand.borrow_mut().remove(idx);
            }
            // Re-flow remaining children immediately (don't wait for WM_SIZE).
            self.layout(0, 0, self.last_w.get(), self.last_h.get());
        }
    }

    pub fn create_box(orientation: crate::backends::nwg::Orientation, spacing: i32, parent: *mut c_void) -> Result<BoxWidget, Error> {
        let mut frame = nwg::Frame::default();
        if !parent.is_null() {
            nwg::Frame::builder()
                .flags(nwg::FrameFlags::NONE)
                .size((0, 0))
                .position((0, 0))
                .parent(&nwg::ControlHandle::Hwnd(parent as _))
                .build(&mut frame)
                .map_err(|e| Error::Backend(format!("{}", e)))?;
        }
        let hwnd = frame.handle.hwnd().unwrap_or(std::ptr::null_mut()) as *mut c_void;
        let children: Rc<RefCell<Vec<*mut c_void>>> = Rc::new(RefCell::new(Vec::new()));
        let child_vexpand: Rc<RefCell<Vec<bool>>> = Rc::new(RefCell::new(Vec::new()));
        let child_hexpand: Rc<RefCell<Vec<bool>>> = Rc::new(RefCell::new(Vec::new()));
        let bw = BoxWidget {
            frame: Some(Rc::new(frame)), hwnd,
            children: children.clone(),
            child_vexpand: child_vexpand.clone(),
            child_hexpand: child_hexpand.clone(),
            orientation, spacing,
            last_w: std::cell::Cell::new(0),
            last_h: std::cell::Cell::new(0),
        };
        // Auto-layout on WM_SIZE — now shares children via Rc<RefCell>
        if hwnd != std::ptr::null_mut() {
            let bw2 = bw.clone();
            static BOX_SIZE_ID: AtomicUsize = AtomicUsize::new(0xB0000000);
            let id = BOX_SIZE_ID.fetch_add(1, Ordering::SeqCst);
            let _ = nwg::bind_raw_event_handler(
                &nwg::ControlHandle::Hwnd(hwnd as _), id,
                move |_h, msg, _w, l| {
                    if msg == winapi::um::winuser::WM_SIZE {
                        let w = (l & 0xFFFF) as i32;
                        let h = ((l >> 16) & 0xFFFF) as i32;
                        bw2.layout(0, 0, w, h);
                    }
                    None
                },
            );
        }
        Ok(bw)
    }

    // -- Grid --

    pub struct Grid;

    impl Grid {
        pub fn attach(&self, child: &impl AsRef<*mut c_void>, left: i32, top: i32, width: i32, height: i32) {
            set_window_pos(*child.as_ref(), left, top, width, height);
        }
    }

    pub fn create_grid() -> Result<Grid, Error> { Ok(Grid) }

    // -- Entry --

    pub struct Entry {
        pub(crate) inner: Rc<nwg::TextInput>,
        pub(crate) _handler: Rc<nwg::EventHandler>,
        pub(crate) changed_cb: Rc<RefCell<Option<Box<dyn FnMut()>>>>,
        focus_in_cb: Rc<RefCell<Option<Box<dyn FnMut(*mut c_void) -> i32>>>>,
        focus_out_cb: Rc<RefCell<Option<Box<dyn FnMut(*mut c_void) -> i32>>>>,
        _focus_in_handler: Option<nwg::RawEventHandler>,
        _focus_out_handler: Option<nwg::RawEventHandler>,
        key_cb: Rc<RefCell<Option<Box<dyn FnMut(u32) -> bool>>>>,
        _key_handler: Option<nwg::RawEventHandler>,
        pub(crate) pos_x: std::cell::Cell<i32>,
        pub(crate) pos_y: std::cell::Cell<i32>,
    }

    impl Clone for Entry {
        fn clone(&self) -> Self {
            Entry {
                inner: self.inner.clone(),
                _handler: self._handler.clone(),
                changed_cb: self.changed_cb.clone(),
                focus_in_cb: self.focus_in_cb.clone(),
                focus_out_cb: self.focus_out_cb.clone(),
                _focus_in_handler: None,
                _focus_out_handler: None,
                key_cb: self.key_cb.clone(),
                _key_handler: None,
                pos_x: std::cell::Cell::new(self.pos_x.get()),
                pos_y: std::cell::Cell::new(self.pos_y.get()),
            }
        }
    }

    impl Entry {
        pub fn set_text(&self, text: &str) { self.inner.set_text(text); }
        pub fn get_text(&self) -> Option<String> { Some(self.inner.text()) }
        pub fn connect_changed(&self, f: impl FnMut() + 'static) -> Result<u64, Error> {
            *self.changed_cb.borrow_mut() = Some(Box::new(f));
            Ok(0)
        }
        pub fn set_width_chars(&self, n: i32) {
            if let Some(hwnd) = self.inner.handle.hwnd() {
                unsafe {
                    let mut rect: winapi::shared::windef::RECT = std::mem::zeroed();
                    if winapi::um::winuser::GetWindowRect(hwnd as _, &mut rect) != 0 {
                        let h = rect.bottom - rect.top;
                        winapi::um::winuser::SetWindowPos(
                            hwnd as _, std::ptr::null_mut(), 0, 0, n * 8, h,
                            winapi::um::winuser::SWP_NOZORDER | winapi::um::winuser::SWP_NOMOVE | winapi::um::winuser::SWP_SHOWWINDOW,
                        );
                    }
                }
            }
        }
        pub fn set_size_request(&self, w: i32, h: i32) {
            if let Some(hwnd) = self.inner.handle.hwnd() {
                unsafe {
                    winapi::um::winuser::SetWindowPos(
                        hwnd as _, std::ptr::null_mut(), 0, 0, w, h,
                        winapi::um::winuser::SWP_NOZORDER | winapi::um::winuser::SWP_NOMOVE | winapi::um::winuser::SWP_SHOWWINDOW,
                    );
                }
            }
        }
        pub fn set_visible(&self, v: bool) { self.inner.set_visible(v); }
        pub fn grab_focus(&self) {
            let _ = self.inner.set_focus();
            if let Some(hwnd) = self.inner.handle.hwnd() {
                unsafe {
                    winapi::um::winuser::ShowWindow(hwnd as _, winapi::um::winuser::SW_SHOW);
                    winapi::um::winuser::SetWindowPos(
                        hwnd as _, winapi::um::winuser::HWND_TOP,
                        0, 0, 0, 0,
                        winapi::um::winuser::SWP_NOMOVE | winapi::um::winuser::SWP_NOSIZE | winapi::um::winuser::SWP_SHOWWINDOW,
                    );
                    winapi::um::winuser::RedrawWindow(
                        hwnd as _,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        winapi::um::winuser::RDW_INVALIDATE | winapi::um::winuser::RDW_UPDATENOW | winapi::um::winuser::RDW_ERASE | winapi::um::winuser::RDW_FRAME,
                    );
                    let parent = winapi::um::winuser::GetParent(hwnd as _);
                    if !parent.is_null() {
                        winapi::um::winuser::RedrawWindow(
                            parent,
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                            winapi::um::winuser::RDW_INVALIDATE | winapi::um::winuser::RDW_UPDATENOW | winapi::um::winuser::RDW_ALLCHILDREN | winapi::um::winuser::RDW_FRAME,
                        );
                    }
                }
            }
        }
        pub fn on_key(&self, f: Box<dyn FnMut(u32) -> bool>) {
            *self.key_cb.borrow_mut() = Some(f);
        }
        pub fn set_hexpand(&self, _expand: bool) {}
        pub fn set_vexpand(&self, _expand: bool) {}
        pub fn add_class(&self, _class: &str) {}
        pub fn remove_class(&self, _class: &str) {}
        pub fn set_margin_start(&self, px: i32) {
            self.pos_x.set(px);
            if let Some(hwnd) = self.inner.handle.hwnd() {
                unsafe {
                    let result = winapi::um::winuser::SetWindowPos(
                        hwnd as _, std::ptr::null_mut(), px, self.pos_y.get(), 0, 0,
                        winapi::um::winuser::SWP_NOZORDER | winapi::um::winuser::SWP_NOSIZE | winapi::um::winuser::SWP_SHOWWINDOW,
                    );
                }
            } else {
            }
        }
        pub fn set_margin_top(&self, px: i32) {
            self.pos_y.set(px);
            if let Some(hwnd) = self.inner.handle.hwnd() {
                unsafe {
                    let result = winapi::um::winuser::SetWindowPos(
                        hwnd as _, std::ptr::null_mut(), self.pos_x.get(), px, 0, 0,
                        winapi::um::winuser::SWP_NOZORDER | winapi::um::winuser::SWP_NOSIZE | winapi::um::winuser::SWP_SHOWWINDOW,
                    );
                }
            } else {
            }
        }
        pub fn set_halign(&self, _align: i32) {}
        pub fn set_valign(&self, _align: i32) {}
        pub fn on_key_raw(&self, cb: Box<dyn FnMut(u32, u32) -> bool>) {
            let mut cb = cb;
            *self.key_cb.borrow_mut() = Some(Box::new(move |k: u32| -> bool { cb(k, 0) }));
        }
        pub fn connect_activate(&self, _f: impl FnMut(*mut c_void) + 'static) -> Result<u64, Error> { Ok(0) }
        pub fn connect_focus_in_event(&self, f: impl FnMut(*mut c_void) -> i32 + 'static) -> Result<u64, Error> {
            *self.focus_in_cb.borrow_mut() = Some(Box::new(f));
            Ok(0)
        }
        pub fn connect_focus_out_event(&self, f: impl FnMut(*mut c_void) -> i32 + 'static) -> Result<u64, Error> {
            *self.focus_out_cb.borrow_mut() = Some(Box::new(f));
            Ok(0)
        }
    }

    impl AsRef<*mut c_void> for Entry {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.inner.handle.hwnd().unwrap_or(std::ptr::null_mut()) as *const _ as *const *mut c_void) }
        }
    }

    impl Widget for Entry {
        fn raw_handle(&self) -> *mut c_void {
            self.inner.handle.hwnd().unwrap_or(std::ptr::null_mut()) as *mut c_void
        }
    }

    pub fn create_entry(parent: *mut c_void) -> Result<Entry, Error> {
        let (inner, changed_cb, handler) = crate::backends::nwg::create_entry(parent)
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        let focus_in_cb: Rc<RefCell<Option<Box<dyn FnMut(*mut c_void) -> i32>>>> = Rc::new(RefCell::new(None));
        let focus_out_cb: Rc<RefCell<Option<Box<dyn FnMut(*mut c_void) -> i32>>>> = Rc::new(RefCell::new(None));
        let key_cb: Rc<RefCell<Option<Box<dyn FnMut(u32) -> bool>>>> = Rc::new(RefCell::new(None));
        let hwnd = inner.handle.hwnd().unwrap_or(std::ptr::null_mut());
        if hwnd != std::ptr::null_mut() {
            unsafe {
                let ex = winapi::um::winuser::GetWindowLongW(
                    hwnd as _, winapi::um::winuser::GWL_EXSTYLE);
                winapi::um::winuser::SetWindowLongW(
                    hwnd as _, winapi::um::winuser::GWL_EXSTYLE,
                    ex | winapi::um::winuser::WS_EX_CLIENTEDGE as i32);
                winapi::um::winuser::SetWindowPos(
                    hwnd as _, std::ptr::null_mut(), 0, 0, 0, 0,
                    winapi::um::winuser::SWP_NOMOVE | winapi::um::winuser::SWP_NOSIZE
                    | winapi::um::winuser::SWP_NOZORDER | winapi::um::winuser::SWP_FRAMECHANGED);
            }
        }

        let _focus_in_handler = if hwnd != std::ptr::null_mut() {
            let cb = focus_in_cb.clone();
            static FOCUS_IN_ID: AtomicUsize = AtomicUsize::new(0x40000000);
            let id = FOCUS_IN_ID.fetch_add(1, Ordering::SeqCst);
            nwg::bind_raw_event_handler(
                &nwg::ControlHandle::Hwnd(hwnd), id,
                move |_h, msg, _w, _l| {
                    if msg == winapi::um::winuser::WM_SETFOCUS {
                        if let Some(ref mut f) = *cb.borrow_mut() { f(std::ptr::null_mut()); }
                    }
                    None
                },
            ).ok()
        } else { None };

        let _focus_out_handler = if hwnd != std::ptr::null_mut() {
            let cb = focus_out_cb.clone();
            static FOCUS_OUT_ID: AtomicUsize = AtomicUsize::new(0x50000000);
            let id = FOCUS_OUT_ID.fetch_add(1, Ordering::SeqCst);
            nwg::bind_raw_event_handler(
                &nwg::ControlHandle::Hwnd(hwnd), id,
                move |_h, msg, _w, _l| {
                    if msg == winapi::um::winuser::WM_KILLFOCUS {
                        if let Some(ref mut f) = *cb.borrow_mut() { f(std::ptr::null_mut()); }
                    }
                    None
                },
            ).ok()
        } else { None };

        let _key_handler = if hwnd != std::ptr::null_mut() {
            let kc = key_cb.clone();
            static ENTRY_KEY_ID: AtomicUsize = AtomicUsize::new(0x60000000);
            let id = ENTRY_KEY_ID.fetch_add(1, Ordering::SeqCst);
            nwg::bind_raw_event_handler(
                &nwg::ControlHandle::Hwnd(hwnd), id,
                move |_h, msg, w, _l| {
                    // Consume WM_CHAR for Enter/Escape to prevent beep
                    if msg == winapi::um::winuser::WM_CHAR {
                        let c = (w & 0xFF) as u8;
                        if c == 0x0D || c == 0x1B {
                            return Some(0);
                        }
                        return None;
                    }
                    if msg == winapi::um::winuser::WM_KEYDOWN || msg == winapi::um::winuser::WM_SYSKEYDOWN {
                        if let Some(ref mut f) = *kc.borrow_mut() {
                            if f(w as u32) { return Some(0); }
                        }
                    }
                    None
                },
            ).ok()
        } else { None };

        Ok(Entry { inner: Rc::new(inner), _handler: Rc::new(handler), changed_cb, focus_in_cb, focus_out_cb, _focus_in_handler, _focus_out_handler, key_cb, _key_handler, pos_x: std::cell::Cell::new(0), pos_y: std::cell::Cell::new(0) })
    }

    // ========== DropDown ==========

    #[derive(Clone)]
    pub struct DropDown {
        pub(crate) inner: Rc<nwg::ComboBox<String>>,
        pub(crate) _handler: Rc<nwg::EventHandler>,
    }

    impl DropDown {
        pub fn set_active(&self, index: Option<u32>) {
            self.inner.set_selection(index.map(|i| i as usize));
        }
        pub fn active(&self) -> Option<u32> {
            self.inner.selection().map(|i| i as u32)
        }
        pub fn get_active(&self) -> u32 {
            self.inner.selection().unwrap_or(0) as u32
        }
        pub fn connect_changed(&self, _f: impl FnMut() + 'static) -> Result<u64, Error> {
            Ok(0)
        }
        pub fn set_hexpand(&self, _expand: bool) {}
        pub fn set_vexpand(&self, _expand: bool) {}
    }

    impl AsRef<*mut c_void> for DropDown {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.inner.handle.hwnd().unwrap_or(std::ptr::null_mut()) as *const _ as *const *mut c_void) }
        }
    }

    pub fn create_dropdown(parent: *mut c_void, items: &[&str]) -> Result<DropDown, Error> {
        let (inner, handler) = crate::backends::nwg::create_dropdown(parent, items)
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(DropDown { inner: Rc::new(inner), _handler: Rc::new(handler) })
    }

    // ========== CheckButton ==========

    #[derive(Clone)]
    pub struct CheckButton {
        pub(crate) inner: Rc<nwg::CheckBox>,
        pub(crate) _handler: Rc<nwg::EventHandler>,
    }

    impl CheckButton {
        pub fn set_active(&self, active: bool) {
            self.inner.set_check_state(if active { nwg::CheckBoxState::Checked } else { nwg::CheckBoxState::Unchecked });
        }
        pub fn is_active(&self) -> bool {
            matches!(self.inner.check_state(), nwg::CheckBoxState::Checked)
        }
        pub fn set_label(&self, label: &str) { self.inner.set_text(label); }
        pub fn get_label(&self) -> Option<String> { Some(self.inner.text()) }
        pub fn connect_toggled(&self, _f: impl FnMut() + 'static) -> Result<u64, Error> {
            Ok(0)
        }
    }

    impl AsRef<*mut c_void> for CheckButton {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.inner.handle.hwnd().unwrap_or(std::ptr::null_mut()) as *const _ as *const *mut c_void) }
        }
    }

    pub fn create_checkbutton(parent: *mut c_void) -> Result<CheckButton, Error> {
        let (inner, handler) = crate::backends::nwg::create_checkbox(parent, "Check")
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(CheckButton { inner: Rc::new(inner), _handler: Rc::new(handler) })
    }

    // ========== RadioButton ==========

    #[derive(Clone)]
    pub struct RadioButton {
        pub(crate) inner: Rc<nwg::RadioButton>,
        pub(crate) _handler: Rc<nwg::EventHandler>,
    }

    impl RadioButton {
        pub fn set_active(&self, active: bool) {
            self.inner.set_check_state(if active { nwg::RadioButtonState::Checked } else { nwg::RadioButtonState::Unchecked });
        }
        pub fn is_active(&self) -> bool {
            self.inner.check_state() == nwg::RadioButtonState::Checked
        }
        pub fn set_label(&self, label: &str) { self.inner.set_text(label); }
        pub fn get_label(&self) -> Option<String> { Some(self.inner.text()) }
        pub fn connect_toggled(&self, _f: impl FnMut() + 'static) -> Result<u64, Error> {
            Ok(0)
        }
    }

    impl AsRef<*mut c_void> for RadioButton {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.inner.handle.hwnd().unwrap_or(std::ptr::null_mut()) as *const _ as *const *mut c_void) }
        }
    }

    pub fn create_radiobutton(parent: *mut c_void) -> Result<RadioButton, Error> {
        let (inner, handler) = crate::backends::nwg::create_radiobutton(parent, "Radio", false)
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(RadioButton { inner: Rc::new(inner), _handler: Rc::new(handler) })
    }

    // ========== TextView ==========

    #[derive(Clone)]
    pub struct TextView {
        pub(crate) inner: Rc<nwg::TextBox>,
        pub(crate) changed_cb: Rc<RefCell<Option<Box<dyn FnMut()>>>>,
        pub(crate) _handler: Rc<nwg::EventHandler>,
    }

    impl TextView {
        pub fn get_buffer(&self) -> &RefCell<Option<Box<dyn FnMut()>>> { &self.changed_cb }
        pub fn set_text(&self, text: &str) { self.inner.set_text(text); }
        pub fn get_text(&self) -> Option<String> { Some(self.inner.text()) }
        pub fn set_editable(&self, editable: bool) { self.inner.set_readonly(!editable); }
        pub fn set_size_request(&self, _w: i32, _h: i32) {}
        pub fn set_wrap_mode(&self, _mode: i32) {}
        /// Append `markup` as plain text. The nwg backend has no Pango markup
        /// engine, so tags/entities are stripped (the GTK backend keeps colour).
        pub fn set_markup(&self, markup: &str) {
            let cur = self.inner.text();
            self.inner.set_text(&format!("{}{}\n", cur, strip_pango(markup)));
        }
    }

    impl AsRef<*mut c_void> for TextView {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.inner.handle.hwnd().unwrap_or(std::ptr::null_mut()) as *const _ as *const *mut c_void) }
        }
    }

    pub fn create_textview(parent: *mut c_void) -> Result<TextView, Error> {
        let (inner, changed_cb, handler) = crate::backends::nwg::create_textview(parent)
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(TextView { inner: Rc::new(inner), changed_cb, _handler: Rc::new(handler) })
    }

    // ========== Dialog ==========

    pub struct Dialog {
        pub(crate) inner: Rc<nwg::Window>,
        pub(crate) buttons: Rc<RefCell<Vec<(nwg::Button, nwg::EventHandler)>>>,
        pub(crate) response_cb: Rc<RefCell<Option<Box<dyn FnMut(i32)>>>>,
        pub(crate) _handler: Rc<nwg::EventHandler>,
        layout_cb: Rc<RefCell<Vec<Box<dyn FnMut(i32, i32)>>>>,
    }

    impl Clone for Dialog {
        fn clone(&self) -> Self {
            Dialog {
                inner: self.inner.clone(),
                buttons: self.buttons.clone(),
                response_cb: self.response_cb.clone(),
                _handler: self._handler.clone(),
                layout_cb: self.layout_cb.clone(),
            }
        }
    }

    impl Dialog {
        pub fn run(&self) -> i32 { 0 }
        pub fn set_title(&self, title: &str) { self.inner.set_text(title); }
        pub fn set_size_request(&self, _w: i32, _h: i32) {}
        pub fn set_default_size(&self, w: i32, h: i32) {
            let hwnd = self.inner.handle.hwnd().unwrap_or(std::ptr::null_mut());
            if hwnd != std::ptr::null_mut() {
                unsafe {
                    winapi::um::winuser::SetWindowPos(
                        hwnd as _, std::ptr::null_mut(), 0, 0, w, h,
                        winapi::um::winuser::SWP_NOZORDER | winapi::um::winuser::SWP_NOMOVE,
                    );
                }
            }
        }
        pub fn present(&self) {
            self.inner.set_visible(true);
            let hwnd = self.inner.handle.hwnd().unwrap_or(std::ptr::null_mut());
            if hwnd != std::ptr::null_mut() {
                unsafe {
                    winapi::um::winuser::SetForegroundWindow(hwnd as _);
                    let mut rect: winapi::shared::windef::RECT = std::mem::zeroed();
                    winapi::um::winuser::GetClientRect(hwnd, &mut rect);
                    let w = rect.right - rect.left;
                    let h = rect.bottom - rect.top;
                    for cb in self.layout_cb.borrow_mut().iter_mut() {
                        cb(w, h);
                    }
                    winapi::um::winuser::RedrawWindow(
                        hwnd as _,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        winapi::um::winuser::RDW_INVALIDATE | winapi::um::winuser::RDW_UPDATENOW | winapi::um::winuser::RDW_ALLCHILDREN | winapi::um::winuser::RDW_ERASE | winapi::um::winuser::RDW_FRAME,
                    );
                }
            }
        }
        pub fn append_content_area(&self, child: &impl Appendable) {
            for &ptr in &child.collect_hwnds() {
                if !ptr.is_null() {
                    unsafe {
                        let dlg = self.inner.handle.hwnd().unwrap_or(std::ptr::null_mut());
                        winapi::um::winuser::SetParent(ptr as _, dlg as _);
                        winapi::um::winuser::ShowWindow(ptr as _, winapi::um::winuser::SW_SHOW);
                        winapi::um::winuser::SetWindowPos(
                            ptr as _, winapi::um::winuser::HWND_TOP,
                            0, 0, 0, 0,
                            winapi::um::winuser::SWP_NOMOVE | winapi::um::winuser::SWP_NOSIZE,
                        );
                    }
                    let child_hwnd = ptr;
                    self.layout_cb.borrow_mut().push(Box::new(move |w, h| {
                        set_window_pos(child_hwnd, 0, 0, w, h);
                    }));
                }
            }
        }
        pub fn add_button(&self, text: &str, response_id: i32) {
            let hwnd = self.inner.handle.hwnd().unwrap_or(std::ptr::null_mut());
            let cb = self.response_cb.clone();
            let result = crate::backends::nwg::create_dialog_button(hwnd as *mut c_void, text, response_id, cb);
            if let Ok(btn) = result {
                self.buttons.borrow_mut().push(btn);
            }
        }
        pub fn connect_response<F: FnMut(i32) + 'static>(&self, f: F) -> Result<u64, Error> {
            *self.response_cb.borrow_mut() = Some(Box::new(f));
            Ok(0)
        }
        pub fn close(&self) {
            if let Some(hwnd) = self.inner.handle.hwnd() {
                unsafe {
                    winapi::um::winuser::ShowWindow(hwnd as _, winapi::um::winuser::SW_HIDE);
                }
            }
        }
    }

    impl AsRef<*mut c_void> for Dialog {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.inner.handle.hwnd().unwrap_or(std::ptr::null_mut()) as *const _ as *const *mut c_void) }
        }
    }

    impl Widget for Dialog {
        fn raw_handle(&self) -> *mut c_void {
            self.inner.handle.hwnd().unwrap_or(std::ptr::null_mut()) as *mut c_void
        }
    }

    impl Dialog {
        pub fn set_visible(&self, v: bool) {
            unsafe {
                winapi::um::winuser::ShowWindow(self.inner.handle.hwnd().unwrap_or(std::ptr::null_mut()) as _, if v { winapi::um::winuser::SW_SHOW } else { winapi::um::winuser::SW_HIDE });
            }
        }
    }

    pub fn create_dialog(
        parent_cell: &Rc<RefCell<Option<*mut c_void>>>,
    ) -> Result<Dialog, Error> {
        let response_cb: Rc<RefCell<Option<Box<dyn FnMut(i32)>>>> = Rc::new(RefCell::new(None));
        let btn_cb = response_cb.clone();
        let (inner, _, handler) = crate::backends::nwg::create_dialog(parent_cell, btn_cb)
            .map_err(|e| Error::Backend(format!("{}", e)))?;

        let layout_cb: Rc<RefCell<Vec<Box<dyn FnMut(i32, i32)>>>> = Rc::new(RefCell::new(Vec::new()));

        // Bind raw WM_SIZE handler for dialog
        let dlg_hwnd = inner.handle.hwnd().unwrap_or(std::ptr::null_mut());
        if dlg_hwnd != std::ptr::null_mut() {
            let cb = layout_cb.clone();
            static DIALOG_RAW_HANDLER_ID: AtomicUsize = AtomicUsize::new(0x20000000);
            let handler_id = DIALOG_RAW_HANDLER_ID.fetch_add(1, Ordering::SeqCst);
            nwg::bind_raw_event_handler(
                &nwg::ControlHandle::Hwnd(dlg_hwnd),
                handler_id,
                move |_h, msg, _w, l| {
                    if msg == winapi::um::winuser::WM_SIZE {
                        let w = (l & 0xFFFF) as i32;
                        let h = ((l >> 16) & 0xFFFF) as i32;
                        for cb_item in cb.borrow_mut().iter_mut() {
                            cb_item(w, h);
                        }
                    }
                    None
                },
            ).map_err(|e| Error::Backend(format!("{}", e)))?;
        }

        Ok(Dialog { inner: Rc::new(inner), buttons: Rc::new(RefCell::new(Vec::new())), response_cb, _handler: Rc::new(handler), layout_cb })
    }

    pub fn create_dialog_button(
        parent: *mut c_void,
        text: &str,
        response_id: i32,
        cb: Rc<RefCell<Option<Box<dyn FnMut(i32)>>>>,
    ) -> Result<(nwg::Button, nwg::EventHandler), Error> {
        crate::backends::nwg::create_dialog_button(parent, text, response_id, cb)
            .map_err(|e| Error::Backend(format!("{}", e)))
    }

    // ========== GDI DrawContext ==========

    pub struct NwgDrawContext {
        hdc: winapi::shared::windef::HDC,
        w: i32,
        h: i32,
    }

    impl NwgDrawContext {
        fn make_font(name: &str, size: f64, weight: i32, italic: bool) -> winapi::shared::windef::HFONT {
            unsafe {
                let is_mono = name.eq_ignore_ascii_case("monospace");
                let face = if is_mono { "Courier New" } else { name };
                let wide_name: Vec<u16> = face.encode_utf16().chain(std::iter::once(0)).collect();
                let pitch = if is_mono { winapi::um::wingdi::FF_MODERN } else { 0 };
                winapi::um::wingdi::CreateFontW(
                    -(size.abs() as i32), 0, 0, 0, weight as i32, italic as u32, 0, 0,
                    winapi::um::wingdi::ANSI_CHARSET,
                    winapi::um::wingdi::OUT_DEFAULT_PRECIS,
                    winapi::um::wingdi::CLIP_DEFAULT_PRECIS,
                    winapi::um::wingdi::PROOF_QUALITY,
                    winapi::um::wingdi::DEFAULT_PITCH | pitch,
                    wide_name.as_ptr(),
                )
            }
        }
    }

    impl DrawContext for NwgDrawContext {
        fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, g: f64, b: f64, _a: f64) {
            unsafe {
                let color: u32 = winapi::um::wingdi::RGB(
                    (r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8);
                let brush = winapi::um::wingdi::CreateSolidBrush(color);
                if !brush.is_null() {
                    let mut rect = winapi::shared::windef::RECT {
                        left: x as i32, top: y as i32,
                        right: (x + w) as i32, bottom: (y + h) as i32,
                    };
                    winapi::um::winuser::FillRect(self.hdc, &mut rect, brush);
                    winapi::um::wingdi::DeleteObject(brush as _);
                }
            }
        }
        fn stroke_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, g: f64, b: f64, _a: f64, lw: f64) {
            unsafe {
                let color: u32 = winapi::um::wingdi::RGB(
                    (r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8);
                let pen = winapi::um::wingdi::CreatePen(winapi::um::wingdi::PS_SOLID as i32, lw as i32, color);
                if !pen.is_null() {
                    let old_pen = winapi::um::wingdi::SelectObject(self.hdc, pen as _);
                    if w == 0.0 && h != 0.0 {
                        winapi::um::wingdi::MoveToEx(self.hdc, x as i32, y as i32, std::ptr::null_mut());
                        winapi::um::wingdi::LineTo(self.hdc, x as i32, (y + h) as i32);
                    } else if h == 0.0 && w != 0.0 {
                        winapi::um::wingdi::MoveToEx(self.hdc, x as i32, y as i32, std::ptr::null_mut());
                        winapi::um::wingdi::LineTo(self.hdc, (x + w) as i32, y as i32);
                    } else {
                        let null_brush = winapi::um::wingdi::GetStockObject(winapi::um::wingdi::NULL_BRUSH as i32);
                        let old_brush = winapi::um::wingdi::SelectObject(self.hdc, null_brush);
                        winapi::um::wingdi::Rectangle(self.hdc, x as i32, y as i32, (x + w) as i32, (y + h) as i32);
                        winapi::um::wingdi::SelectObject(self.hdc, old_brush);
                    }
                    winapi::um::wingdi::SelectObject(self.hdc, old_pen);
                    winapi::um::wingdi::DeleteObject(pen as _);
                }
            }
        }
        fn draw_text_styled(&mut self, x: f64, y: f64, text: &str, font: &str, size: f64,
                            r: f64, g: f64, b: f64, _a: f64, _slant: i32, weight: i32) {
            unsafe {
                let wide: Vec<u16> = text.encode_utf16().collect();
                let color: u32 = winapi::um::wingdi::RGB(
                    (r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8);
                winapi::um::wingdi::SetTextColor(self.hdc, color);
                let old_bkmode = winapi::um::wingdi::SetBkMode(self.hdc, winapi::um::wingdi::TRANSPARENT as i32);
                let hfont = Self::make_font(font, size, if weight != 0 { winapi::um::wingdi::FW_BOLD as i32 } else { winapi::um::wingdi::FW_NORMAL as i32 }, _slant != 0);
                if hfont.is_null() {
                    let sys = winapi::um::wingdi::GetStockObject(winapi::um::wingdi::SYSTEM_FONT as i32) as winapi::shared::windef::HFONT;
                    let old = winapi::um::wingdi::SelectObject(self.hdc, sys as _);
                    winapi::um::wingdi::TextOutW(self.hdc, x as i32, y as i32, wide.as_ptr() as _, wide.len() as i32);
                    winapi::um::wingdi::SelectObject(self.hdc, old);
                } else {
                    let old_font = winapi::um::wingdi::SelectObject(self.hdc, hfont as _);
                    winapi::um::wingdi::TextOutW(self.hdc, x as i32, y as i32, wide.as_ptr() as _, wide.len() as i32);
                    winapi::um::wingdi::SelectObject(self.hdc, old_font);
                    winapi::um::wingdi::DeleteObject(hfont as _);
                }
                winapi::um::wingdi::SetBkMode(self.hdc, old_bkmode);
            }
        }
        fn text_extents_styled(&self, text: &str, font: &str, size: f64, _slant: i32, weight: i32) -> (f64, f64, f64, f64) {
            unsafe {
                let wide: Vec<u16> = text.encode_utf16().collect();
                let hfont = Self::make_font(font, size, if weight != 0 { winapi::um::wingdi::FW_BOLD as i32 } else { winapi::um::wingdi::FW_NORMAL as i32 }, _slant != 0);
                if hfont.is_null() { return (0.0, 0.0, 0.0, 0.0); }
                let old_font = winapi::um::wingdi::SelectObject(self.hdc, hfont as _);
                let mut size_tag: winapi::shared::windef::SIZE = std::mem::zeroed();
                winapi::um::wingdi::GetTextExtentPoint32W(self.hdc, wide.as_ptr() as _, wide.len() as i32, &mut size_tag);
                winapi::um::wingdi::SelectObject(self.hdc, old_font);
                winapi::um::wingdi::DeleteObject(hfont as _);
                (0.0, 0.0, size_tag.cx as f64, size_tag.cy as f64)
            }
        }
        fn clear(&mut self, r: f64, g: f64, b: f64, _a: f64) {
            self.fill_rect(0.0, 0.0, self.w as f64, self.h as f64, r, g, b, 1.0);
        }
        fn save(&mut self) {
            unsafe { winapi::um::wingdi::SaveDC(self.hdc); }
        }
        fn restore(&mut self) {
            unsafe { winapi::um::wingdi::RestoreDC(self.hdc, -1); }
        }
        fn clip(&mut self, x: f64, y: f64, w: f64, h: f64) {
            unsafe { winapi::um::wingdi::IntersectClipRect(self.hdc, x as i32, y as i32, (x + w) as i32, (y + h) as i32); }
        }
    }

    // ========== Canvas ==========

    pub struct Canvas {
        frame: Option<Rc<nwg::Frame>>,
        hwnd: *mut c_void,
        draw_cb: Rc<RefCell<Option<Box<dyn FnMut(&mut dyn crate::core::DrawContext, i32, i32)>>>>,
        click_cb: Rc<RefCell<Option<Box<dyn FnMut(f64, f64)>>>>,
        key_cb: Rc<RefCell<Option<Box<dyn FnMut(u32) -> bool>>>>,
        _raw_handlers: Rc<Vec<nwg::RawEventHandler>>,
        painting: Rc<RefCell<bool>>,
    }

    impl Canvas {
        pub fn set_draw_callback(&self, cb: Box<dyn FnMut(&mut dyn DrawContext, i32, i32)>) {
            *self.draw_cb.borrow_mut() = Some(cb);
            self.queue_redraw();
        }
        pub fn queue_redraw(&self) {
            if !self.hwnd.is_null() {
                unsafe {
                    winapi::um::winuser::InvalidateRect(self.hwnd as _, std::ptr::null_mut(), 0);
                }
            }
        }
        pub fn set_size_request(&self, w: i32, h: i32) {
            if !self.hwnd.is_null() {
                unsafe {
                    winapi::um::winuser::SetWindowPos(
                        self.hwnd as _,
                        std::ptr::null_mut(), 0, 0, w, h,
                        winapi::um::winuser::SWP_NOZORDER | winapi::um::winuser::SWP_NOMOVE | winapi::um::winuser::SWP_SHOWWINDOW,
                    );
                }
            }
        }
        pub fn set_content_size(&self, _w: i32, _h: i32) {}
        pub fn on_click(&self, cb: Box<dyn FnMut(f64, f64)>) {
            *self.click_cb.borrow_mut() = Some(cb);
        }
        pub fn on_key(&self, cb: Box<dyn FnMut(u32) -> bool>) {
            *self.key_cb.borrow_mut() = Some(cb);
        }
        pub fn on_key_raw(&self, cb: Box<dyn FnMut(u32, u32) -> bool>) {
            let mut cb = cb;
            *self.key_cb.borrow_mut() = Some(Box::new(move |k: u32| -> bool { cb(k, 0) }));
        }
        pub fn grab_focus(&self) {
            unsafe { winapi::um::winuser::SetFocus(self.hwnd as _); }
        }
        pub fn set_can_focus(&self, _can: bool) {}
        pub fn force_draw(&self, _window_ptr: *mut c_void, _fallback_w: i32, _fallback_h: i32) {}
    }

    impl Clone for Canvas {
        fn clone(&self) -> Self {
            Canvas {
                frame: self.frame.clone(),
                hwnd: self.hwnd,
                draw_cb: self.draw_cb.clone(),
                click_cb: self.click_cb.clone(),
                key_cb: self.key_cb.clone(),
                _raw_handlers: Rc::new(Vec::new()),
                painting: self.painting.clone(),
            }
        }
    }

    impl AsRef<*mut c_void> for Canvas {
        fn as_ref(&self) -> &*mut c_void {
            &self.hwnd
        }
    }

    impl Widget for Canvas {
        fn raw_handle(&self) -> *mut c_void { self.hwnd }
    }

    pub fn create_canvas(parent: *mut c_void) -> Result<Canvas, Error> {
        let mut frame = nwg::Frame::default();
        if !parent.is_null() {
            nwg::Frame::builder()
                .flags(nwg::FrameFlags::NONE)
                .size((0, 0))
                .position((0, 0))
                .parent(&nwg::ControlHandle::Hwnd(parent as _))
                .build(&mut frame)
                .map_err(|e| Error::Backend(format!("{}", e)))?;
        }
        let hwnd = frame.handle.hwnd().unwrap_or(std::ptr::null_mut()) as *mut c_void;
        let draw_cb: Rc<RefCell<Option<Box<dyn FnMut(&mut dyn DrawContext, i32, i32)>>>> = Rc::new(RefCell::new(None));
        let painting: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
        let click_cb: Rc<RefCell<Option<Box<dyn FnMut(f64, f64)>>>> = Rc::new(RefCell::new(None));
        let key_cb: Rc<RefCell<Option<Box<dyn FnMut(u32) -> bool>>>> = Rc::new(RefCell::new(None));

        let mut handlers: Vec<nwg::RawEventHandler> = Vec::new();

        if hwnd != std::ptr::null_mut() {
            let raw_hwnd: winapi::shared::windef::HWND = hwnd as _;

            // WM_KEYDOWN/WM_SYSKEYDOWN handler for keyboard input.
            // Without this, the Canvas's key_cb is never called because
            // no raw handler is registered to process keystroke messages.
            {
                let kc = key_cb.clone();
                static KEYBOARD_ID: AtomicUsize = AtomicUsize::new(0x70000000);
                let kid = KEYBOARD_ID.fetch_add(1, Ordering::SeqCst);
                if let Some(h) = nwg::bind_raw_event_handler(
                    &nwg::ControlHandle::Hwnd(raw_hwnd), kid,
                    move |_h, msg, w, _l| {
                        if msg == winapi::um::winuser::WM_KEYDOWN || msg == winapi::um::winuser::WM_SYSKEYDOWN {
                            if let Some(ref mut f) = *kc.borrow_mut() {
                                if f(w as u32) { return Some(0); }
                            }
                        }
                        None
                    },
                ).ok() { handlers.push(h); }
            }

            // Suppress WM_ERASEBKGND (prevent flash from class background brush)
            {
                static ERASE_ID: AtomicUsize = AtomicUsize::new(0x60000000);
                let eid = ERASE_ID.fetch_add(1, Ordering::SeqCst);
                if let Some(h) = nwg::bind_raw_event_handler(
                    &nwg::ControlHandle::Hwnd(raw_hwnd), eid,
                    move |_h, msg, _w, _l| {
                        if msg == winapi::um::winuser::WM_ERASEBKGND { Some(1) } else { None }
                    },
                ).ok() { handlers.push(h); }
            }

            // WM_PAINT handler
            {
                let cb = draw_cb.clone();
                let paint_flag = painting.clone();
                static CANVAS_PAINT_ID: AtomicUsize = AtomicUsize::new(0x30000000);
                let pid = CANVAS_PAINT_ID.fetch_add(1, Ordering::SeqCst);
                if let Some(h) = nwg::bind_raw_event_handler(
                    &nwg::ControlHandle::Hwnd(raw_hwnd), pid,
                    move |_h, msg, _w, _l| {
                        if msg != winapi::um::winuser::WM_PAINT { return None; }
                        if *paint_flag.borrow() { return Some(0); }
                        *paint_flag.borrow_mut() = true;
                        unsafe {
                            let mut ps: winapi::um::winuser::PAINTSTRUCT = std::mem::zeroed();
                            let hdc = winapi::um::winuser::BeginPaint(hwnd as _, &mut ps);
                            let mut rect: winapi::shared::windef::RECT = std::mem::zeroed();
                            winapi::um::winuser::GetClientRect(hwnd as _, &mut rect);
                            let w = rect.right;
                            let h = rect.bottom;
                            if w > 0 && h > 0 {
                                let mem_dc = winapi::um::wingdi::CreateCompatibleDC(hdc);
                                if !mem_dc.is_null() {
                                    let bmp = winapi::um::wingdi::CreateCompatibleBitmap(hdc, w, h);
                                    if !bmp.is_null() {
                                        let old = winapi::um::wingdi::SelectObject(mem_dc, bmp as _);
                                        if let Some(ref mut draw_fn) = *cb.borrow_mut() {
                                            let mut ctx = NwgDrawContext { hdc: mem_dc, w, h };
                                            draw_fn(&mut ctx, w, h);
                                        }
                                        winapi::um::wingdi::BitBlt(hdc, 0, 0, w, h, mem_dc, 0, 0, winapi::um::wingdi::SRCCOPY);
                                        winapi::um::wingdi::SelectObject(mem_dc, old);
                                        winapi::um::wingdi::DeleteObject(bmp as _);
                                    }
                                    winapi::um::wingdi::DeleteDC(mem_dc);
                                }
                            }
                            winapi::um::winuser::EndPaint(hwnd as _, &mut ps);
                        }
                        *paint_flag.borrow_mut() = false;
                        Some(0)
                    },
                ).ok() { handlers.push(h); }
            }

            // WM_LBUTTONDOWN handler
            {
                let cc = click_cb.clone();
                static CLICK_ID: AtomicUsize = AtomicUsize::new(0x40000000);
                let cid = CLICK_ID.fetch_add(1, Ordering::SeqCst);
                if let Some(h) = nwg::bind_raw_event_handler(
                    &nwg::ControlHandle::Hwnd(raw_hwnd), cid,
                    move |_h, msg, _w, l| {
                        if msg != winapi::um::winuser::WM_LBUTTONDOWN { return None; }
                        unsafe {
                            let x = (l & 0xFFFF) as i16 as f64;
                            let y = ((l >> 16) & 0xFFFF) as i16 as f64;
                            if let Some(ref mut f) = *cc.borrow_mut() {
                                f(x, y);
                            }
                        }
                        Some(0)
                    },
                ).ok() { handlers.push(h); }
            }

            // WM_KEYDOWN handler — need focus first; forward WM_SETFOCUS to force keyboard input
            {
                let kc = key_cb.clone();
                static KEY_ID: AtomicUsize = AtomicUsize::new(0x50000000);
                let kid = KEY_ID.fetch_add(1, Ordering::SeqCst);
                if let Some(h) = nwg::bind_raw_event_handler(
                    &nwg::ControlHandle::Hwnd(raw_hwnd), kid,
                    move |_h, msg, w, _l| {
                        if msg != winapi::um::winuser::WM_KEYDOWN && msg != winapi::um::winuser::WM_SYSKEYDOWN { return None; }
                        if let Some(ref mut f) = *kc.borrow_mut() {
                            if f(w as u32) { return Some(0); }
                        }
                        None
                    },
                ).ok() { handlers.push(h); }
            }
        }

        Ok(Canvas {
            frame: Some(Rc::new(frame)),
            hwnd,
            draw_cb,
            click_cb,
            key_cb,
            _raw_handlers: Rc::new(handlers),
            painting,
        })
    }

    // ========== Overlay ==========

    pub struct Overlay {
        frame: Option<Rc<nwg::Frame>>,
        hwnd: *mut c_void,
    }

    impl Overlay {
        pub fn set_child(&self, child: &impl AsRef<*mut c_void>) {
            let ptr = *child.as_ref();
            if !ptr.is_null() && !self.hwnd.is_null() {
                unsafe {
                    winapi::um::winuser::SetParent(ptr as _, self.hwnd as _);
                }
            }
        }
        pub fn add_overlay(&self, child: &impl AsRef<*mut c_void>) {
            let ptr = *child.as_ref();
            if !ptr.is_null() && !self.hwnd.is_null() {
                unsafe {
                    winapi::um::winuser::SetParent(ptr as _, self.hwnd as _);
                    winapi::um::winuser::SetWindowPos(
                        ptr as _, winapi::um::winuser::HWND_TOP,
                        0, 0, 0, 0,
                        winapi::um::winuser::SWP_NOSIZE | winapi::um::winuser::SWP_SHOWWINDOW,
                    );
                }
            }
        }
        pub fn set_overlay_pass_through(&self, _child: &impl AsRef<*mut c_void>, _pass: bool) {}
        pub fn remove(&self, _child: &impl AsRef<*mut c_void>) {}
        pub fn show_all(&self) {
            if !self.hwnd.is_null() {
                unsafe {
                    winapi::um::winuser::ShowWindow(self.hwnd as _, winapi::um::winuser::SW_SHOW);
                }
            }
        }
        pub fn set_vexpand(&self, _expand: bool) {}
        pub fn set_hexpand(&self, _expand: bool) {}
        pub fn set_size_request(&self, w: i32, h: i32) {
            if !self.hwnd.is_null() {
                unsafe {
                    winapi::um::winuser::SetWindowPos(
                        self.hwnd as _,
                        std::ptr::null_mut(), 0, 0, w, h,
                        winapi::um::winuser::SWP_NOZORDER | winapi::um::winuser::SWP_NOMOVE | winapi::um::winuser::SWP_SHOWWINDOW,
                    );
                }
            }
        }
    }

    impl Clone for Overlay {
        fn clone(&self) -> Self {
            Overlay { frame: self.frame.clone(), hwnd: self.hwnd }
        }
    }

    impl AsRef<*mut c_void> for Overlay {
        fn as_ref(&self) -> &*mut c_void {
            &self.hwnd
        }
    }

    impl Widget for Overlay {
        fn raw_handle(&self) -> *mut c_void { self.hwnd }
    }

    pub fn create_overlay(parent: *mut c_void) -> Result<Overlay, Error> {
        let mut frame = nwg::Frame::default();
        if !parent.is_null() {
            nwg::Frame::builder()
                .flags(nwg::FrameFlags::NONE)
                .size((0, 0))
                .position((0, 0))
                .parent(&nwg::ControlHandle::Hwnd(parent as _))
                .build(&mut frame)
                .map_err(|e| Error::Backend(format!("{}", e)))?;
        }
        let hwnd = frame.handle.hwnd().unwrap_or(std::ptr::null_mut()) as *mut c_void;
        Ok(Overlay { frame: Some(Rc::new(frame)), hwnd })
    }

    // ========== ScrolledWindow ==========

    pub struct ScrolledWindow {
        frame: Option<Rc<nwg::Frame>>,
        hwnd: *mut c_void,
        child: Rc<RefCell<Option<*mut c_void>>>,
        vscroll: Rc<RefCell<nwg::ScrollBar>>,
        hscroll: Rc<RefCell<nwg::ScrollBar>>,
        _handlers: Rc<Vec<nwg::RawEventHandler>>,
        child_size: Rc<RefCell<(i32, i32)>>,
    }

    impl Clone for ScrolledWindow {
        fn clone(&self) -> Self {
            ScrolledWindow {
                frame: self.frame.clone(),
                hwnd: self.hwnd,
                child: self.child.clone(),
                vscroll: self.vscroll.clone(),
                hscroll: self.hscroll.clone(),
                _handlers: self._handlers.clone(),
                child_size: self.child_size.clone(),
            }
        }
    }

    impl ScrolledWindow {
        pub fn set_child(&self, child: &impl AsRef<*mut c_void>) {
            let ptr = *child.as_ref();
            if ptr.is_null() || self.hwnd.is_null() { return; }
            unsafe {
                winapi::um::winuser::SetParent(ptr as _, self.hwnd as _);
                winapi::um::winuser::SetWindowPos(
                    ptr as _, std::ptr::null_mut(),
                    0, 0, 0, 0,
                    winapi::um::winuser::SWP_NOZORDER | winapi::um::winuser::SWP_NOSIZE | winapi::um::winuser::SWP_SHOWWINDOW,
                );
            }
            *self.child.borrow_mut() = Some(ptr);
            // Get child size for scroll range
            unsafe {
                let mut rect: winapi::shared::windef::RECT = std::mem::zeroed();
                winapi::um::winuser::GetWindowRect(ptr as _, &mut rect);
                *self.child_size.borrow_mut() = (rect.right - rect.left, rect.bottom - rect.top);
            }
            self.update_scroll_range();
        }

        pub fn set_policy(&self, _h: u32, _v: u32) {
            // NWG: always show both scrollbars
        }

        pub fn set_vexpand(&self, _v: bool) {}
        pub fn set_hexpand(&self, _h: bool) {}

        fn update_scroll_range(&self) {
            let (child_w, child_h) = *self.child_size.borrow();
            unsafe {
                let mut rect: winapi::shared::windef::RECT = std::mem::zeroed();
                winapi::um::winuser::GetClientRect(self.hwnd as _, &mut rect);
                let view_w = rect.right - rect.left;
                let view_h = rect.bottom - rect.top;
                let vrange = if child_h > view_h { (child_h - view_h) as usize } else { 0usize };
                let hrange = if child_w > view_w { (child_w - view_w) as usize } else { 0usize };
                if let Ok(mut sb) = self.vscroll.try_borrow_mut() {
                    sb.set_range(0..vrange.max(1));
                }
                if let Ok(mut sb) = self.hscroll.try_borrow_mut() {
                    sb.set_range(0..hrange.max(1));
                }
            }
        }

        fn on_scroll(&self) {
            let (child_w, child_h) = *self.child_size.borrow();
            if child_w == 0 && child_h == 0 { return; }
            let child_hwnd = match *self.child.borrow() {
                Some(h) => h,
                None => return,
            };
            let vpos = if let Ok(sb) = self.vscroll.try_borrow() { sb.pos() } else { return };
            let hpos = if let Ok(sb) = self.hscroll.try_borrow() { sb.pos() } else { return };
            unsafe {
                let mut rect: winapi::shared::windef::RECT = std::mem::zeroed();
                winapi::um::winuser::GetClientRect(self.hwnd as _, &mut rect);
                let view_w = rect.right - rect.left;
                let view_h = rect.bottom - rect.top;
                let max_v = (child_h - view_h).max(0);
                let max_h = (child_w - view_w).max(0);
                let v_range = if let Ok(sb) = self.vscroll.try_borrow() { sb.range().end } else { 1 };
                let h_range = if let Ok(sb) = self.hscroll.try_borrow() { sb.range().end } else { 1 };
                let scroll_y = if v_range > 0 { -(vpos as i32 * max_v / v_range as i32) } else { 0 };
                let scroll_x = if h_range > 0 { -(hpos as i32 * max_h / h_range as i32) } else { 0 };
                winapi::um::winuser::SetWindowPos(
                    child_hwnd as _,
                    std::ptr::null_mut(),
                    scroll_x, scroll_y,
                    0, 0,
                    winapi::um::winuser::SWP_NOZORDER | winapi::um::winuser::SWP_NOSIZE | winapi::um::winuser::SWP_SHOWWINDOW,
                );
            }
        }
    }

    impl AsRef<*mut c_void> for ScrolledWindow {
        fn as_ref(&self) -> &*mut c_void {
            &self.hwnd
        }
    }

    pub fn create_scrolled_window(parent: *mut c_void) -> Result<ScrolledWindow, Error> {
        let mut frame = nwg::Frame::default();
        if !parent.is_null() {
            nwg::Frame::builder()
                .flags(nwg::FrameFlags::NONE)
                .size((0, 0))
                .position((0, 0))
                .parent(&nwg::ControlHandle::Hwnd(parent as _))
                .build(&mut frame)
                .map_err(|e| Error::Backend(format!("{}", e)))?;
        }
        let hwnd = frame.handle.hwnd().unwrap_or(std::ptr::null_mut()) as *mut c_void;

        let mut vscroll = nwg::ScrollBar::default();
        nwg::ScrollBar::builder()
            .flags(nwg::ScrollBarFlags::VISIBLE | nwg::ScrollBarFlags::VERTICAL)
            .parent(&nwg::ControlHandle::Hwnd(hwnd as _))
            .range(Some(0..1))
            .pos(Some(0))
            .build(&mut vscroll)
            .map_err(|e| Error::Backend(format!("{}", e)))?;

        let mut hscroll = nwg::ScrollBar::default();
        nwg::ScrollBar::builder()
            .flags(nwg::ScrollBarFlags::VISIBLE | nwg::ScrollBarFlags::HORIZONTAL)
            .parent(&nwg::ControlHandle::Hwnd(hwnd as _))
            .range(Some(0..1))
            .pos(Some(0))
            .build(&mut hscroll)
            .map_err(|e| Error::Backend(format!("{}", e)))?;

        let vscroll = Rc::new(RefCell::new(vscroll));
        let hscroll = Rc::new(RefCell::new(hscroll));
        let child: Rc<RefCell<Option<*mut c_void>>> = Rc::new(RefCell::new(None));
        let child_size: Rc<RefCell<(i32, i32)>> = Rc::new(RefCell::new((0, 0)));

        // Position scrollbars at right/bottom edges
        let v_hwnd = if let Ok(sb) = vscroll.try_borrow() {
            sb.handle.hwnd().unwrap_or(std::ptr::null_mut())
        } else { std::ptr::null_mut() };
        let h_hwnd = if let Ok(sb) = hscroll.try_borrow() {
            sb.handle.hwnd().unwrap_or(std::ptr::null_mut())
        } else { std::ptr::null_mut() };

        let mut handlers: Vec<nwg::RawEventHandler> = Vec::new();

        // WM_SIZE on frame to reposition scrollbars and update range
        if hwnd != std::ptr::null_mut() {
            let vscroll_sz = vscroll.clone();
            let hscroll_sz = hscroll.clone();
            let child_sz = child_size.clone();
            let c_hwnd = hwnd;
            static SIZE_ID: AtomicUsize = AtomicUsize::new(0x80000000);
            let sid = SIZE_ID.fetch_add(1, Ordering::SeqCst);
            if let Some(h) = nwg::bind_raw_event_handler(
                &nwg::ControlHandle::Hwnd(hwnd as _), sid,
                move |_h, msg, _w, _l| {
                    if msg != winapi::um::winuser::WM_SIZE { return None; }
                    unsafe {
                        let mut rect: winapi::shared::windef::RECT = std::mem::zeroed();
                        winapi::um::winuser::GetClientRect(c_hwnd as _, &mut rect);
                        let w = rect.right;
                        let h = rect.bottom;
                        let scroll_w = 20i32;
                        let scroll_h = 20i32;
                        if let Ok(sb) = vscroll_sz.try_borrow() {
                            if let Some(vh) = sb.handle.hwnd() {
                                winapi::um::winuser::SetWindowPos(
                                    vh as _, std::ptr::null_mut(),
                                    w - scroll_w, 0, scroll_w, h - scroll_h,
                                    winapi::um::winuser::SWP_NOZORDER | winapi::um::winuser::SWP_SHOWWINDOW,
                                );
                            }
                        }
                        if let Ok(sb) = hscroll_sz.try_borrow() {
                            if let Some(hh) = sb.handle.hwnd() {
                                winapi::um::winuser::SetWindowPos(
                                    hh as _, std::ptr::null_mut(),
                                    0, h - scroll_h, w - scroll_w, scroll_h,
                                    winapi::um::winuser::SWP_NOZORDER | winapi::um::winuser::SWP_SHOWWINDOW,
                                );
                            }
                        }
                        // Update scroll range based on child size
                        let (cw, ch) = *child_sz.borrow();
                        let view_w = w;
                        let view_h = h;
                        let vrange = if ch > view_h { (ch - view_h) as usize } else { 0usize };
                        let hrange = if cw > view_w { (cw - view_w) as usize } else { 0usize };
                        if let Ok(sb) = vscroll_sz.try_borrow_mut() {
                            sb.set_range(0..vrange.max(1));
                        }
                        if let Ok(sb) = hscroll_sz.try_borrow_mut() {
                            sb.set_range(0..hrange.max(1));
                        }
                    }
                    None
                },
            ).ok() { handlers.push(h); }

            // VScroll raw handler
            let vscroll_sc = vscroll.clone();
            let child_sc = child.clone();
            let c_hwnd_sc = hwnd;
            let child_sz_sc = child_size.clone();
            static VSCROLL_ID: AtomicUsize = AtomicUsize::new(0x90000000);
            let vsid = VSCROLL_ID.fetch_add(1, Ordering::SeqCst);
            if let Some(h) = nwg::bind_raw_event_handler(
                &nwg::ControlHandle::Hwnd(v_hwnd as _), vsid,
                move |_h, msg, _w, _l| {
                    if msg != winapi::um::winuser::WM_VSCROLL { return None; }
                    let (cw, ch) = *child_sz_sc.borrow();
                    if cw == 0 && ch == 0 { return Some(0); }
                    let child_hwnd = match *child_sc.borrow() { Some(h) => h, None => return Some(0) };
                    let pos = if let Ok(sb) = vscroll_sc.try_borrow() { sb.pos() } else { return Some(0) };
                    unsafe {
                        let mut rect: winapi::shared::windef::RECT = std::mem::zeroed();
                        winapi::um::winuser::GetClientRect(c_hwnd_sc as _, &mut rect);
                        let view_h = rect.bottom - rect.top;
                        let max_v = (ch - view_h).max(0);
                        let range = if let Ok(sb) = vscroll_sc.try_borrow() { sb.range().end.max(1) } else { 1 };
                        let scroll_y = -(pos as i32 * max_v / range as i32);
                        winapi::um::winuser::SetWindowPos(
                            child_hwnd as _, std::ptr::null_mut(),
                            0, scroll_y, 0, 0,
                            winapi::um::winuser::SWP_NOZORDER | winapi::um::winuser::SWP_NOSIZE | winapi::um::winuser::SWP_SHOWWINDOW,
                        );
                    }
                    Some(0)
                },
            ).ok() { handlers.push(h); }

            // HScroll raw handler
            let hscroll_sc = hscroll.clone();
            let child_sc2 = child.clone();
            let c_hwnd_sc2 = hwnd;
            let child_sz_sc2 = child_size.clone();
            static HSCROLL_ID: AtomicUsize = AtomicUsize::new(0xA0000000);
            let hsid = HSCROLL_ID.fetch_add(1, Ordering::SeqCst);
            if let Some(h) = nwg::bind_raw_event_handler(
                &nwg::ControlHandle::Hwnd(h_hwnd as _), hsid,
                move |_h, msg, _w, _l| {
                    if msg != winapi::um::winuser::WM_HSCROLL { return None; }
                    let (cw, ch) = *child_sz_sc2.borrow();
                    if cw == 0 && ch == 0 { return Some(0); }
                    let child_hwnd = match *child_sc2.borrow() { Some(h) => h, None => return Some(0) };
                    let pos = if let Ok(sb) = hscroll_sc.try_borrow() { sb.pos() } else { return Some(0) };
                    unsafe {
                        let mut rect: winapi::shared::windef::RECT = std::mem::zeroed();
                        winapi::um::winuser::GetClientRect(c_hwnd_sc2 as _, &mut rect);
                        let view_w = rect.right - rect.left;
                        let max_h = (cw - view_w).max(0);
                        let range = if let Ok(sb) = hscroll_sc.try_borrow() { sb.range().end.max(1) } else { 1 };
                        let scroll_x = -(pos as i32 * max_h / range as i32);
                        winapi::um::winuser::SetWindowPos(
                            child_hwnd as _, std::ptr::null_mut(),
                            scroll_x, 0, 0, 0,
                            winapi::um::winuser::SWP_NOZORDER | winapi::um::winuser::SWP_NOSIZE | winapi::um::winuser::SWP_SHOWWINDOW,
                        );
                    }
                    Some(0)
                },
            ).ok() { handlers.push(h); }
        }

        Ok(ScrolledWindow {
            frame: Some(Rc::new(frame)),
            hwnd,
            child,
            vscroll,
            hscroll,
            _handlers: Rc::new(handlers),
            child_size,
        })
    }

    // ========== Menu / MenuBar / SimpleAction ==========

    /// Convert adapter MenuItemData → nwg builder format
    fn as_nwg_data(items: &[MenuItem]) -> Vec<crate::backends::nwg::MenuItemData> {
        items.iter().map(|i| crate::backends::nwg::MenuItemData {
            label: i.label.clone(),
            detailed_action: i.action.clone(),
            submenu: i.submenu.as_ref().map(|s| as_nwg_data(&s.items)),
        }).collect()
    }

    // -- Menu (data-only, builds nothing until consumed by MenuBar) --

    struct MenuItem {
        label: String,
        action: String,
        submenu: Option<Menu>,
    }

    pub struct Menu {
        items: Vec<MenuItem>,
    }

    impl Menu {
        pub fn append(&mut self, label: &str, detailed_action: &str) {
            self.items.push(MenuItem {
                label: label.to_string(),
                action: detailed_action.to_string(),
                submenu: None,
            });
        }
        pub fn append_submenu(&mut self, label: &str, submenu: &Menu) {
            self.items.push(MenuItem {
                label: label.to_string(),
                action: String::new(),
                submenu: Some(submenu.clone()),
            });
        }
    }

    impl Clone for Menu { fn clone(&self) -> Self { Menu { items: self.items.iter().map(|i| MenuItem {
        label: i.label.clone(), action: i.action.clone(), submenu: i.submenu.clone(),
    }).collect() } } }

    pub fn create_menu() -> Result<Menu, Error> { Ok(Menu { items: Vec::new() }) }

    // -- MenuBar: builds NWG menus, wires actions --

    /// Table mapping (hmenu_ptr, item_index) → stripped action name
    type MenuIndex = HashMap<(*mut std::ffi::c_void, u32), String>;

    pub struct MenuBar {
        pub(crate) _menus: Rc<Vec<nwg::Menu>>,
        pub(crate) _items: Rc<Vec<nwg::MenuItem>>,
        pub(crate) _raw_handler: Rc<nwg::RawEventHandler>,
        pub(crate) action_registry: Rc<RefCell<HashMap<String, Box<dyn FnMut()>>>>,
    }

    impl Clone for MenuBar {
        fn clone(&self) -> Self {
            MenuBar {
                _menus: self._menus.clone(),
                _items: self._items.clone(),
                _raw_handler: self._raw_handler.clone(),
                action_registry: self.action_registry.clone(),
            }
        }
    }

    impl AsRef<*mut c_void> for MenuBar {
        fn as_ref(&self) -> &*mut c_void {
            static NULL: usize = 0;
            unsafe { &*(&NULL as *const usize as *const *mut c_void) }
        }
    }

    impl Widget for MenuBar {
        fn raw_handle(&self) -> *mut c_void {
            std::ptr::null_mut()
        }
    }

    impl MenuBar {
        pub fn activate_submenu_by_mnemonic(&self, _keyval: u32) -> bool { false }
        pub fn activate_submenu_item_by_mnemonic(&self, _keyval: u32) -> bool { false }
        pub unsafe fn insert_action_group(&self, _name: &str, _group_ptr: *mut c_void) {}
        pub fn handle_mnemonic_key(&self, _keyval: u32) -> bool { false }
        pub fn handle_menu_key(&self, _keyval: u32, _mod: u32) -> bool { false }
        pub fn menu_active(&self) -> bool { false }
        pub fn menu_close(&self) {}
    }

    /// Recursively build NWG menu items, recording the (hmenu, index)→action mapping.
    /// Collectors `menus` and `items` keep the NWG objects alive (else Drop destroys them).
    fn build_and_index(
        parent_handle: &nwg::ControlHandle,
        items: &[crate::backends::nwg::MenuItemData],
        index: &mut MenuIndex,
        menus: &mut Vec<nwg::Menu>,
        items_collector: &mut Vec<nwg::MenuItem>,
    ) -> Result<(), nwg::NwgError> {
        for (i, item) in items.iter().enumerate() {
            if let Some(ref children) = item.submenu {
                let mut sub = nwg::Menu::default();
                nwg::Menu::builder()
                    .text(&format!("&{}", item.label))
                    .popup(false)
                    .parent(parent_handle.clone())
                    .build(&mut sub)?;
                let sub_hmenu = sub.handle.hmenu().map(|(_, h)| h).unwrap_or(std::ptr::null_mut());
                let sub_handle = match parent_handle {
                    nwg::ControlHandle::Hwnd(h) => nwg::ControlHandle::PopMenu(*h, sub_hmenu),
                    nwg::ControlHandle::PopMenu(h, _) => nwg::ControlHandle::PopMenu(*h, sub_hmenu),
                    _ => unreachable!(),
                };
                menus.push(sub);
                build_and_index(&sub_handle, children, index, menus, items_collector)?;
            } else {
                let mut mi = nwg::MenuItem::default();
                nwg::MenuItem::builder()
                    .text(&format!("&{}", item.label))
                    .parent(parent_handle.clone())
                    .build(&mut mi)?;
                let parent_hmenu: *mut c_void = match parent_handle {
                    nwg::ControlHandle::Hwnd(h) => *h as *mut c_void,
                    nwg::ControlHandle::PopMenu(_h, m) => *m as *mut c_void,
                    _ => unreachable!(),
                };
                if !item.detailed_action.is_empty() {
                    index.insert((parent_hmenu as *mut c_void, i as u32), item.detailed_action.clone());
                }
                items_collector.push(mi);
            }
        }
        Ok(())
    }

    /// Count leaf items in the menu tree (sequential numbering).
    fn count_leaves(items: &[crate::backends::nwg::MenuItemData]) -> u32 {
        let mut n = 0;
        for i in items {
            if let Some(ref children) = i.submenu {
                n += count_leaves(children);
            } else {
                n += 1;
            }
        }
        n
    }

    pub fn create_menubar(
        model: &Menu,
        window_hwnd: *mut c_void,
        action_registry: Rc<RefCell<HashMap<String, Box<dyn FnMut()>>>>,
    ) -> Result<MenuBar, Error> {
        let window_handle = nwg::ControlHandle::Hwnd(window_hwnd as _);
        let mut index: MenuIndex = HashMap::new();
        let data = as_nwg_data(&model.items);
        let mut menus: Vec<nwg::Menu> = Vec::new();
        let mut items: Vec<nwg::MenuItem> = Vec::new();

        // Build top-level menubar entries (File, Edit, Help, ...)
        for item in &data {
            if let Some(ref children) = item.submenu {
                let mut menu = nwg::Menu::default();
                nwg::Menu::builder()
                    .text(&format!("&{}", item.label))
                    .popup(false)
                    .parent(&window_handle)
                    .build(&mut menu)
                    .map_err(|e| Error::Backend(format!("{}", e)))?;
                let menu_hmenu = menu.handle.hmenu().map(|(_, h)| h).unwrap_or(std::ptr::null_mut());
                let menu_handle = nwg::ControlHandle::PopMenu(
                    window_hwnd as *mut std::ffi::c_void as _,
                    menu_hmenu,
                );
                menus.push(menu);
                build_and_index(&menu_handle, children, &mut index, &mut menus, &mut items)
                    .map_err(|e| Error::Backend(format!("{}", e)))?;
            } else {
                let mut mi = nwg::MenuItem::default();
                nwg::MenuItem::builder()
                    .text(&format!("&{}", item.label))
                    .parent(&window_handle)
                    .build(&mut mi)
                    .map_err(|e| Error::Backend(format!("{}", e)))?;
                items.push(mi);
            }
        }

        // Bind raw event handler for WM_MENUCOMMAND
        // handler_id must be > 0xFFFF (NWG reserves lower IDs)
        const RAW_MENU_ID: usize = 0x10001;
        let idx = index.clone();
        let reg = action_registry.clone();
        let raw_handler = nwg::bind_raw_event_handler(
            &nwg::ControlHandle::Hwnd(window_hwnd as _),
            RAW_MENU_ID,
            move |_hwnd, msg, wparam, lparam| {
                if msg != winapi::um::winuser::WM_MENUCOMMAND { return None; }
                let item_index = (wparam & 0xFFFF) as u32;
                let hmenu = lparam as *mut c_void;
                let key = (hmenu, item_index);
                if let Some(action_name) = idx.get(&key) {
                    let stripped = action_name.rsplit('.').next().unwrap_or(action_name);
                    if let Some(cb) = reg.borrow_mut().get_mut(stripped) {
                        cb();
                    }
                }
                Some(0)
            },
        ).map_err(|e| Error::Backend(format!("{}", e)))?;

        Ok(MenuBar { _menus: Rc::new(menus), _items: Rc::new(items), _raw_handler: Rc::new(raw_handler), action_registry })
    }

    // -- SimpleAction: stores callback by action name --

    pub struct SimpleAction {
        pub(crate) name: String,
        pub(crate) registry: Rc<RefCell<HashMap<String, Box<dyn FnMut()>>>>,
    }

    impl Clone for SimpleAction {
        fn clone(&self) -> Self {
            SimpleAction {
                name: self.name.clone(),
                registry: self.registry.clone(),
            }
        }
    }

    impl SimpleAction {
        pub fn connect_activate<F: FnMut(*mut c_void) + 'static>(&self, mut f: F) -> Result<u64, Error> {
            let mut map = self.registry.borrow_mut();
            map.insert(self.name.clone(), Box::new(move || f(std::ptr::null_mut())));
            Ok(0)
        }
    }

    pub fn create_simple_action(
        name: &str,
        registry: Rc<RefCell<HashMap<String, Box<dyn FnMut()>>>>,
    ) -> Result<SimpleAction, Error> {
        Ok(SimpleAction {
            name: name.to_string(),
            registry,
        })
    }
    // ---- File dialogs ----

    pub fn open_file(title: &str, parent: *mut c_void) -> Result<Option<String>, Error> {
        let mut dialog = nwg::FileDialog::default();
        nwg::FileDialog::builder()
            .title(title)
            .action(nwg::FileDialogAction::Open)
            .build(&mut dialog)
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        let parent_handle = nwg::ControlHandle::Hwnd(parent as _);
        if dialog.run(Some(&parent_handle)) {
            match dialog.get_selected_item() {
                Ok(path) => Ok(Some(path.to_string_lossy().into_owned())),
                Err(_) => Ok(None),
            }
        } else {
            Ok(None)
        }
    }

    pub fn save_file(title: &str, parent: *mut c_void) -> Result<Option<String>, Error> {
        let mut dialog = nwg::FileDialog::default();
        nwg::FileDialog::builder()
            .title(title)
            .action(nwg::FileDialogAction::Save)
            .build(&mut dialog)
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        let parent_handle = nwg::ControlHandle::Hwnd(parent as _);
        if dialog.run(Some(&parent_handle)) {
            match dialog.get_selected_item() {
                Ok(path) => Ok(Some(path.to_string_lossy().into_owned())),
                Err(_) => Ok(None),
            }
        } else {
            Ok(None)
        }
    }

    /// Strip a Pango markup string down to plain text (remove `<span ...>` tags
    /// and decode the common entities). Used by the nwg `TextView::set_markup`
    /// shim so conversation lines render legibly without a markup engine.
    fn strip_pango(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let bytes = s.as_bytes();
        let mut i = 0;
        let mut in_tag = false;
        while i < bytes.len() {
            let c = bytes[i] as char;
            if c == '<' {
                in_tag = true;
                i += 1;
                continue;
            }
            if c == '>' {
                in_tag = false;
                i += 1;
                continue;
            }
            if in_tag {
                i += 1;
                continue;
            }
            if c == '&' {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j] as char != ';' {
                    j += 1;
                }
                if j < bytes.len() {
                    let ent = &s[i..=j];
                    let ch = match ent {
                        "&amp;" => '&',
                        "&lt;" => '<',
                        "&gt;" => '>',
                        "&quot;" => '"',
                        "&#39;" => '\'',
                        "&apos;" => '\'',
                        _ => {
                            out.push('&');
                            i += 1;
                            continue;
                        }
                    };
                    out.push(ch);
                    i = j + 1;
                    continue;
                }
            }
            out.push(c);
            i += 1;
        }
        out
    }

    pub fn quit_main_loop() {
        crate::backends::nwg::quit_main_loop();
    }
}

#[cfg(windows)]
pub use nwg_adapter::*;
#[cfg(windows)]
pub use crate::backends::nwg::Orientation;



// ---------- TabView (horizontal tabs; button bar + overlaid show/hide panels) ----------
// The TabView lives at the `backends_nwg_adapter` module level (after
// `mod nwg_adapter` closes and re-exports its items), so the names that
// `mod nwg_adapter` brings into scope via `use` must be re-imported here.
use std::rc::Rc;
use std::cell::{RefCell, Cell};
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use native_windows_gui as nwg;

/// Client size of a window, used to size the active panel to fill `content`.
fn tv_client_size(hwnd: *mut c_void) -> (i32, i32) {
    if hwnd.is_null() { return (0, 0); }
    unsafe {
        let mut rect = std::mem::zeroed();
        winapi::um::winuser::GetClientRect(hwnd as _, &mut rect);
        (rect.right - rect.left, rect.bottom - rect.top)
    }
}

/// Position a panel to exactly fill its (content) parent's client area.
fn tv_fill_panel(panel_hwnd: *mut c_void, w: i32, h: i32) {
    if panel_hwnd.is_null() { return; }
    unsafe {
        winapi::um::winuser::SetWindowPos(
            panel_hwnd as _,
            winapi::um::winuser::HWND_TOP,
            0, 0, w.max(0), h.max(0),
            winapi::um::winuser::SWP_NOZORDER | winapi::um::winuser::SWP_SHOWWINDOW,
        );
    }
}

pub struct TabView {
    pub(crate) outer: BoxWidget,
    pub(crate) tab_bar: BoxWidget,
    pub(crate) content: BoxWidget,
    pub(crate) panels: Rc<RefCell<Vec<BoxWidget>>>,
    pub(crate) buttons: Rc<RefCell<Vec<Button>>>,
    /// Per-tab current index. Stored in a cell (rather than captured by value
    /// in the click closure) so it survives re-numbering when a tab is closed.
    pub(crate) indices: Rc<RefCell<Vec<Rc<Cell<usize>>>>>,
    pub(crate) active: Rc<Cell<usize>>,
    pub(crate) tab_changed: Rc<RefCell<Option<Box<dyn FnMut(usize)>>>>,
}

impl Clone for TabView {
    fn clone(&self) -> Self {
        TabView {
            outer: self.outer.clone(),
            tab_bar: self.tab_bar.clone(),
            content: self.content.clone(),
            panels: self.panels.clone(),
            buttons: self.buttons.clone(),
            indices: self.indices.clone(),
            active: self.active.clone(),
            tab_changed: self.tab_changed.clone(),
        }
    }
}

impl AsRef<*mut std::ffi::c_void> for TabView {
    fn as_ref(&self) -> &*mut std::ffi::c_void { self.outer.as_ref() }
}

fn tv_set_active(
    panels: &Rc<RefCell<Vec<BoxWidget>>>,
    content_hwnd: *mut c_void,
    active: &Rc<Cell<usize>>,
    idx: usize,
    tab_changed: &Rc<RefCell<Option<Box<dyn FnMut(usize)>>>>,
) {
    let count = panels.borrow().len();
    if count == 0 { return; }
    let idx = idx.min(count - 1);
    active.set(idx);
    let (w, h) = tv_client_size(content_hwnd);
    for (i, p) in panels.borrow().iter().enumerate() {
        if i == idx {
            p.set_visible(true);
            tv_fill_panel(p.hwnd, w, h);
        } else {
            p.set_visible(false);
        }
    }
    if let Some(cb) = tab_changed.borrow_mut().as_mut() {
        cb(idx);
    }
}

impl TabView {
    pub fn new(parent: *mut std::ffi::c_void) -> Result<Self, crate::core::Error> {
        let outer = create_box(Orientation::Vertical, 0, parent)?;
        let tab_bar = create_box(Orientation::Horizontal, 4, outer.hwnd)?;
        let content = create_box(Orientation::Vertical, 0, outer.hwnd)?;
        // Register the bar + content with the outer box so they get laid out
        // when the window (and therefore `outer`) is resized on present().
        outer.append(&tab_bar);
        outer.append(&content);
        outer.set_child_hexpand(&tab_bar, true);
        outer.set_child_hexpand(&content, true);
        outer.set_child_vexpand(&content, true);

        let tv = TabView {
            outer,
            tab_bar,
            content,
            panels: Rc::new(RefCell::new(Vec::new())),
            buttons: Rc::new(RefCell::new(Vec::new())),
            indices: Rc::new(RefCell::new(Vec::new())),
            active: Rc::new(Cell::new(0)),
            tab_changed: Rc::new(RefCell::new(None)),
        };

        // Keep the active panel filling the content area whenever `content` is
        // resized (e.g. window resize). Panels are overlaid, not box-laid-out.
        let content_hwnd = tv.content.hwnd;
        let panels_r = tv.panels.clone();
        let active_r = tv.active.clone();
        if content_hwnd != std::ptr::null_mut() {
            static RESIZE_ID: AtomicUsize = AtomicUsize::new(0xC0000000);
            let rid = RESIZE_ID.fetch_add(1, Ordering::SeqCst);
            let _ = nwg::bind_raw_event_handler(
                &nwg::ControlHandle::Hwnd(content_hwnd as winapi::shared::windef::HWND), rid,
                move |_h, msg, _w, l| {
                    if msg == winapi::um::winuser::WM_SIZE {
                        let w = (l & 0xFFFF) as i32;
                        let h = ((l >> 16) & 0xFFFF) as i32;
                        let idx = active_r.get();
                        if let Some(p) = panels_r.borrow().get(idx) {
                            tv_fill_panel(p.hwnd, w, h);
                        }
                    }
                    None
                },
            );
        }
        Ok(tv)
    }
    pub fn add_tab(&self, title: &str) -> Result<usize, crate::core::Error> {
        let idx = self.panels.borrow().len();
        let btn = create_button(self.tab_bar.hwnd, title)?;
        self.tab_bar.append(&btn);
        self.tab_bar.set_child_hexpand(&btn, true);
        let panel = create_box(Orientation::Vertical, 0, self.content.hwnd)?;
        panel.set_visible(false);

        let my_index = Rc::new(Cell::new(idx));
        let panels = self.panels.clone();
        let content_hwnd = self.content.hwnd;
        let active = self.active.clone();
        let tab_changed = self.tab_changed.clone();
        let _ = btn.on_click({
            let my_index = my_index.clone();
            move || { tv_set_active(&panels, content_hwnd, &active, my_index.get(), &tab_changed); }
        });

        self.buttons.borrow_mut().push(btn);
        self.panels.borrow_mut().push(panel.clone());
        self.indices.borrow_mut().push(my_index);

        // The first tab is active immediately so its panel fills the content.
        if idx == 0 {
            tv_set_active(&self.panels, self.content.hwnd, &self.active, 0, &self.tab_changed);
        }
        Ok(idx)
    }
    pub fn tab_box(&self, idx: usize) -> Result<BoxWidget, crate::core::Error> {
        Ok(self.panels.borrow()
            .get(idx)
            .ok_or_else(|| crate::core::Error::Backend(format!("tab index {idx} out of range")))?
            .clone())
    }
    pub fn set_on_tab_changed(&self, cb: Box<dyn FnMut(usize)>) {
        *self.tab_changed.borrow_mut() = Some(cb);
    }
    pub fn set_active(&self, idx: usize) {
        tv_set_active(&self.panels, self.content.hwnd, &self.active, idx, &self.tab_changed);
    }
    pub fn current_tab(&self) -> usize { self.active.get() }
    pub fn tab_count(&self) -> usize { self.panels.borrow().len() }
    pub fn tab_title(&self, idx: usize) -> Option<String> {
        self.buttons.borrow().get(idx).and_then(|b| b.get_text())
    }
    pub fn set_tab_title(&self, idx: usize, title: &str) {
        if let Some(b) = self.buttons.borrow().get(idx) {
            b.set_text(title);
        }
    }
    pub fn close_tab(&self, idx: usize) {
        let count = self.panels.borrow().len();
        if idx >= count { return; }
        if let Some(b) = self.buttons.borrow().get(idx) { self.tab_bar.remove(b); }
        if let Some(p) = self.panels.borrow().get(idx) { self.content.remove(p); }
        self.buttons.borrow_mut().remove(idx);
        self.panels.borrow_mut().remove(idx);
        self.indices.borrow_mut().remove(idx);
        // Re-number surviving tabs so their click handlers stay correct.
        for (i, cell) in self.indices.borrow().iter().enumerate() {
            cell.set(i);
        }
        if self.panels.borrow().is_empty() {
            self.active.set(0);
            return;
        }
        // Activate the neighbour of the closed tab.
        let new_active = if idx >= count - 1 { count - 2 } else { idx };
        self.set_active(new_active);
    }
}
