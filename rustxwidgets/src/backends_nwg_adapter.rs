#[cfg(windows)]
mod nwg_adapter {
    use native_windows_gui as nwg;
    use crate::core::{Error, Widget};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::os::raw::c_void;
    use std::rc::Rc;

    fn set_window_pos(hwnd: *mut c_void, x: i32, y: i32, w: i32, h: i32) {
        unsafe {
            winapi::um::winuser::SetWindowPos(
                hwnd as winapi::um::winnt::HWND,
                std::ptr::null_mut(), x, y, w, h,
                winapi::um::winuser::SWP_NOZORDER | winapi::um::winuser::SWP_SHOWWINDOW,
            );
        }
    }

    // -- Window --

    pub struct Window {
        pub(crate) inner: nwg::Window,
        pub(crate) _handler: nwg::EventHandler,
    }

    impl Widget for Window {
        fn raw_handle(&self) -> *mut c_void {
            self.inner.hwnd().unwrap_or(std::ptr::null_mut()) as *mut c_void
        }
    }

    impl AsRef<*mut c_void> for Window {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.inner.hwnd().unwrap_or(std::ptr::null_mut()) as *const _ as *const *mut c_void) }
        }
    }

    impl Window {
        pub fn set_title(&self, title: &str) { self.inner.set_text(title); }
        pub fn set_child(&self, _child: &impl AsRef<*mut c_void>) {}
        pub fn present(&self) {}
        pub fn insert_action_group(&self, _name: &str, _group_ptr: *mut c_void) {}
        pub fn hwnd(&self) -> *mut c_void {
            self.inner.hwnd().unwrap_or(std::ptr::null_mut()) as *mut c_void
        }
    }

    pub fn create_window(parent_cell: &Rc<RefCell<Option<*mut c_void>>>) -> Result<Window, Error> {
        let (inner, handler) = crate::backends::nwg::create_window(parent_cell)
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Window { inner, _handler: handler })
    }

    // -- Button --

    pub struct Button {
        pub(crate) inner: nwg::Button,
        pub(crate) _handler: nwg::EventHandler,
        pub(crate) click_cb: Rc<RefCell<Option<Box<dyn FnMut()>>>>,
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
    }

    impl AsRef<*mut c_void> for Button {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.inner.hwnd().unwrap_or(std::ptr::null_mut()) as *const _ as *const *mut c_void) }
        }
    }

    pub fn create_button(parent: *mut c_void) -> Result<Button, Error> {
        let (inner, click_cb, handler) = crate::backends::nwg::create_button(parent)
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Button { inner, _handler: handler, click_cb })
    }

    // -- Label --

    pub struct Label(pub(crate) nwg::Label);

    impl Label {
        pub fn set_text(&self, text: &str) { self.0.set_text(text); }
        pub fn get_text(&self) -> Option<String> { Some(self.0.text()) }
        pub fn set_visible(&self, visible: bool) { self.0.set_visible(visible); }
        pub fn set_markup(&self, markup: &str) { self.0.set_text(markup); }
    }

    impl AsRef<*mut c_void> for Label {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.0.hwnd().unwrap_or(std::ptr::null_mut()) as *const _ as *const *mut c_void) }
        }
    }

    pub fn create_label(parent: *mut c_void) -> Result<Label, Error> {
        crate::backends::nwg::create_label(parent).map(Label).map_err(|e| Error::Backend(format!("{}", e)))
    }

    // -- BoxWidget --

    pub struct BoxWidget {
        pub(crate) children: Vec<*mut c_void>,
        pub(crate) orientation: crate::backends::nwg::Orientation,
        pub(crate) spacing: i32,
    }

    impl BoxWidget {
        pub fn append(&mut self, child: &impl AsRef<*mut c_void>) {
            let ptr = *child.as_ref();
            if !ptr.is_null() { self.children.push(ptr); }
        }
        pub fn layout(&self, x: i32, y: i32, w: i32, h: i32) {
            let mut pos = 5;
            let (iw, ih) = match self.orientation {
                crate::backends::nwg::Orientation::Horizontal => (60, h - 10),
                crate::backends::nwg::Orientation::Vertical => (w - 10, 28),
            };
            for &child in &self.children {
                match self.orientation {
                    crate::backends::nwg::Orientation::Horizontal => {
                        set_window_pos(child, x + pos, y + 5, iw, ih);
                        pos += iw + self.spacing;
                    }
                    crate::backends::nwg::Orientation::Vertical => {
                        set_window_pos(child, x + 5, y + pos, iw, ih);
                        pos += ih + self.spacing;
                    }
                }
            }
        }
    }

    pub fn create_box(orientation: crate::backends::nwg::Orientation, spacing: i32) -> Result<BoxWidget, Error> {
        Ok(BoxWidget { children: Vec::new(), orientation, spacing })
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
        pub(crate) inner: nwg::TextInput,
        pub(crate) _handler: nwg::EventHandler,
        pub(crate) changed_cb: Rc<RefCell<Option<Box<dyn FnMut()>>>>,
    }

    impl Entry {
        pub fn set_text(&self, text: &str) { self.inner.set_text(text); }
        pub fn get_text(&self) -> Option<String> { Some(self.inner.text()) }
        pub fn connect_changed(&self, f: impl FnMut() + 'static) -> Result<u64, Error> {
            *self.changed_cb.borrow_mut() = Some(Box::new(f));
            Ok(0)
        }
        pub fn set_width_chars(&self, _n: i32) {}
        pub fn set_size_request(&self, _w: i32, _h: i32) {}
        pub fn grab_focus(&self) { let _ = self.inner.set_focus(); }
    }

    impl AsRef<*mut c_void> for Entry {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.inner.hwnd().unwrap_or(std::ptr::null_mut()) as *const _ as *const *mut c_void) }
        }
    }

    pub fn create_entry(parent: *mut c_void) -> Result<Entry, Error> {
        let (inner, changed_cb, handler) = crate::backends::nwg::create_entry(parent)
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Entry { inner, _handler: handler, changed_cb })
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
        pub(crate) _menu: nwg::Menu,
        pub(crate) _raw_handler: nwg::RawEventHandler,
        pub(crate) action_registry: Rc<RefCell<HashMap<String, Box<dyn FnMut()>>>>,
    }

    impl AsRef<*mut c_void> for MenuBar {
        fn as_ref(&self) -> &*mut c_void { std::ptr::null() }
    }

    /// Recursively build NWG menu items, recording the (hmenu, index)→action mapping.
    /// `local_idx` tracks the 0-based index within the CURRENT menu (resets per submenu).
    fn build_and_index(
        parent_handle: &nwg::ControlHandle,
        items: &[crate::backends::nwg::MenuItemData],
        index: &mut MenuIndex,
    ) -> Result<(), nwg::NwgError> {
        for (i, item) in items.iter().enumerate() {
            if let Some(ref children) = item.submenu {
                let mut sub = nwg::Menu::default();
                nwg::Menu::builder()
                    .text(&format!("&{}", item.label))
                    .popup(true)
                    .parent(parent_handle.clone())
                    .build(&mut sub)?;
                let sub_hmenu = sub.hmenu().unwrap_or(std::ptr::null_mut());
                let sub_handle = match parent_handle {
                    nwg::ControlHandle::Hwnd(h) => nwg::ControlHandle::PopMenu(*h, sub_hmenu),
                    nwg::ControlHandle::PopMenu(h, _) => nwg::ControlHandle::PopMenu(*h, sub_hmenu),
                };
                build_and_index(&sub_handle, children, index)?;
            } else {
                let mut mi = nwg::MenuItem::default();
                nwg::MenuItem::builder()
                    .text(&format!("&{}", item.label))
                    .parent(parent_handle.clone())
                    .build(&mut mi)?;
                let parent_hmenu = match parent_handle {
                    nwg::ControlHandle::Hwnd(h) => *h,
                    nwg::ControlHandle::PopMenu(_h, m) => *m,
                };
                if !item.detailed_action.is_empty() {
                    index.insert((parent_hmenu as *mut c_void, i as u32), item.detailed_action.clone());
                }
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
        // Build the menubar (popup=false → window-level menubar)
        let mut bar = nwg::Menu::default();
        nwg::Menu::builder()
            .popup(false)
            .parent(&nwg::ControlHandle::Hwnd(window_hwnd as _))
            .build(&mut bar)
            .map_err(|e| Error::Backend(format!("{}", e)))?;

        let bar_handle = nwg::ControlHandle::PopMenu(
            window_hwnd as *mut std::ffi::c_void as _,
            bar.hmenu().unwrap_or(std::ptr::null_mut()),
        );

        // Build index: (hmenu, local_index) → action_name
        let mut index: MenuIndex = HashMap::new();
        let data = as_nwg_data(&model.items);
        build_and_index(&bar_handle, &data, &mut index)
            .map_err(|e| Error::Backend(format!("{}", e)))?;

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
                let item_index = ((wparam & 0xFFFF) as u32) - 1; // local index is 0-based
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

        Ok(MenuBar { _menu: bar, _raw_handler: raw_handler, action_registry })
    }

    // -- SimpleAction: stores callback by action name --

    pub struct SimpleAction {
        pub(crate) name: String,
        pub(crate) registry: Rc<RefCell<HashMap<String, Box<dyn FnMut()>>>>,
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
}

#[cfg(windows)]
pub use nwg_adapter::*;
