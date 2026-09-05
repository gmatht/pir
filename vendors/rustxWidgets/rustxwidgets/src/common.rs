//! Common type definitions shared across all backends.

// Platform-specific type re-exports using cfg
macro_rules! platform_module {
    ($backend:path, $Orientation:ident) => {
        pub use $backend::{Window, BoxWidget, Label, Entry, Canvas, Menu, SimpleAction, MenuBar, Dialog, DropDown, CheckButton, RadioButton, TextView, Orientation as $Orientation};
        pub type PlatformWindow = Window;
        pub type PlatformWidgetBox = BoxWidget;
        pub type PlatformLabel = Label;
        pub type PlatformEntry = Entry;
        pub type PlatformCanvas = Canvas;
        pub type PlatformMenu = Menu;
        pub type PlatformSimpleAction = SimpleAction;
        pub type PlatformMenuBar = MenuBar;
        pub type PlatformDialog = Dialog;
        pub type PlatformDropDown = DropDown;
        pub type PlatformCheckButton = CheckButton;
        pub type PlatformRadioButton = RadioButton;
        pub type PlatformTextView = TextView;
    };
}

#[cfg(all(feature = "gtk4-rs", target_os = "linux", not(feature = "zork")))]
mod platform {
    platform_module!(crate::backends_gtk_adapter, GtkOrientation);
}

#[cfg(all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs")))]
mod platform {
    platform_module!(crate::backends_gtk_adapter, GtkOrientation);
}

#[cfg(all(windows, not(feature = "zork")))]
mod platform {
    platform_module!(crate::backends_nwg_adapter, NwgOrientation);
}

#[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
mod platform {
    platform_module!(crate::backends_pancurses_adapter, PancursesOrientation);
}

#[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
mod platform {
    platform_module!(crate::backends_wasm_adapter, WasmOrientation);
}

#[cfg(all(target_os = "android", not(feature = "zork")))]
mod platform {
    platform_module!(crate::backends_android_adapter, AndroidOrientation);
}

#[cfg(feature = "zork")]
mod platform {
    platform_module!(crate::backends_zork_adapter, ZorkOrientation);
}

