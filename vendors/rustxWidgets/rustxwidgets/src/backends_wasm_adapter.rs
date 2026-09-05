#[cfg(target_arch = "wasm32")]
mod wasm_adapter {
    use std::any::Any;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::os::raw::c_void;
    use std::rc::Rc;
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::JsCast;
    use web_sys::{
        Document, Element, Event, FocusEvent, HtmlButtonElement, HtmlCanvasElement,
        HtmlDialogElement, HtmlDivElement, HtmlElement, HtmlInputElement, HtmlOptionElement,
        HtmlSelectElement, HtmlTextAreaElement, KeyboardEvent, MouseEvent,
    };
    use crate::core::{Error, Widget};

    // -----------------------------------------------------------------------
    // Global helpers
    // -----------------------------------------------------------------------
    fn document() -> Document {
        web_sys::window().unwrap().document().unwrap()
    }

    fn body() -> HtmlElement {
        document().body().unwrap()
    }

    fn create_element(tag: &str) -> Element {
        document().create_element(tag).unwrap()
    }

    pub fn quit_main_loop() {
        // On WASM there is no message loop to quit.  Signal the test
        // framework that the app has quit, and try to close the tab.
        if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
            // Set document title as a signal for test frameworks
            // that may poll for it.
            doc.set_title("CORRO_QUIT");
        }
        if let Some(win) = web_sys::window() {
            let _ = win.close();
        }
    }

    /// Store a line of output in a JS global (`window.__corro_output`).
    /// The browser test framework can read this variable after the app quits
    /// to verify the recording replay output.
    pub fn append_wasm_output_line(line: &str) {
        if let Some(win) = web_sys::window() {
            let js_val = wasm_bindgen::JsValue::from_str(line);
            let _ = js_sys::Reflect::set(
                &win,
                &wasm_bindgen::JsValue::from_str("__corro_output_dirty"),
                &wasm_bindgen::JsValue::TRUE,
            );
            let current = js_sys::Reflect::get(
                &win,
                &wasm_bindgen::JsValue::from_str("__corro_output"),
            )
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default();
            let new = if current.is_empty() {
                line.to_string()
            } else {
                format!("{}\n{}", current, line)
            };
            let _ = js_sys::Reflect::set(
                &win,
                &wasm_bindgen::JsValue::from_str("__corro_output"),
                &wasm_bindgen::JsValue::from_str(&new),
            );
        }
    }

    fn set_css(elem: &Element, prop: &str, val: &str) {
        if let Some(html) = elem.dyn_ref::<HtmlElement>() {
            html.style().set_property(prop, val).ok();
        }
    }

    // -----------------------------------------------------------------------
    // AsElement trait – unified way to get a DOM Element from any widget
    // -----------------------------------------------------------------------
    pub trait AsElement {
        fn as_element(&self) -> &Element;
    }

    // -----------------------------------------------------------------------
    // Action registry (single‑threaded WASM, safe under wasm32)
    // -----------------------------------------------------------------------
    struct SendFnPtr(*mut dyn FnMut(*mut c_void));
    unsafe impl Send for SendFnPtr {}

    fn register_action(name: &str, f: Box<dyn FnMut(*mut c_void)>) {
        use once_cell::sync::Lazy;
        use std::sync::Mutex;
        static REG: Lazy<Mutex<HashMap<String, SendFnPtr>>> =
            Lazy::new(|| Mutex::new(HashMap::new()));
        let mut map = REG.lock().unwrap();
        let ptr = Box::into_raw(Box::new(f) as Box<dyn FnMut(*mut c_void)>);
        map.insert(name.to_owned(), SendFnPtr(ptr));
    }

    fn invoke_action(name: &str, param: *mut c_void) {
        use once_cell::sync::Lazy;
        use std::sync::Mutex;
        static REG: Lazy<Mutex<HashMap<String, SendFnPtr>>> =
            Lazy::new(|| Mutex::new(HashMap::new()));
        if let Ok(mut map) = REG.lock() {
            if let Some(SendFnPtr(ptr)) = map.get_mut(name) {
                let cb: &mut dyn FnMut(*mut c_void) = unsafe { &mut **ptr };
                cb(param);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Global closure storage – prevents Closures from being dropped while
    // DOM event listeners still reference them after main() returns.
    // -----------------------------------------------------------------------
    struct AnySend(Box<dyn Any>);
    unsafe impl Send for AnySend {}

    fn store_closure(c: Box<dyn Any>) {
        use once_cell::sync::Lazy;
        use std::sync::Mutex;
        static CLOSURES: Lazy<Mutex<Vec<AnySend>>> =
            Lazy::new(|| Mutex::new(Vec::new()));
        CLOSURES.lock().unwrap().push(AnySend(c));
    }

    // -----------------------------------------------------------------------
    // Orientation
    // -----------------------------------------------------------------------
    pub enum Orientation {
        Horizontal,
        Vertical,
    }

    impl Orientation {
        pub fn as_flex_direction(&self) -> &'static str {
            match self {
                Orientation::Horizontal => "row",
                Orientation::Vertical => "column",
            }
        }
    }

    // -----------------------------------------------------------------------
    // Window
    // -----------------------------------------------------------------------
pub struct Window {
    elem: HtmlDivElement,
    event_key_cb: Rc<RefCell<Option<Box<dyn FnMut(u32, u32) -> i32>>>>,
    closures: Rc<RefCell<Vec<Box<dyn Any>>>>,
}

impl Clone for Window {
    fn clone(&self) -> Self {
        Window {
            elem: self.elem.clone(),
            event_key_cb: self.event_key_cb.clone(),
            closures: self.closures.clone(),
        }
    }
}

impl AsElement for Window {
        fn as_element(&self) -> &Element {
            self.elem.as_ref()
        }
    }

    impl Widget for Window {
        fn raw_handle(&self) -> *mut c_void {
            &self.elem as *const HtmlDivElement as *mut c_void
        }
    }

    impl AsRef<*mut c_void> for Window {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.raw_handle() as *const *mut c_void) }
        }
    }

impl Window {
    pub fn set_title(&self, title: &str) {
        self.elem.set_text_content(Some(title));
    }

    pub fn set_child(&self, child: &impl AsElement) {
        self.elem.append_child(child.as_element()).ok();
    }

    pub fn set_child_box(&self, child: &impl AsElement) {
        self.elem.append_child(child.as_element()).ok();
    }

    pub fn present(&self) {}

    pub fn hwnd(&self) -> *mut c_void {
        &self.elem as *const HtmlDivElement as *mut c_void
    }

    pub fn set_default_size(&self, w: i32, h: i32) {
            if w > 0 {
                set_css(self.elem.as_ref(), "width", &format!("{}px", w));
            }
            if h > 0 {
                set_css(self.elem.as_ref(), "height", &format!("{}px", h));
            }
        }

