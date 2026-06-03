#[cfg(windows)]
mod nwg_backend {
    use native_windows_gui as nwg;
    use std::cell::RefCell;
    use std::os::raw::c_void;
    use std::rc::Rc;

    pub struct NwgApp {
        pub(crate) current_parent: Rc<RefCell<Option<*mut c_void>>>,
    }

    impl NwgApp {
        pub fn new() -> Result<Self, nwg::NwgError> {
            nwg::init()?;
            Ok(NwgApp { current_parent: Rc::new(RefCell::new(None)) })
        }
    }

    impl crate::backends::BackendApp for NwgApp {
        fn run(self: Box<Self>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            nwg::dispatch_thread_events();
            Ok(())
        }
    }

    pub fn init() -> Result<Box<dyn crate::backends::BackendApp>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Box::new(NwgApp::new().map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?))
    }

    // -- Window --

    pub fn create_window(parent_cell: &Rc<RefCell<Option<*mut c_void>>>) -> Result<(nwg::Window, nwg::EventHandler), nwg::NwgError> {
        let mut win = nwg::Window::default();
        let handler = nwg::Window::builder()
            .flags(nwg::WindowFlags::MAIN_WINDOW | nwg::WindowFlags::VISIBLE)
            .size((700, 400))
            .build(&mut win)?;
        *parent_cell.borrow_mut() = Some(win.hwnd().unwrap_or(std::ptr::null_mut()) as *mut c_void);
        Ok((win, handler))
    }

    // -- Control creation --

    pub fn create_button(
        parent: *mut c_void,
    ) -> Result<(nwg::Button, Rc<RefCell<Option<Box<dyn FnMut()>>>>, nwg::EventHandler), nwg::NwgError> {
        let mut btn = nwg::Button::default();
        nwg::Button::builder()
            .text("")
            .flags(nwg::ButtonFlags::VISIBLE | nwg::ButtonFlags::TAB_STOP)
            .parent(&nwg::ControlHandle::Hwnd(parent as _))
            .build(&mut btn)?;
        let click_cb: Rc<RefCell<Option<Box<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
        let cb = click_cb.clone();
        let hwnd = btn.hwnd().unwrap();
        let parent_h = nwg::ControlHandle::Hwnd(parent as _);
        let handler = nwg::bind_event_handler(
            &nwg::ControlHandle::Hwnd(hwnd), &parent_h,
            move |evt, _data, _handle| {
                if let nwg::Event::OnButtonClick = evt {
                    if let Some(ref mut f) = *cb.borrow_mut() { f(); }
                }
            },
        );
        Ok((btn, click_cb, handler))
    }

    pub fn create_label(parent: *mut c_void) -> Result<nwg::Label, nwg::NwgError> {
        let mut lbl = nwg::Label::default();
        nwg::Label::builder()
            .text("")
            .flags(nwg::LabelFlags::VISIBLE)
            .parent(&nwg::ControlHandle::Hwnd(parent as _))
            .build(&mut lbl)?;
        Ok(lbl)
    }

    pub fn create_entry(
        parent: *mut c_void,
    ) -> Result<(nwg::TextInput, Rc<RefCell<Option<Box<dyn FnMut()>>>>, nwg::EventHandler), nwg::NwgError> {
        let mut entry = nwg::TextInput::default();
        nwg::TextInput::builder()
            .text("")
            .flags(nwg::TextInputFlags::VISIBLE | nwg::TextInputFlags::TAB_STOP | nwg::TextInputFlags::AUTO_SCROLL)
            .parent(&nwg::ControlHandle::Hwnd(parent as _))
            .build(&mut entry)?;
        let changed_cb: Rc<RefCell<Option<Box<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
        let cb = changed_cb.clone();
        let hwnd = entry.hwnd().unwrap();
        let parent_h = nwg::ControlHandle::Hwnd(parent as _);
        let handler = nwg::bind_event_handler(
            &nwg::ControlHandle::Hwnd(hwnd), &parent_h,
            move |evt, _data, _handle| {
                if let nwg::Event::OnTextInput = evt {
                    if let Some(ref mut f) = *cb.borrow_mut() { f(); }
                }
            },
        );
        Ok((entry, changed_cb, handler))
    }

    // -- Menu building types (used by the adapter) --

    #[derive(Clone)]
    pub struct MenuItemData {
        pub label: String,
        pub detailed_action: String,
        pub submenu: Option<Vec<MenuItemData>>,
    }

    /// Build an NWG menu/submenu from MenuItemData slice.
    /// `parent_handle` is the ControlHandle of the parent (window or parent menu).
    /// `next_id` is a counter for assigning unique menu item IDs.
    /// Returns the list of leaf items with their IDs for action dispatching.
    pub fn build_nwg_menu_items(
        parent_handle: &nwg::ControlHandle,
        items: &[MenuItemData],
        next_id: &mut u32,
    ) -> Result<Vec<(u32, String)>, nwg::NwgError> {
        let mut leaves = Vec::new();
        for item in items {
            if let Some(ref children) = item.submenu {
                // Submenu: create a popup menu with children
                let mut sub_menu = nwg::Menu::default();
                nwg::Menu::builder()
                    .text(&format!("&{}", item.label))
                    .popup(true)
                    .parent(parent_handle.clone())
                    .build(&mut sub_menu)?;
                let sub_hmenu = sub_menu.hmenu().unwrap_or(std::ptr::null_mut());
                // Build a temp handle for recursion — we need the PopMenu variant
                let temp_handle = match parent_handle {
                    nwg::ControlHandle::Hwnd(h) => nwg::ControlHandle::PopMenu(*h, sub_hmenu),
                    nwg::ControlHandle::PopMenu(h, _) => nwg::ControlHandle::PopMenu(*h, sub_hmenu),
                };
                let child_leaves = build_nwg_menu_items(&temp_handle, children, next_id)?;
                leaves.extend(child_leaves);
            } else {
                // Leaf item
                let mut menu_item = nwg::MenuItem::default();
                let id = *next_id;
                *next_id += 1;
                nwg::MenuItem::builder()
                    .text(&format!("&{}", item.label))
                    .parent(parent_handle.clone())
                    .build(&mut menu_item)?;
                leaves.push((id, item.detailed_action.clone()));
            }
        }
        Ok(leaves)
    }

    #[derive(Clone, Copy)]
    pub enum Orientation {
        Horizontal = 0,
        Vertical = 1,
    }
}

#[cfg(windows)]
pub use nwg_backend::*;