macro_rules! common_types_mod {
    () => {
        use super::platform::*;
        use crate::core::Widget;

        #[derive(Clone)]
        pub struct Window { pub inner: PlatformWindow }
        #[derive(Clone)]
        pub struct WidgetBox { pub inner: PlatformWidgetBox }
        #[derive(Clone)]
        pub struct Label { pub inner: PlatformLabel }
        #[derive(Clone)]
        pub struct Entry { pub inner: PlatformEntry }
        #[derive(Clone)]
        pub struct Canvas { pub inner: PlatformCanvas }
        #[derive(Clone)]
        pub struct Menu { pub inner: PlatformMenu }
        #[derive(Clone)]
        pub struct SimpleAction { pub inner: PlatformSimpleAction }
        #[derive(Clone)]
        pub struct MenuBar { pub inner: PlatformMenuBar }
        #[derive(Clone)]
        pub struct Dialog { pub inner: PlatformDialog }

        impl Window {
            pub fn set_title(&self, title: &str) { self.inner.set_title(title); }
            pub fn set_default_size(&self, w: i32, h: i32) { self.inner.set_default_size(w, h); }
            pub fn present(&self) { self.inner.present(); }
            pub fn insert_action_group(&self, name: &str, group_ptr: *mut std::os::raw::c_void) { unsafe { self.inner.insert_action_group(name, group_ptr); } }
            pub fn hwnd(&self) -> *mut std::os::raw::c_void { self.inner.hwnd() }
            pub fn set_child_box(&self, bx: &WidgetBox) { self.inner.set_child_box(&bx.inner); }
        }
        impl WidgetBox {
            pub fn append(&self, child: &impl AsRef<*mut std::os::raw::c_void>) { self.inner.append(child); }
            pub fn set_child_hexpand(&self, child: &impl AsRef<*mut std::os::raw::c_void>, expand: bool) { self.inner.set_child_hexpand(child, expand); }
            pub fn set_child_vexpand(&self, child: &impl AsRef<*mut std::os::raw::c_void>, expand: bool) { self.inner.set_child_vexpand(child, expand); }
            pub fn set_hexpand(&self, expand: bool) { self.inner.set_hexpand(expand); }
        }
        impl AsRef<*mut std::os::raw::c_void> for WidgetBox {
            fn as_ref(&self) -> &*mut std::os::raw::c_void { self.inner.as_ref() }
        }
        impl Label {
            pub fn set_text(&self, text: &str) { self.inner.set_text(text); }
            pub fn get_text(&self) -> Option<String> { self.inner.get_text() }
            pub fn set_visible(&self, visible: bool) { self.inner.set_visible(visible); }
            pub fn set_markup(&self, markup: &str) { self.inner.set_markup(markup); }
            pub fn raw_handle(&self) -> *mut std::os::raw::c_void { self.inner.raw_handle() }
        }
        impl AsRef<*mut std::os::raw::c_void> for Label {
            fn as_ref(&self) -> &*mut std::os::raw::c_void { self.inner.as_ref() }
        }
        impl Entry {
            pub fn set_text(&self, text: &str) { self.inner.set_text(text); }
            pub fn get_text(&self) -> Option<String> { self.inner.get_text() }
            pub fn grab_focus(&self) { self.inner.grab_focus(); }
            pub fn set_hexpand(&self, expand: bool) { self.inner.set_hexpand(expand); }
            pub fn set_vexpand(&self, expand: bool) { self.inner.set_vexpand(expand); }
            pub fn set_size_request(&self, w: i32, h: i32) { self.inner.set_size_request(w, h); }
            pub fn set_visible(&self, v: bool) { self.inner.set_visible(v); }
            pub fn connect_changed(&self, f: impl FnMut() + 'static) -> Result<u64, crate::Error> { self.inner.connect_changed(f) }
            pub fn on_key_raw(&self, cb: Box<dyn FnMut(u32, u32) -> bool>) { self.inner.on_key_raw(cb); }
            pub fn add_class(&self, class_name: &str) { self.inner.add_class(class_name); }
            pub fn remove_class(&self, class_name: &str) { self.inner.remove_class(class_name); }
            pub fn set_halign(&self, align: i32) { self.inner.set_halign(align); }
            pub fn set_valign(&self, align: i32) { self.inner.set_valign(align); }
            pub fn set_width_chars(&self, n: i32) { self.inner.set_width_chars(n); }
            pub fn connect_activate<F: FnMut(*mut std::os::raw::c_void) + 'static>(&self, f: F) -> Result<u64, crate::Error> { self.inner.connect_activate(f) }
            pub fn connect_focus_in_event<F: FnMut(*mut std::os::raw::c_void) -> i32 + 'static>(&self, f: F) -> Result<u64, crate::Error> { self.inner.connect_focus_in_event(f) }
            pub fn connect_focus_out_event<F: FnMut(*mut std::os::raw::c_void) -> i32 + 'static>(&self, f: F) -> Result<u64, crate::Error> { self.inner.connect_focus_out_event(f) }
            pub fn set_margin_start(&self, px: i32) { self.inner.set_margin_start(px); }
            pub fn set_margin_top(&self, px: i32) { self.inner.set_margin_top(px); }
        }
        impl AsRef<*mut std::os::raw::c_void> for Entry {
            fn as_ref(&self) -> &*mut std::os::raw::c_void { self.inner.as_ref() }
        }
        impl Canvas {
            pub fn set_draw_callback(&self, cb: Box<dyn FnMut(&mut dyn crate::core::DrawContext, i32, i32)>) { self.inner.set_draw_callback(cb); }
            pub fn queue_redraw(&self) { self.inner.queue_redraw(); }
            pub fn set_size_request(&self, w: i32, h: i32) { self.inner.set_size_request(w, h); }
            pub fn on_click(&self, cb: Box<dyn FnMut(f64, f64)>) { self.inner.on_click(cb); }
            pub fn set_content_size(&self, w: i32, h: i32) { self.inner.set_content_size(w, h); }
            pub fn grab_focus(&self) { self.inner.grab_focus(); }
            pub fn set_can_focus(&self, can: bool) { self.inner.set_can_focus(can); }
            pub fn on_key_raw(&self, cb: Box<dyn FnMut(u32, u32) -> bool>) { self.inner.on_key_raw(cb); }
            /// Force an immediate draw by rendering directly to the window
            /// surface.  `fallback_w`/`fallback_h` are used when the platform
            /// surface reports zero dimensions (the display server hasn't
            /// configured the surface yet).  On non-GTK backends this is a no-op.
            pub fn force_draw(&self, window_ptr: *mut std::os::raw::c_void, fallback_w: i32, fallback_h: i32) { self.inner.force_draw(window_ptr, fallback_w, fallback_h); }
        }
        impl Window {
            pub fn on_event(&self, cb: Box<dyn FnMut(*mut std::os::raw::c_void) -> i32>) { self.inner.on_event(cb); }
            pub fn on_event_key(&self, cb: Box<dyn FnMut(u32, u32) -> i32>) { self.inner.on_event_key(cb); }
            pub fn on_close(&self, cb: Box<dyn FnMut()>) { self.inner.on_close(cb); }
            pub fn queue_redraw(&self) { self.inner.queue_redraw(); }
        }
        impl AsRef<*mut std::os::raw::c_void> for Canvas {
            fn as_ref(&self) -> &*mut std::os::raw::c_void { self.inner.as_ref() }
        }
        impl Menu {
            pub fn append(&mut self, label: &str, detailed_action: &str) { self.inner.append(label, detailed_action); }
            pub fn append_submenu(&mut self, label: &str, submenu: &Menu) { self.inner.append_submenu(label, &submenu.inner); }
        }
        impl SimpleAction {
            pub fn connect_activate<F: FnMut(*mut std::os::raw::c_void) + 'static>(&self, f: F) -> Result<u64, crate::Error> { self.inner.connect_activate(f) }
        }
        impl MenuBar {
            pub fn activate_submenu_by_mnemonic(&self, keyval: u32) -> bool {
                self.inner.activate_submenu_by_mnemonic(keyval)
            }
            pub fn activate_submenu_item_by_mnemonic(&self, keyval: u32) -> bool {
                self.inner.activate_submenu_item_by_mnemonic(keyval)
            }
            pub unsafe fn insert_action_group(&self, name: &str, group_ptr: *mut std::os::raw::c_void) {
                self.inner.insert_action_group(name, group_ptr);
            }
            /// Handle a mnemonic keypress when a submenu popover may be open.
            /// Returns true if the key was consumed (matched a menu item).
            /// Returns false if no match — caller should fall through to normal key handling.
            /// On NWG/WASM/Pancurses/Zork this is a no-op (returns false).
            pub fn handle_mnemonic_key(&self, keyval: u32) -> bool {
                self.inner.handle_mnemonic_key(keyval)
            }
            /// Handle any menu-related key event (Alt+letter to open submenu,
            /// Escape to close, printable to select, Up/Down to navigate).
            /// `modifiers` is a bitmask of GDK modifier state (1=Shift, 4=Control, 8=Alt/Meta).
            /// Returns true if the key was consumed by menu handling.
            /// Callers should also check `menu_active()` to prevent normal key handling.
            pub fn handle_menu_key(&self, keyval: u32, modifiers: u32) -> bool {
                self.inner.handle_menu_key(keyval, modifiers)
            }
            /// Returns true if a keyboard menu is currently open (Alt+letter was pressed,
            /// menu is active for navigation).  Callers can use this to skip normal
            /// key handling when the menu is open.
            pub fn menu_active(&self) -> bool {
                self.inner.menu_active()
            }
            /// Close any open keyboard menu and dismiss visible popovers.
            pub fn menu_close(&self) {
                self.inner.menu_close();
            }
        }
        impl AsRef<*mut std::os::raw::c_void> for MenuBar {
            fn as_ref(&self) -> &*mut std::os::raw::c_void { self.inner.as_ref() }
        }
        impl Dialog {
            pub fn set_title(&self, title: &str) { self.inner.set_title(title); }
            pub fn set_default_size(&self, w: i32, h: i32) { self.inner.set_default_size(w, h); }
            pub fn append_content_area(&self, child: &impl AsRef<*mut std::os::raw::c_void>) { self.inner.append_content_area(child); }
            pub fn add_button(&self, text: &str, response_id: i32) { self.inner.add_button(text, response_id); }
            pub fn present(&self) { self.inner.present(); }
            pub fn connect_response<F: FnMut(i32) + 'static>(&self, f: F) -> Result<u64, crate::Error> { self.inner.connect_response(f) }
            pub fn close(&self) { self.inner.close(); }
        }
        impl AsRef<*mut std::os::raw::c_void> for Dialog {
            fn as_ref(&self) -> &*mut std::os::raw::c_void { self.inner.as_ref() }
        }
    }
}

// Common wrapper types with `inner` field for the platform-specific types
#[cfg(all(feature = "gtk4-rs", target_os = "linux", not(feature = "zork")))]
mod common_types {
    common_types_mod!();
    impl Canvas {
        pub fn on_key(&self, mut cb: Box<dyn FnMut(u32) -> bool>) {
            self.inner.on_key(Box::new(move |k: u32, _s: u32| -> bool { cb(k) }));
        }
    }
}

#[cfg(all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs")))]
mod common_types {
    common_types_mod!();
    impl Canvas {
        pub fn on_key(&self, mut cb: Box<dyn FnMut(u32) -> bool>) {
            self.inner.on_key(Box::new(move |k: u32, _s: u32| -> bool { cb(k) }));
        }
    }
}

#[cfg(all(windows, not(feature = "zork")))]
mod common_types {
    common_types_mod!();
    impl Canvas {
        pub fn on_key(&self, cb: Box<dyn FnMut(u32) -> bool>) { self.inner.on_key(cb); }
    }
    impl Entry {
        pub fn on_key(&self, f: Box<dyn FnMut(u32) -> bool>) { self.inner.on_key(f); }
    }
}

#[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
mod common_types { common_types_mod!(); }

#[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
mod common_types {
    common_types_mod!();
    impl Canvas {
        pub fn on_key(&self, cb: Box<dyn FnMut(u32) -> bool>) { self.inner.on_key(cb); }
    }
}

#[cfg(all(target_os = "android", not(feature = "zork")))]
mod common_types { common_types_mod!(); }

#[cfg(feature = "zork")]
mod common_types { common_types_mod!(); }

#[cfg(all(feature = "gtk4-rs", target_os = "linux", not(feature = "zork")))]
pub use common_types::{Window, WidgetBox, Label, Entry, Canvas, Menu, SimpleAction, MenuBar, Dialog};

#[cfg(all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs")))]
pub use common_types::{Window, WidgetBox, Label, Entry, Canvas, Menu, SimpleAction, MenuBar, Dialog};

#[cfg(all(windows, not(feature = "zork")))]
pub use common_types::{Window, WidgetBox, Label, Entry, Canvas, Menu, SimpleAction, MenuBar, Dialog};

#[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
pub use common_types::{Window, WidgetBox, Label, Entry, Canvas, Menu, SimpleAction, MenuBar, Dialog};

#[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
pub use common_types::{Window, WidgetBox, Label, Entry, Canvas, Menu, SimpleAction, MenuBar, Dialog};

#[cfg(all(target_os = "android", not(feature = "zork")))]
pub use common_types::{Window, WidgetBox, Label, Entry, Canvas, Menu, SimpleAction, MenuBar, Dialog};

#[cfg(feature = "zork")]
pub use common_types::{Window, WidgetBox, Label, Entry, Canvas, Menu, SimpleAction, MenuBar, Dialog};

// Re-export Orientation from the active platform backend
#[cfg(all(feature = "gtk4-rs", target_os = "linux", not(feature = "zork")))]
pub use self::platform::GtkOrientation as Orientation;
#[cfg(all(feature = "gtk", target_os = "linux", not(feature = "zork"), not(feature = "gtk4-rs")))]
pub use self::platform::GtkOrientation as Orientation;
#[cfg(all(windows, not(feature = "zork")))]
pub use self::platform::NwgOrientation as Orientation;
#[cfg(all(feature = "pancurses", not(any(feature = "gtk", windows, target_arch = "wasm32", target_os = "android"))))]
pub use self::platform::PancursesOrientation as Orientation;
#[cfg(all(target_arch = "wasm32", not(feature = "zork")))]
pub use self::platform::WasmOrientation as Orientation;
#[cfg(all(target_os = "android", not(feature = "zork")))]
pub use self::platform::AndroidOrientation as Orientation;
#[cfg(feature = "zork")]
pub use self::platform::ZorkOrientation as Orientation;

// -- Shared menu definition types (no platform deps) --

/// A single menu item: label + action name + optional nested submenu.
#[derive(Clone, Copy)]
pub struct MenuItemDef {
    pub label: &'static str,
    pub action: &'static str,
    pub submenu: Option<&'static [MenuItemDef]>,
}

/// A submenu: label + prefix for action names + items.
#[derive(Clone, Copy)]
pub struct SubmenuDef {
    pub label: &'static str,
    pub prefix: &'static str,
    pub items: &'static [MenuItemDef],
}