        /// # Safety – kept for API compatibility; no‑op on WASM.
        pub unsafe fn insert_action_group(&self, _name: &str, _group_ptr: *mut c_void) {}
        pub fn on_event(&self, _cb: Box<dyn FnMut(*mut c_void) -> i32>) {}
        pub fn on_close(&self, _cb: Box<dyn FnMut()>) {}
        pub fn queue_redraw(&self) {}
        pub fn on_event_key(&self, mut cb: Box<dyn FnMut(u32, u32) -> i32>) {
            *self.event_key_cb.borrow_mut() = Some(cb);
            let cb2 = self.event_key_cb.clone();
            let closure = Closure::<dyn FnMut(KeyboardEvent)>::new(move |evt: KeyboardEvent| {
                let keyval = evt.key_code() as u32;
                let mut state = 0u32;
                if evt.alt_key() {
                    state |= 0x8; // GDK_MOD1_MASK / ALT
                }
                if let Some(cb) = cb2.borrow_mut().as_mut() {
                    if cb(keyval, state) != 0 {
                        evt.prevent_default();
                    }
                }
            });
            let listener = wasm_bindgen::JsCast::unchecked_into::<js_sys::Function>(
                closure.as_ref().clone(),
            );
            web_sys::window()
                .and_then(|w| w.document())
                .map(|doc| {
                    doc.add_event_listener_with_callback("keydown", &listener).ok();
                });
            self.closures.borrow_mut().push(Box::new(closure));
        }
    }

    pub fn create_window() -> Result<Window, Error> {
        let div: HtmlDivElement = create_element("div").dyn_into().map_err(|e| {
            Error::Backend(format!("create_window: {:?}", e))
        })?;
        set_css(div.as_ref(), "all", "initial");
        body().append_child(div.as_ref()).map_err(|e| {
            Error::Backend(format!("create_window append: {:?}", e))
        })?;
        Ok(Window {
            elem: div,
            event_key_cb: Rc::new(RefCell::new(None)),
            closures: Rc::new(RefCell::new(Vec::new())),
        })
    }

    // -----------------------------------------------------------------------
    // Button
    // -----------------------------------------------------------------------
    pub struct Button {
        elem: HtmlButtonElement,
        closures: Rc<RefCell<Vec<Box<dyn Any>>>>,
        next_id: Rc<RefCell<u64>>,
    }

    impl AsElement for Button {
        fn as_element(&self) -> &Element {
            self.elem.as_ref()
        }
    }

    impl Widget for Button {
        fn raw_handle(&self) -> *mut c_void {
            &self.elem as *const HtmlButtonElement as *mut c_void
        }
    }

    impl Clone for Button {
        fn clone(&self) -> Self {
            Button {
                elem: self.elem.clone(),
                closures: self.closures.clone(),
                next_id: self.next_id.clone(),
            }
        }
    }

    impl Button {
        pub fn on_click(&self, f: impl FnMut() + 'static) -> Result<u64, Error> {
            let cb = Rc::new(RefCell::new(f));
            let cb2 = cb.clone();
            let closure = Closure::<dyn FnMut(MouseEvent)>::new(move |_: MouseEvent| {
                (cb2.borrow_mut())();
            });
            self.elem
                .add_event_listener_with_callback("click", closure.as_ref().unchecked_ref())
                .map_err(|e| Error::Backend(format!("on_click: {:?}", e)))?;
            let id = *self.next_id.borrow();
            *self.next_id.borrow_mut() += 1;
            store_closure(Box::new(closure));
            Ok(id)
        }

        pub fn emit_clicked(&self) -> Result<u64, Error> {
            self.elem.click();
            Ok(0)
        }

        pub fn set_size_request(&self, w: i32, h: i32) {
            if w > 0 {
                set_css(self.elem.as_ref(), "width", &format!("{}px", w));
            }
            if h > 0 {
                set_css(self.elem.as_ref(), "height", &format!("{}px", h));
            }
        }

        pub fn set_font_style(&self, weight: i32, italic: bool) {
            set_css(self.elem.as_ref(), "font-weight", &weight.to_string());
            set_css(self.elem.as_ref(), "font-style", if italic { "italic" } else { "normal" });
        }
    }

    pub fn create_button(label: &str) -> Result<Button, Error> {
        let btn: HtmlButtonElement = create_element("button").dyn_into().map_err(|e| {
            Error::Backend(format!("create_button: {:?}", e))
        })?;
        btn.set_text_content(Some(label));
        Ok(Button {
            elem: btn,
            closures: Rc::new(RefCell::new(Vec::new())),
            next_id: Rc::new(RefCell::new(1)),
        })
    }

    // -----------------------------------------------------------------------
    // Label
    // -----------------------------------------------------------------------
    pub struct Label {
        elem: Element,
    }

    impl AsElement for Label {
        fn as_element(&self) -> &Element {
            &self.elem
        }
    }

    impl Clone for Label {
        fn clone(&self) -> Self {
            Label {
                elem: self.elem.clone(),
            }
        }
    }

    impl Widget for Label {
        fn raw_handle(&self) -> *mut c_void {
            &self.elem as *const Element as *mut c_void
        }
    }

    impl AsRef<*mut c_void> for Label {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.raw_handle() as *const *mut c_void) }
        }
    }

    impl Label {
        pub fn set_text(&self, text: &str) {
            self.elem.set_text_content(Some(text));
        }

        pub fn get_text(&self) -> Option<String> {
            self.elem.text_content()
        }

        pub fn add_class(&self, class_name: &str) {
            self.elem.class_list().add_1(class_name).ok();
        }

        pub fn remove_class(&self, class_name: &str) {
            self.elem.class_list().remove_1(class_name).ok();
        }

        pub fn set_markup(&self, markup: &str) {
            self.elem.set_inner_html(markup);
        }

        pub fn set_visible(&self, visible: bool) {
            if let Some(html) = self.elem.dyn_ref::<HtmlElement>() {
                if visible {
                    html.style().set_property("display", "").ok();
                } else {
                    html.style().set_property("display", "none").ok();
                }
            }
        }

        pub fn set_xalign(&self, x: f32) {
            let align = if x <= 0.0 {
                "left"
            } else if x >= 1.0 {
                "right"
            } else {
                "center"
            };
            if let Some(html) = self.elem.dyn_ref::<HtmlElement>() {
                html.style().set_property("text-align", align).ok();
            }
        }
    }

    pub fn create_label(text: &str) -> Result<Label, Error> {
        let elem = create_element("span");
        elem.set_text_content(Some(text));
        Ok(Label { elem })
    }

    // -----------------------------------------------------------------------
    // BoxWidget
    // -----------------------------------------------------------------------
pub struct BoxWidget {
    elem: HtmlDivElement,
}

impl Clone for BoxWidget {
    fn clone(&self) -> Self {
        BoxWidget { elem: self.elem.clone() }
    }
}

impl AsElement for BoxWidget {
        fn as_element(&self) -> &Element {
            self.elem.as_ref()
        }
    }

    impl Widget for BoxWidget {
        fn raw_handle(&self) -> *mut c_void {
            &self.elem as *const HtmlDivElement as *mut c_void
        }
    }

    impl AsRef<*mut c_void> for BoxWidget {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.raw_handle() as *const *mut c_void) }
        }
    }

    fn as_element_from_ptr(ptr: *mut c_void) -> &'static Element {
        unsafe { &*(ptr as *const Element) }
    }

    fn as_html_element(ptr: *mut c_void) -> Option<&'static HtmlElement> {
        unsafe { (*(ptr as *const Element)).dyn_ref::<HtmlElement>() }
    }

    impl BoxWidget {
        pub fn append(&self, child: &impl AsRef<*mut c_void>) {
            self.elem.append_child(as_element_from_ptr(*child.as_ref())).ok();
        }

        pub fn set_child_vexpand(&self, child: &impl AsRef<*mut c_void>, expand: bool) {
            if let Some(html) = as_html_element(*child.as_ref()) {
                if expand {
                    html.style().set_property("flex-grow", "1").ok();
                    html.style().set_property("align-self", "stretch").ok();
                } else {
                    html.style().set_property("flex-grow", "0").ok();
                }
            }
        }

        pub fn set_child_hexpand(&self, child: &impl AsRef<*mut c_void>, expand: bool) {
            if let Some(html) = as_html_element(*child.as_ref()) {
                if expand {
                    html.style().set_property("align-self", "stretch").ok();
                }
            }
        }

        pub fn set_hexpand(&self, _expand: bool) {
        }
    }

    pub fn create_box(orientation: Orientation, spacing: i32) -> Result<BoxWidget, Error> {
        let div: HtmlDivElement = create_element("div").dyn_into().map_err(|e| {
            Error::Backend(format!("create_box: {:?}", e))
        })?;
        let s = div.style();
        s.set_property("display", "flex").ok();
        s.set_property("flex-direction", orientation.as_flex_direction())
            .ok();
        if spacing > 0 {
            s.set_property("gap", &format!("{}px", spacing)).ok();
        }
        Ok(BoxWidget { elem: div })
    }

    // -----------------------------------------------------------------------
    // Grid
    // -----------------------------------------------------------------------
    pub struct Grid {
        elem: HtmlDivElement,
    }

    impl AsElement for Grid {
        fn as_element(&self) -> &Element {
            self.elem.as_ref()
        }
    }

    impl Widget for Grid {
        fn raw_handle(&self) -> *mut c_void {
            &self.elem as *const HtmlDivElement as *mut c_void
        }
    }

    impl Grid {
        pub fn attach(&self, child: &impl AsElement, left: i32, top: i32, width: i32, height: i32) {
            let child = child.as_element();
            if let Some(html) = child.dyn_ref::<HtmlElement>() {
                let s = html.style();
                s.set_property(
                    "grid-column",
                    &format!("{} / {}", left + 1, left + width + 1),
                )
                .ok();
                s.set_property(
                    "grid-row",
                    &format!("{} / {}", top + 1, top + height + 1),
                )
                .ok();
            }
            self.elem.append_child(child).ok();
        }
    }

    pub fn create_grid() -> Result<Grid, Error> {
        let div: HtmlDivElement = create_element("div").dyn_into().map_err(|e| {
            Error::Backend(format!("create_grid: {:?}", e))
        })?;
        div.style().set_property("display", "grid").ok();
        Ok(Grid { elem: div })
    }

    // -----------------------------------------------------------------------
    // Entry
    // -----------------------------------------------------------------------
    pub struct Entry {
        elem: HtmlInputElement,
        key_cb: Rc<RefCell<Option<Box<dyn FnMut(u32, u32) -> bool>>>>,
        closures: Rc<RefCell<Vec<Box<dyn Any>>>>,
        next_id: Rc<RefCell<u64>>,
    }

    impl AsElement for Entry {
        fn as_element(&self) -> &Element {
            self.elem.as_ref()
        }
    }

    impl Widget for Entry {
        fn raw_handle(&self) -> *mut c_void {
            &self.elem as *const HtmlInputElement as *mut c_void
        }
    }

    impl Clone for Entry {
        fn clone(&self) -> Self {
            Entry {
                elem: self.elem.clone(),
                key_cb: self.key_cb.clone(),
                closures: self.closures.clone(),
                next_id: self.next_id.clone(),
            }
        }
    }

    impl AsRef<*mut c_void> for Entry {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.raw_handle() as *const *mut c_void) }
        }
    }

    impl Entry {
        pub fn set_text(&self, text: &str) {
            self.elem.set_value(text);
        }

        pub fn get_text(&self) -> Option<String> {
            Some(self.elem.value())
        }

        pub fn set_width_chars(&self, n: i32) {
            self.elem.set_size(n as u32);
        }

        pub fn set_size_request(&self, w: i32, h: i32) {
            if w > 0 {
                set_css(self.elem.as_ref(), "width", &format!("{}px", w));
            }
            if h > 0 {
                set_css(self.elem.as_ref(), "height", &format!("{}px", h));
            }
        }

        pub fn set_hexpand(&self, _expand: bool) {
        }

        pub fn connect_changed(&self, f: impl FnMut() + 'static) -> Result<u64, Error> {
            let cb = Rc::new(RefCell::new(f));
            let cb2 = cb.clone();
            let closure = Closure::<dyn FnMut(Event)>::new(move |_: Event| {
                (cb2.borrow_mut())();
            });
            self.elem
                .add_event_listener_with_callback("input", closure.as_ref().unchecked_ref())
                .map_err(|e| Error::Backend(format!("connect_changed: {:?}", e)))?;
            let id = *self.next_id.borrow();
            *self.next_id.borrow_mut() += 1;
            store_closure(Box::new(closure));
            Ok(id)
        }

        pub fn connect_activate<F: FnMut(*mut c_void) + 'static>(
            &self,
            f: F,
        ) -> Result<u64, Error> {
            let cb = Rc::new(RefCell::new(f));
            let cb2 = cb.clone();
            let closure =
                Closure::<dyn FnMut(KeyboardEvent)>::new(move |evt: KeyboardEvent| {
                    if evt.key() == "Enter" {
                        (cb2.borrow_mut())(std::ptr::null_mut());
                    }
                });
            self.elem
                .add_event_listener_with_callback("keydown", closure.as_ref().unchecked_ref())
                .map_err(|e| Error::Backend(format!("connect_activate: {:?}", e)))?;
            let id = *self.next_id.borrow();
            *self.next_id.borrow_mut() += 1;
            store_closure(Box::new(closure));
            Ok(id)
        }

        pub fn connect_button_press(&self, f: impl FnMut() + 'static) -> Result<u64, Error> {
            let cb = Rc::new(RefCell::new(f));
            let cb2 = cb.clone();
            let closure = Closure::<dyn FnMut(MouseEvent)>::new(move |_: MouseEvent| {
                (cb2.borrow_mut())();
            });
            self.elem
                .add_event_listener_with_callback("click", closure.as_ref().unchecked_ref())
                .map_err(|e| Error::Backend(format!("connect_button_press: {:?}", e)))?;
            let id = *self.next_id.borrow();
            *self.next_id.borrow_mut() += 1;
            store_closure(Box::new(closure));
            Ok(id)
        }

        pub fn add_class(&self, class_name: &str) {
            self.elem.class_list().add_1(class_name).ok();
        }

        pub fn remove_class(&self, class_name: &str) {
            self.elem.class_list().remove_1(class_name).ok();
        }

        pub fn grab_focus(&self) {
            let _ = self.elem.focus();
        }

        pub fn on_key_raw(&self, cb: Box<dyn FnMut(u32, u32) -> bool>) {
            *self.key_cb.borrow_mut() = Some(cb);
            let cb2 = self.key_cb.clone();
            let closure = Closure::<dyn FnMut(KeyboardEvent)>::new(move |evt: KeyboardEvent| {
                if let Some(ref mut f) = *cb2.borrow_mut() {
                    if f(evt.key_code(), 0) {
                        evt.prevent_default();
                    }
                }
            });
            self.elem
                .add_event_listener_with_callback("keydown", closure.as_ref().unchecked_ref())
                .ok();
            store_closure(Box::new(closure));
        }

        pub fn connect_focus_in_event<F: FnMut(*mut c_void) -> i32 + 'static>(
            &self,
            f: F,
        ) -> Result<u64, Error> {
            let cb = Rc::new(RefCell::new(f));
            let cb2 = cb.clone();
            let closure = Closure::<dyn FnMut(FocusEvent)>::new(move |_: FocusEvent| {
                (cb2.borrow_mut())(std::ptr::null_mut());
            });
            self.elem
                .add_event_listener_with_callback("focus", closure.as_ref().unchecked_ref())
                .map_err(|e| Error::Backend(format!("connect_focus_in_event: {:?}", e)))?;
            let id = *self.next_id.borrow();
            *self.next_id.borrow_mut() += 1;
            store_closure(Box::new(closure));
            Ok(id)
        }

        pub fn set_margin_start(&self, margin: i32) {
            set_css(self.elem.as_ref(), "left", &format!("{}px", margin));
        }

        pub fn set_margin_top(&self, margin: i32) {
            set_css(self.elem.as_ref(), "top", &format!("{}px", margin));
        }

        pub fn set_halign(&self, align: i32) {
            let tx = match align {
                1 => "translateX(-50%)",
                2 => "translateX(-100%)",
                _ => "",
            };
            let ty = self.elem.style().get_property_value("transform").unwrap_or_default();
            let new = if ty.contains("translateY") {
                format!("{} {}", tx, ty)
            } else {
                tx.to_string()
            };
            set_css(self.elem.as_ref(), "transform", &new);
        }

        pub fn set_valign(&self, align: i32) {
            let ty = match align {
                1 => "translateY(-50%)",
                2 => "translateY(-100%)",
                _ => "",
            };
            let tx = self.elem.style().get_property_value("transform").unwrap_or_default();
            let new = if tx.contains("translateX") {
                format!("{} {}", tx, ty)
            } else {
                ty.to_string()
            };
            set_css(self.elem.as_ref(), "transform", &new);
        }

        pub fn set_visible(&self, visible: bool) {
            if let Some(html) = self.elem.dyn_ref::<HtmlElement>() {
                if visible {
                    html.style().set_property("display", "").ok();
                } else {
                    html.style().set_property("display", "none").ok();
                }
            }
        }

        pub fn connect_focus_out_event<F: FnMut(*mut c_void) -> i32 + 'static>(
            &self,
            f: F,
        ) -> Result<u64, Error> {
            let cb = Rc::new(RefCell::new(f));
            let cb2 = cb.clone();
            let closure = Closure::<dyn FnMut(FocusEvent)>::new(move |_: FocusEvent| {
                (cb2.borrow_mut())(std::ptr::null_mut());
            });
            self.elem
                .add_event_listener_with_callback("blur", closure.as_ref().unchecked_ref())
                .map_err(|e| Error::Backend(format!("connect_focus_out_event: {:?}", e)))?;
            let id = *self.next_id.borrow();
            *self.next_id.borrow_mut() += 1;
            store_closure(Box::new(closure));
            Ok(id)
        }
    }

    pub fn create_entry() -> Result<Entry, Error> {
        let elem: HtmlInputElement = create_element("input").dyn_into().map_err(|e| {
            Error::Backend(format!("create_entry: {:?}", e))
        })?;
        elem.set_type("text");
        Ok(Entry {
            elem,
            key_cb: Rc::new(RefCell::new(None)),
            closures: Rc::new(RefCell::new(Vec::new())),
            next_id: Rc::new(RefCell::new(1)),
        })
    }

    // -----------------------------------------------------------------------
    // Menu, MenuBar, SimpleAction
    // -----------------------------------------------------------------------
    #[derive(Clone)]
    enum MenuItem {
        Item { label: String, action: String },
        Submenu { label: String, items: Vec<MenuItem> },
    }

#[derive(Clone)]
pub struct Menu {
    items: Vec<MenuItem>,
}

impl Menu {
        pub fn append(&mut self, label: &str, detailed_action: &str) {
            let action = detailed_action
                .split('(')
                .next()
                .unwrap_or(detailed_action)
                .to_owned();
            self.items.push(MenuItem::Item {
                label: label.to_owned(),
                action,
            });
        }

        pub fn append_submenu(&mut self, label: &str, submenu: &Menu) {
            self.items.push(MenuItem::Submenu {
                label: label.to_owned(),
                items: submenu.items.clone(),
            });
        }
    }

    pub fn create_menu() -> Result<Menu, Error> {
        Ok(Menu { items: Vec::new() })
    }

pub struct MenuBar {
    elem: HtmlDivElement,
}

impl Clone for MenuBar {
    fn clone(&self) -> Self {
        MenuBar { elem: self.elem.clone() }
    }
}

impl AsElement for MenuBar {
        fn as_element(&self) -> &Element {
            self.elem.as_ref()
        }
    }

    impl Widget for MenuBar {
        fn raw_handle(&self) -> *mut c_void {
            &self.elem as *const HtmlDivElement as *mut c_void
        }
    }

    impl AsRef<*mut c_void> for MenuBar {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.raw_handle() as *const *mut c_void) }
        }
    }

     impl MenuBar {
         pub fn handle_mnemonic_key(&self, _keyval: u32) -> bool { false }
         pub fn handle_menu_key(&self, _keyval: u32, _mod: u32) -> bool { false }
         pub fn menu_active(&self) -> bool { false }
         pub fn menu_close(&self) {}
     }

    pub fn create_menubar(model: &Menu, _action_group: *mut c_void) -> Result<MenuBar, Error> {
        let bar: HtmlDivElement = create_element("div").dyn_into().map_err(|e| {
            Error::Backend(format!("create_menubar: {:?}", e))
        })?;
        let s = bar.style();
        s.set_property("display", "flex").ok();
        s.set_property("background", "#f0f0f0").ok();
        s.set_property("border-bottom", "1px solid #ccc").ok();

        for item in &model.items {
            match item {
                MenuItem::Item { label, action } => {
                    let btn: HtmlButtonElement = create_element("button").dyn_into().unwrap();
                    btn.set_text_content(Some(label));
                    let action_name = action.clone();
                    let closure =
                        Closure::<dyn FnMut(MouseEvent)>::new(move |_: MouseEvent| {
                            invoke_action(&action_name, std::ptr::null_mut());
                        });
                    btn.add_event_listener_with_callback("click", closure.as_ref().unchecked_ref())
                        .ok();
                    closure.forget();
                    let s = btn.style();
                    s.set_property("background", "transparent").ok();
                    s.set_property("border", "none").ok();
                    s.set_property("padding", "4px 12px").ok();
                    s.set_property("cursor", "pointer").ok();
                    bar.append_child(btn.as_ref()).ok();
                }
                MenuItem::Submenu { label, items } => {
                    let wrapper = create_element("div");
                    set_css(&wrapper, "position", "relative");

                    let toggle: HtmlButtonElement = create_element("button").dyn_into().unwrap();
                    toggle.set_text_content(Some(label));
                    let s = toggle.style();
                    s.set_property("background", "transparent").ok();
                    s.set_property("border", "none").ok();
                    s.set_property("padding", "4px 12px").ok();
                    s.set_property("cursor", "pointer").ok();

                    let dropdown = create_element("div");
                    set_css(&dropdown, "display", "none");
                    set_css(&dropdown, "position", "absolute");
                    set_css(&dropdown, "top", "100%");
                    set_css(&dropdown, "left", "0");
                    set_css(&dropdown, "background", "#fff");
                    set_css(&dropdown, "border", "1px solid #ccc");
                    set_css(&dropdown, "z-index", "1000");

                    for sub in items {
                        match sub {
                            MenuItem::Item { label, action } => {
                                let item = create_element("div");
                                set_css(&item, "padding", "4px 12px");
                                set_css(&item, "cursor", "pointer");
                                item.set_text_content(Some(label));
                                let action_name = action.clone();
                                let cl =
                                    Closure::<dyn FnMut(MouseEvent)>::new(move |_: MouseEvent| {
                                        invoke_action(&action_name, std::ptr::null_mut());
                                    });
                                item.add_event_listener_with_callback(
                                    "click",
                                    cl.as_ref().unchecked_ref(),
                                )
                                .ok();
                                cl.forget();
                                dropdown.append_child(&item).ok();
                            }
                            _ => {}
                        }
                    }

                    let dd = dropdown.clone();
                    let toggle_closure =
                        Closure::<dyn FnMut(MouseEvent)>::new(move |_: MouseEvent| {
                            if let Some(html) = dd.dyn_ref::<HtmlElement>() {
                                let disp = html
                                    .style()
                                    .get_property_value("display")
                                    .unwrap_or_default();
                                if disp == "none" {
                                    html.style().set_property("display", "block").ok();
                                } else {
                                    html.style().set_property("display", "none").ok();
                                }
                            }
                        });
                    toggle
                        .add_event_listener_with_callback(
                            "click",
                            toggle_closure.as_ref().unchecked_ref(),
                        )
                        .ok();
                    toggle_closure.forget();

                    wrapper.append_child(toggle.as_ref()).ok();
                    wrapper.append_child(&dropdown).ok();
                    bar.append_child(&wrapper).ok();
                }
            }
        }

        Ok(MenuBar { elem: bar })
    }

#[derive(Clone)]
pub struct SimpleAction {
    name: String,
}

impl SimpleAction {
        pub fn connect_activate<F: FnMut(*mut c_void) + 'static>(
            &self,
            f: F,
        ) -> Result<u64, Error> {
            register_action(&self.name, Box::new(f));
            Ok(0)
        }
    }

    pub fn create_simple_action(name: &str) -> Result<SimpleAction, Error> {
        Ok(SimpleAction {
            name: name.to_owned(),
        })
    }

    // -----------------------------------------------------------------------
    // Dialog
    // -----------------------------------------------------------------------
    pub struct Dialog {
        elem: HtmlDialogElement,
        content_area: HtmlDivElement,
        response_cb: Rc<RefCell<Option<Box<dyn FnMut(i32)>>>>,
    }

    impl Clone for Dialog {
        fn clone(&self) -> Self {
            Dialog {
                elem: self.elem.clone(),
                content_area: self.content_area.clone(),
                response_cb: self.response_cb.clone(),
            }
        }
    }

    impl AsElement for Dialog {
        fn as_element(&self) -> &Element {
            self.elem.as_ref()
        }
    }

    impl Widget for Dialog {
        fn raw_handle(&self) -> *mut c_void {
            &self.elem as *const HtmlDialogElement as *mut c_void
        }
    }

    impl AsRef<*mut c_void> for Dialog {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.raw_handle() as *const *mut c_void) }
        }
    }

    impl Dialog {
        pub fn set_title(&self, title: &str) {
            let h2 = create_element("h2");
            h2.set_text_content(Some(title));
            self.elem.insert_before(&h2, self.elem.first_child().as_ref())
                .ok();
        }

        pub fn set_default_size(&self, w: i32, h: i32) {
            if w > 0 {
                set_css(self.elem.as_ref(), "width", &format!("{}px", w));
            }
            if h > 0 {
                set_css(self.elem.as_ref(), "height", &format!("{}px", h));
            }
        }

        pub fn add_button(&self, text: &str, response_id: i32) {
            let btn: HtmlButtonElement = create_element("button").dyn_into().unwrap();
            btn.set_text_content(Some(text));
            let cb = self.response_cb.clone();
            let closure = Closure::<dyn FnMut(MouseEvent)>::new(move |_: MouseEvent| {
                if let Some(ref mut f) = *cb.borrow_mut() {
                    f(response_id);
                }
            });
            btn.add_event_listener_with_callback("click", closure.as_ref().unchecked_ref())
                .ok();
            closure.forget();
            self.elem.append_child(btn.as_ref()).ok();
        }

        pub fn get_content_area(&self) -> *mut c_void {
            &self.content_area as *const HtmlDivElement as *mut c_void
        }

        pub fn append_content_area(&self, child: &impl AsRef<*mut c_void>) {
            self.content_area.append_child(as_element_from_ptr(*child.as_ref())).ok();
        }

        pub fn present(&self) {
            let _ = self.elem.show();
        }

        pub fn connect_response<F: FnMut(i32) + 'static>(&self, f: F) -> Result<u64, Error> {
            *self.response_cb.borrow_mut() = Some(Box::new(f));
            Ok(0)
        }
        pub fn close(&self) { self.elem.close(); }
    }

    pub fn create_dialog() -> Result<Dialog, Error> {
        let elem: HtmlDialogElement = create_element("dialog").dyn_into().map_err(|e| {
            Error::Backend(format!("create_dialog: {:?}", e))
        })?;
        let content: HtmlDivElement = create_element("div").dyn_into().map_err(|e| {
            Error::Backend(format!("create_dialog content: {:?}", e))
        })?;
        elem.append_child(content.as_ref()).ok();
        body().append_child(elem.as_ref()).ok();
        Ok(Dialog {
            elem,
            content_area: content,
            response_cb: Rc::new(RefCell::new(None)),
        })
    }

    // -----------------------------------------------------------------------
    // DropDown
    // -----------------------------------------------------------------------
    pub struct DropDown {
        elem: HtmlSelectElement,
        closures: Rc<RefCell<Vec<Box<dyn Any>>>>,
        next_id: Rc<RefCell<u64>>,
    }

    impl AsElement for DropDown {
        fn as_element(&self) -> &Element {
            self.elem.as_ref()
        }
    }

    impl Widget for DropDown {
        fn raw_handle(&self) -> *mut c_void {
            &self.elem as *const HtmlSelectElement as *mut c_void
        }
    }

    impl Clone for DropDown {
        fn clone(&self) -> Self {
            DropDown {
                elem: self.elem.clone(),
                closures: self.closures.clone(),
                next_id: self.next_id.clone(),
            }
        }
    }

    impl DropDown {
        pub fn set_active(&self, index: Option<u32>) {
            if let Some(i) = index {
                self.elem.set_selected_index(i as i32);
            }
        }

        pub fn get_active(&self) -> u32 {
            self.elem.selected_index() as u32
        }

        pub fn connect_changed(&self, f: impl FnMut() + 'static) -> Result<u64, Error> {
            let cb = Rc::new(RefCell::new(f));
            let cb2 = cb.clone();
            let closure = Closure::<dyn FnMut(Event)>::new(move |_: Event| {
                (cb2.borrow_mut())();
            });
            self.elem
                .add_event_listener_with_callback("change", closure.as_ref().unchecked_ref())
                .map_err(|e| Error::Backend(format!("connect_changed: {:?}", e)))?;
            let id = *self.next_id.borrow();
            *self.next_id.borrow_mut() += 1;
            store_closure(Box::new(closure));
            Ok(id)
        }
    }

    pub fn create_dropdown(items: &[&str]) -> Result<DropDown, Error> {
        let elem: HtmlSelectElement = create_element("select").dyn_into().map_err(|e| {
            Error::Backend(format!("create_dropdown: {:?}", e))
        })?;
        for item in items {
            let opt: HtmlOptionElement = create_element("option").dyn_into().unwrap();
            opt.set_text_content(Some(item));
            elem.append_child(opt.as_ref()).ok();
        }
        Ok(DropDown {
            elem,
            closures: Rc::new(RefCell::new(Vec::new())),
            next_id: Rc::new(RefCell::new(1)),
        })
    }

    // -----------------------------------------------------------------------
    // CheckButton
    // -----------------------------------------------------------------------
    pub struct CheckButton {
        elem: Element,
        input: HtmlInputElement,
        closures: Rc<RefCell<Vec<Box<dyn Any>>>>,
        next_id: Rc<RefCell<u64>>,
    }

    impl AsElement for CheckButton {
        fn as_element(&self) -> &Element {
            &self.elem
        }
    }

    impl Widget for CheckButton {
        fn raw_handle(&self) -> *mut c_void {
            &self.elem as *const Element as *mut c_void
        }
    }

    impl Clone for CheckButton {
        fn clone(&self) -> Self {
            CheckButton {
                elem: self.elem.clone(),
                input: self.input.clone(),
                closures: self.closures.clone(),
                next_id: self.next_id.clone(),
            }
        }
    }

    impl CheckButton {
        pub fn is_active(&self) -> bool {
            self.input.checked()
        }

        pub fn set_active(&self, active: bool) {
            self.input.set_checked(active);
        }

        pub fn connect_toggled(&self, f: impl FnMut() + 'static) -> Result<u64, Error> {
            let cb = Rc::new(RefCell::new(f));
            let cb2 = cb.clone();
            let closure = Closure::<dyn FnMut(Event)>::new(move |_: Event| {
                (cb2.borrow_mut())();
            });
            self.input
                .add_event_listener_with_callback("change", closure.as_ref().unchecked_ref())
                .map_err(|e| Error::Backend(format!("connect_toggled: {:?}", e)))?;
            let id = *self.next_id.borrow();
            *self.next_id.borrow_mut() += 1;
            store_closure(Box::new(closure));
            Ok(id)
        }
    }

    pub fn create_checkbutton(label: &str) -> Result<CheckButton, Error> {
        let wrapper = create_element("label");
        let input: HtmlInputElement = create_element("input").dyn_into().unwrap();
        input.set_type("checkbox");
        wrapper.append_child(input.as_ref()).ok();
        wrapper
            .append_child(&web_sys::Text::new_with_data(label).unwrap())
            .ok();
        Ok(CheckButton {
            elem: wrapper,
            input,
            closures: Rc::new(RefCell::new(Vec::new())),
            next_id: Rc::new(RefCell::new(1)),
        })
    }

    // -----------------------------------------------------------------------
    // RadioButton
    // -----------------------------------------------------------------------
    pub struct RadioButton {
        elem: Element,
        input: HtmlInputElement,
        closures: Rc<RefCell<Vec<Box<dyn Any>>>>,
        next_id: Rc<RefCell<u64>>,
        group_name: String,
    }

    impl AsElement for RadioButton {
        fn as_element(&self) -> &Element {
            &self.elem
        }
    }

    impl Widget for RadioButton {
        fn raw_handle(&self) -> *mut c_void {
            &self.elem as *const Element as *mut c_void
        }
    }

    impl Clone for RadioButton {
        fn clone(&self) -> Self {
            RadioButton {
                elem: self.elem.clone(),
                input: self.input.clone(),
                closures: self.closures.clone(),
                next_id: self.next_id.clone(),
                group_name: self.group_name.clone(),
            }
        }
    }

    impl RadioButton {
        pub fn is_active(&self) -> bool {
            self.input.checked()
        }

        pub fn set_active(&self, active: bool) {
            self.input.set_checked(active);
        }

        pub fn connect_toggled(&self, f: impl FnMut() + 'static) -> Result<u64, Error> {
            let cb = Rc::new(RefCell::new(f));
            let cb2 = cb.clone();
            let closure = Closure::<dyn FnMut(Event)>::new(move |_: Event| {
                (cb2.borrow_mut())();
            });
            self.input
                .add_event_listener_with_callback("change", closure.as_ref().unchecked_ref())
                .map_err(|e| Error::Backend(format!("connect_toggled: {:?}", e)))?;
            let id = *self.next_id.borrow();
            *self.next_id.borrow_mut() += 1;
            store_closure(Box::new(closure));
            Ok(id)
        }
    }

    pub fn create_radiobutton(group: Option<&RadioButton>, label: &str) -> Result<RadioButton, Error> {
        let group_name = group
            .map(|g| g.group_name.clone())
            .unwrap_or_else(|| format!("rb_{}", rand_id()));
        let wrapper = create_element("label");
        let input: HtmlInputElement = create_element("input").dyn_into().unwrap();
        input.set_type("radio");
        input.set_name(&group_name);
        wrapper.append_child(input.as_ref()).ok();
        wrapper
            .append_child(&web_sys::Text::new_with_data(label).unwrap())
            .ok();
        Ok(RadioButton {
            elem: wrapper,
            input,
            closures: Rc::new(RefCell::new(Vec::new())),
            next_id: Rc::new(RefCell::new(1)),
            group_name,
        })
    }

    fn rand_id() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        COUNTER.fetch_add(1, Ordering::Relaxed)
    }

    // -----------------------------------------------------------------------
    // TextView
    // -----------------------------------------------------------------------
    pub struct TextView {
        elem: HtmlTextAreaElement,
        closures: Rc<RefCell<Vec<Box<dyn Any>>>>,
        next_id: Rc<RefCell<u64>>,
    }

    impl AsElement for TextView {
        fn as_element(&self) -> &Element {
            self.elem.as_ref()
        }
    }

    impl Widget for TextView {
        fn raw_handle(&self) -> *mut c_void {
            &self.elem as *const HtmlTextAreaElement as *mut c_void
        }
    }

    impl Clone for TextView {
        fn clone(&self) -> Self {
            TextView {
                elem: self.elem.clone(),
                closures: self.closures.clone(),
                next_id: self.next_id.clone(),
            }
        }
    }

    impl TextView {
        pub fn set_text(&self, text: &str) {
            self.elem.set_value(text);
        }

        pub fn get_text(&self) -> Option<String> {
            Some(self.elem.value())
        }

        pub fn set_wrap_mode(&self, wrap_mode: i32) {
            match wrap_mode {
                0 => self.elem.set_wrap("off"),
                1 => self.elem.set_wrap("soft"),
                2 => self.elem.set_wrap("hard"),
                _ => self.elem.set_wrap("soft"),
            }
        }

        pub fn set_size_request(&self, w: i32, h: i32) {
            if w > 0 {
                set_css(self.elem.as_ref(), "width", &format!("{}px", w));
            }
            if h > 0 {
                set_css(self.elem.as_ref(), "height", &format!("{}px", h));
            }
        }

        pub fn set_hexpand(&self, expand: bool) {
            if expand {
                set_css(self.elem.as_ref(), "flex-grow", "1");
            } else {
                set_css(self.elem.as_ref(), "flex-grow", "0");
            }
        }

        pub fn set_vexpand(&self, expand: bool) {
            if expand {
                set_css(self.elem.as_ref(), "flex-shrink", "1");
                set_css(self.elem.as_ref(), "align-self", "stretch");
            } else {
                set_css(self.elem.as_ref(), "flex-shrink", "0");
            }
        }
    }

    pub fn create_textview() -> Result<TextView, Error> {
        let elem: HtmlTextAreaElement = create_element("textarea").dyn_into().map_err(|e| {
            Error::Backend(format!("create_textview: {:?}", e))
        })?;
        Ok(TextView {
            elem,
            closures: Rc::new(RefCell::new(Vec::new())),
            next_id: Rc::new(RefCell::new(1)),
        })
    }

    // -----------------------------------------------------------------------
    // DrawContext (Canvas 2D)
    // -----------------------------------------------------------------------
    pub struct WasmDrawContext {
        ctx: web_sys::CanvasRenderingContext2d,
    }

    impl WasmDrawContext {
        fn set_font(&self, font: &str, size: f64, slant: i32, weight: i32) {
            let weight_str = if weight != 0 { "bold" } else { "" };
            let slant_str = if slant != 0 { "italic " } else { "" };
            let size_str = format!("{}px", size);
            let f = format!("{}{} {} {}", slant_str, weight_str, size_str, font);
            let _ = self.ctx.set_font(&f);
        }

        fn rgba(r: f64, g: f64, b: f64, a: f64) -> String {
            format!("rgba({},{},{},{})", (r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8, a)
        }
    }

    impl crate::core::DrawContext for WasmDrawContext {
        fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, g: f64, b: f64, a: f64) {
            let _ = self.ctx.set_fill_style_str(&Self::rgba(r, g, b, a));
            self.ctx.fill_rect(x, y, w, h);
        }

        fn stroke_rect(&mut self, x: f64, y: f64, w: f64, h: f64, r: f64, g: f64, b: f64, a: f64, lw: f64) {
            let _ = self.ctx.set_stroke_style_str(&Self::rgba(r, g, b, a));
            self.ctx.set_line_width(lw);
            self.ctx.stroke_rect(x, y, w, h);
        }

        fn draw_text_styled(&mut self, x: f64, y: f64, text: &str, font: &str, size: f64,
                            r: f64, g: f64, b: f64, a: f64, slant: i32, weight: i32) {
            self.set_font(font, size, slant, weight);
            let _ = self.ctx.set_fill_style_str(&Self::rgba(r, g, b, a));
            let _ = self.ctx.fill_text(text, x, y);
        }

        fn text_extents_styled(&self, text: &str, font: &str, size: f64, slant: i32, weight: i32) -> (f64, f64, f64, f64) {
            let weight_str = if weight != 0 { "bold" } else { "" };
            let slant_str = if slant != 0 { "italic " } else { "" };
            let size_str = format!("{}px", size);
            let f = format!("{}{} {} {}", slant_str, weight_str, size_str, font);
            self.ctx.save();
            let _ = self.ctx.set_font(&f);
            let metrics = self.ctx.measure_text(text).expect("measure_text failed");
            self.ctx.restore();
            let w = metrics.width();
            let xb = -metrics.actual_bounding_box_left();
            let yb = -metrics.actual_bounding_box_ascent();
            let h = metrics.actual_bounding_box_ascent() + metrics.actual_bounding_box_descent();
            (xb, yb, w, h)
        }

        fn clear(&mut self, r: f64, g: f64, b: f64, a: f64) {
            let _ = self.ctx.set_fill_style_str(&Self::rgba(r, g, b, a));
            let canvas = self.ctx.canvas().unwrap();
            let w = canvas.width() as f64;
            let h = canvas.height() as f64;
            self.ctx.fill_rect(0.0, 0.0, w, h);
        }

        fn save(&mut self) { self.ctx.save(); }
        fn restore(&mut self) { self.ctx.restore(); }

        fn clip(&mut self, x: f64, y: f64, w: f64, h: f64) {
            self.ctx.begin_path();
            self.ctx.rect(x, y, w, h);
            self.ctx.clip();
        }
    }

    // -----------------------------------------------------------------------
    // Canvas
    // -----------------------------------------------------------------------
    pub struct Canvas {
        elem: HtmlCanvasElement,
        draw_cb: Rc<RefCell<Option<Box<dyn FnMut(&mut dyn crate::core::DrawContext, i32, i32)>>>>,
        click_cb: Rc<RefCell<Option<Box<dyn FnMut(f64, f64)>>>>,
        key_cb: Rc<RefCell<Option<Box<dyn FnMut(u32) -> bool>>>>,
        closures: Rc<RefCell<Vec<Box<dyn Any>>>>,
    }

    impl AsElement for Canvas {
        fn as_element(&self) -> &Element {
            self.elem.as_ref()
        }
    }

    impl Widget for Canvas {
        fn raw_handle(&self) -> *mut c_void {
            &self.elem as *const HtmlCanvasElement as *mut c_void
        }
    }

    impl AsRef<*mut c_void> for Canvas {
        fn as_ref(&self) -> &*mut c_void {
            unsafe { &*(&self.raw_handle() as *const *mut c_void) }
        }
    }

    impl Clone for Canvas {
        fn clone(&self) -> Self {
            Canvas {
                elem: self.elem.clone(),
                draw_cb: self.draw_cb.clone(),
                click_cb: self.click_cb.clone(),
                key_cb: self.key_cb.clone(),
                closures: self.closures.clone(),
            }
        }
    }

    impl Canvas {
        pub fn set_draw_callback(&self, cb: Box<dyn FnMut(&mut dyn crate::core::DrawContext, i32, i32)>) {
            *self.draw_cb.borrow_mut() = Some(cb);
            self.queue_redraw();
        }

        pub fn queue_redraw(&self) {
            if let Some(ref mut draw_fn) = *self.draw_cb.borrow_mut() {
                let ctx = self.elem.get_context("2d")
                    .ok().flatten()
                    .and_then(|o| o.dyn_into::<web_sys::CanvasRenderingContext2d>().ok());
                if let Some(ctx) = ctx {
                    let w = self.elem.width() as i32;
                    let h = self.elem.height() as i32;
                    let mut dc = WasmDrawContext { ctx };
                    draw_fn(&mut dc, w, h);
                }
            }
        }

        pub fn set_size_request(&self, w: i32, h: i32) {
            if w > 0 { self.elem.set_width(w as u32); }
            if h > 0 { self.elem.set_height(h as u32); }
            set_css(self.elem.as_ref(), "width", &format!("{}px", w));
            set_css(self.elem.as_ref(), "height", &format!("{}px", h));
        }

        pub fn set_content_size(&self, w: i32, h: i32) {
            self.elem.set_width(w as u32);
            self.elem.set_height(h as u32);
        }

        pub fn on_click(&self, cb: Box<dyn FnMut(f64, f64)>) {
            *self.click_cb.borrow_mut() = Some(cb);
            let cb2 = self.click_cb.clone();
            let closure = Closure::<dyn FnMut(MouseEvent)>::new(move |evt: MouseEvent| {
                if let Some(ref mut f) = *cb2.borrow_mut() {
                    f(evt.offset_x() as f64, evt.offset_y() as f64);
                }
            });
            self.elem
                .add_event_listener_with_callback("mousedown", closure.as_ref().unchecked_ref())
                .ok();
            store_closure(Box::new(closure));
        }

        pub fn on_key(&self, cb: Box<dyn FnMut(u32) -> bool>) {
            *self.key_cb.borrow_mut() = Some(cb);
            let cb2 = self.key_cb.clone();
            let closure = Closure::<dyn FnMut(KeyboardEvent)>::new(move |evt: KeyboardEvent| {
                if let Some(ref mut f) = *cb2.borrow_mut() {
                    if f(evt.key_code()) {
                        evt.prevent_default();
                    }
                }
            });
            self.elem
                .add_event_listener_with_callback("keydown", closure.as_ref().unchecked_ref())
                .ok();
            store_closure(Box::new(closure));
            // Make canvas focusable
            self.elem.set_tab_index(0);
        }
        pub fn on_key_raw(&self, cb: Box<dyn FnMut(u32, u32) -> bool>) {
            let mut cb = cb;
            self.on_key(Box::new(move |k: u32| -> bool { cb(k, 0) }));
        }
        pub fn grab_focus(&self) {
            let _ = self.elem.focus();
        }
        pub fn set_can_focus(&self, _can: bool) {}
        pub fn force_draw(&self, _window_ptr: *mut c_void, _fallback_w: i32, _fallback_h: i32) {}
    }

    pub fn create_canvas() -> Result<Canvas, Error> {
        let elem: HtmlCanvasElement = create_element("canvas").dyn_into().map_err(|e| {
            Error::Backend(format!("create_canvas: {:?}", e))
        })?;
        let ctx = elem.get_context("2d")
            .ok().flatten()
            .and_then(|o| o.dyn_into::<web_sys::CanvasRenderingContext2d>().ok())
            .ok_or_else(|| Error::Backend("getContext('2d') failed".into()))?;
        // Use default font that will be overridden per draw call
        let _ = ctx.set_font("12px monospace");
        Ok(Canvas {
            elem,
            draw_cb: Rc::new(RefCell::new(None)),
            click_cb: Rc::new(RefCell::new(None)),
            key_cb: Rc::new(RefCell::new(None)),
            closures: Rc::new(RefCell::new(Vec::new())),
        })
    }

    // -----------------------------------------------------------------------
    // Overlay (stacking container using position:relative + absolute overlays)
    // -----------------------------------------------------------------------
    pub struct Overlay {
        elem: HtmlDivElement,
        children: Rc<RefCell<Vec<Element>>>,
    }

    impl AsElement for Overlay {
        fn as_element(&self) -> &Element {
            self.elem.as_ref()
        }
    }

    impl Widget for Overlay {
        fn raw_handle(&self) -> *mut c_void {
            &self.elem as *const HtmlDivElement as *mut c_void
        }
    }

    impl Clone for Overlay {
        fn clone(&self) -> Self {
            Overlay {
                elem: self.elem.clone(),
                children: self.children.clone(),
            }
        }
    }

    impl Overlay {
        pub fn set_child(&self, child: &impl AsElement) {
            while let Some(c) = self.elem.first_child() {
                self.elem.remove_child(&c).ok();
            }
            self.children.borrow_mut().clear();
            self.children.borrow_mut().push(child.as_element().clone());
            self.elem.append_child(child.as_element()).ok();
        }

        pub fn add_overlay(&self, child: &impl AsElement) {
            self.children.borrow_mut().push(child.as_element().clone());
            set_css(child.as_element(), "position", "absolute");
            set_css(child.as_element(), "z-index", "10");
            self.elem.append_child(child.as_element()).ok();
        }

        pub fn set_overlay_pass_through(&self, child: &impl AsElement, pass: bool) {
            if pass {
                set_css(child.as_element(), "pointer-events", "none");
            } else {
                set_css(child.as_element(), "pointer-events", "auto");
            }
        }

        pub fn remove(&self, child: &impl AsElement) {
            self.elem.remove_child(child.as_element()).ok();
            self.children.borrow_mut().retain(|c| c != child.as_element());
        }

        pub fn show_all(&self) {
            for c in self.children.borrow().iter() {
                if let Some(html) = c.dyn_ref::<HtmlElement>() {
                    html.style().set_property("display", "").ok();
                }
            }
        }

        pub fn set_size_request(&self, w: i32, h: i32) {
            if w > 0 { set_css(self.elem.as_ref(), "width", &format!("{}px", w)); }
            if h > 0 { set_css(self.elem.as_ref(), "height", &format!("{}px", h)); }
        }

        pub fn set_vexpand(&self, expand: bool) {
            if expand {
                set_css(self.elem.as_ref(), "flex-grow", "1");
            } else {
                set_css(self.elem.as_ref(), "flex-grow", "0");
            }
        }

        pub fn set_hexpand(&self, expand: bool) {
            if expand {
                set_css(self.elem.as_ref(), "align-self", "stretch");
            }
        }
    }

    pub fn create_overlay() -> Result<Overlay, Error> {
        let div: HtmlDivElement = create_element("div").dyn_into().map_err(|e| {
            Error::Backend(format!("create_overlay: {:?}", e))
        })?;
        set_css(div.as_ref(), "position", "relative");
        div.style().set_property("overflow", "hidden").ok();
        Ok(Overlay {
            elem: div,
            children: Rc::new(RefCell::new(Vec::new())),
        })
    }

    // -----------------------------------------------------------------------
    // ScrolledWindow
    // -----------------------------------------------------------------------
    pub struct ScrolledWindow {
        elem: HtmlDivElement,
    }

    impl AsElement for ScrolledWindow {
        fn as_element(&self) -> &Element {
            self.elem.as_ref()
        }
    }

    impl Widget for ScrolledWindow {
        fn raw_handle(&self) -> *mut c_void {
            &self.elem as *const HtmlDivElement as *mut c_void
        }
    }

    impl Clone for ScrolledWindow {
        fn clone(&self) -> Self {
            ScrolledWindow { elem: self.elem.clone() }
        }
    }

    impl ScrolledWindow {
        pub fn set_policy(&self, hscroll: i32, vscroll: i32) {
            let h = match hscroll { 0 => "hidden", 1 => "scroll", _ => "auto" };
            let v = match vscroll { 0 => "hidden", 1 => "scroll", _ => "auto" };
            self.elem.style().set_property("overflow-x", h).ok();
            self.elem.style().set_property("overflow-y", v).ok();
        }

        pub fn set_child(&self, child: &impl AsElement) {
            while let Some(c) = self.elem.first_child() {
                self.elem.remove_child(&c).ok();
            }
            self.elem.append_child(child.as_element()).ok();
        }

        pub fn set_vexpand(&self, expand: bool) {
            if expand {
                set_css(self.elem.as_ref(), "flex-grow", "1");
                set_css(self.elem.as_ref(), "align-self", "stretch");
            }
        }

        pub fn set_hexpand(&self, expand: bool) {
            if expand {
                set_css(self.elem.as_ref(), "align-self", "stretch");
            }
        }
    }

    pub fn create_scrolled_window() -> Result<ScrolledWindow, Error> {
        let div: HtmlDivElement = create_element("div").dyn_into().map_err(|e| {
            Error::Backend(format!("create_scrolled_window: {:?}", e))
        })?;
        div.style().set_property("overflow", "auto").ok();
        div.style().set_property("position", "relative").ok();
        Ok(ScrolledWindow { elem: div })
    }
}

#[cfg(target_arch = "wasm32")]
pub use wasm_adapter::*;
