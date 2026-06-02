#[cfg(target_os = "android")]
mod android_adapter {
    use crate::core::{Error, Widget};
    use jni::objects::JString;
    use std::os::raw::c_void;

    #[repr(transparent)]
    pub struct Window(pub *mut c_void);

    impl Widget for Window {
        fn raw_handle(&self) -> *mut c_void {
            self.0
        }
    }

    impl AsRef<*mut c_void> for Window {
        fn as_ref(&self) -> &*mut c_void {
            &self.0
        }
    }

    impl Window {
        pub fn set_title(&self, _title: &str) {}

        pub fn set_child(&self, child: &impl AsRef<*mut c_void>) {
            let child_ptr = *child.as_ref();
            if child_ptr.is_null() {
                return;
            }
            let _ = crate::backends::android::with_env_and_activity(|env, _activity| {
                let root = crate::backends::android::root_layout()?;
                let child_obj = unsafe {
                    // SAFETY: child_ptr was obtained from a JNI-created object. We reconstruct
                    // a JObject from the raw pointer only for the duration of this JNI call.
                    jni::objects::JObject::from_raw(child_ptr as jni::sys::jobject)
                };
                env.call_method(
                    root.as_obj(),
                    "addView",
                    "(Landroid/view/View;)V",
                    &[(&child_obj).into()],
                )?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            });
        }

        pub fn present(&self) {}
    }

    #[repr(transparent)]
    pub struct Button(pub *mut c_void);

    impl AsRef<*mut c_void> for Button {
        fn as_ref(&self) -> &*mut c_void {
            &self.0
        }
    }

    impl Button {
        pub fn on_click(&self, f: impl FnMut() + Send + 'static) -> Result<u64, Error> {
            let id = crate::backends::android::register_callback(Box::new(f));
            let _ = crate::backends::android::with_env_and_activity(|env, _activity| {
                if let Some(listener) = crate::backends::android::try_create_onclick_listener(env, id)
                    .map_err(|e| format!("{e}"))
                    .unwrap_or(None)
                {
                    let btn = unsafe {
                        // SAFETY: self.0 is a raw jobject from create_button(). We reconstruct
                        // it only for this JNI call while the JVM is attached.
                        jni::objects::JObject::from_raw(self.0 as jni::sys::jobject)
                    };
                    env.call_method(
                        &btn,
                        "setOnClickListener",
                        "(Landroid/view/View$OnClickListener;)V",
                        &[(&listener).into()],
                    )?;
                }
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            });
            Ok(id)
        }

        pub fn emit_clicked(&self) -> Result<u64, Error> {
            let _ = crate::backends::android::with_env_and_activity(|env, _activity| {
                    let btn = unsafe {
                        // SAFETY: self.0 is a raw jobject from create_button(). We only use it
                        // synchronously within this JNI call while the JVM is attached.
                        jni::objects::JObject::from_raw(self.0 as jni::sys::jobject)
                    };
                    env.call_method(&btn, "performClick", "()Z", &[])?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            });
            Ok(0)
        }
    }

    impl Clone for Button {
        fn clone(&self) -> Self {
            Button(self.0)
        }
    }

    #[repr(transparent)]
    pub struct Label(pub *mut c_void);

    impl AsRef<*mut c_void> for Label {
        fn as_ref(&self) -> &*mut c_void {
            &self.0
        }
    }

    impl Label {
        pub fn set_text(&self, text: &str) {
            let _ = crate::backends::android::with_env_and_activity(|env, _activity| {
                let tv = unsafe {
                    // SAFETY: self.0 is a raw jobject from create_label(). JVM is attached.
                    jni::objects::JObject::from_raw(self.0 as jni::sys::jobject)
                };
                let j_text = env.new_string(text)?;
                env.call_method(
                    &tv,
                    "setText",
                    "(Ljava/lang/CharSequence;)V",
                    &[(&j_text).into()],
                )?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            });
        }

        pub fn get_text(&self) -> Option<String> {
            let result = crate::backends::android::with_env_and_activity(|env, _activity| {
                let tv = unsafe { jni::objects::JObject::from_raw(self.0 as jni::sys::jobject) };
                let j_value = env.call_method(&tv, "getText", "()Ljava/lang/CharSequence;", &[])?;
                let j_obj_ref = j_value.l()?;
                let j_obj = unsafe { jni::objects::JObject::from_raw(j_obj_ref.as_raw()) };
                let j_str = JString::from(j_obj);
                let text: String = env.get_string(&j_str)?.into();
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(text)
            });
            result.ok()
        }

        pub fn set_visible(&self, visible: bool) {
            let _ = crate::backends::android::with_env_and_activity(|env, _activity| {
                let tv = unsafe { jni::objects::JObject::from_raw(self.0 as jni::sys::jobject) };
                let visibility = if visible { 0i32 } else { 8i32 };
                env.call_method(&tv, "setVisibility", "(I)V", &[visibility.into()])?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            });
        }

        pub fn set_markup(&self, markup: &str) {
            self.set_text(markup);
        }
    }

    impl Clone for Label {
        fn clone(&self) -> Self {
            Label(self.0)
        }
    }

    #[repr(transparent)]
    pub struct BoxWidget(pub *mut c_void);

    impl Widget for BoxWidget {
        fn raw_handle(&self) -> *mut c_void {
            self.0
        }
    }

    impl AsRef<*mut c_void> for BoxWidget {
        fn as_ref(&self) -> &*mut c_void {
            &self.0
        }
    }

    impl BoxWidget {
        pub fn append(&self, child: &impl AsRef<*mut c_void>) {
            let child_ptr = *child.as_ref();
            if child_ptr.is_null() {
                return;
            }
            let _ = crate::backends::android::with_env_and_activity(|env, _activity| {
                let layout = unsafe { jni::objects::JObject::from_raw(self.0 as jni::sys::jobject) };
                let child_obj = unsafe { jni::objects::JObject::from_raw(child_ptr as jni::sys::jobject) };
                env.call_method(
                    &layout,
                    "addView",
                    "(Landroid/view/View;)V",
                    &[(&child_obj).into()],
                )?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            });
        }
    }

    #[repr(transparent)]
    pub struct Entry(pub *mut c_void);

    impl AsRef<*mut c_void> for Entry {
        fn as_ref(&self) -> &*mut c_void {
            &self.0
        }
    }

    impl Entry {
        pub fn set_text(&self, text: &str) {
            let _ = crate::backends::android::with_env_and_activity(|env, _activity| {
                let edit = unsafe { jni::objects::JObject::from_raw(self.0 as jni::sys::jobject) };
                let j_text = env.new_string(text)?;
                env.call_method(
                    &edit,
                    "setText",
                    "(Ljava/lang/CharSequence;)V",
                    &[(&j_text).into()],
                )?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            });
        }

        pub fn get_text(&self) -> Option<String> {
            let result = crate::backends::android::with_env_and_activity(|env, _activity| {
                let edit = unsafe { jni::objects::JObject::from_raw(self.0 as jni::sys::jobject) };
                let j_value = env.call_method(&edit, "getText", "()Ljava/lang/CharSequence;", &[])?;
                let j_obj_ref = j_value.l()?;
                let j_obj = unsafe { jni::objects::JObject::from_raw(j_obj_ref.as_raw()) };
                let j_str = JString::from(j_obj);
                let text: String = env.get_string(&j_str)?.into();
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(text)
            });
            result.ok()
        }

        pub fn set_width_chars(&self, _n: i32) {}

        pub fn set_size_request(&self, _w: i32, _h: i32) {}

        pub fn grab_focus(&self) {
            let _ = crate::backends::android::with_env_and_activity(|env, _activity| {
                let edit = unsafe { jni::objects::JObject::from_raw(self.0 as jni::sys::jobject) };
                env.call_method(&edit, "requestFocus", "()Z", &[])?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            });
        }

        pub fn connect_changed(&self, _f: impl FnMut() + 'static) -> Result<u64, Error> {
            Ok(0)
        }
    }

    impl Clone for Entry {
        fn clone(&self) -> Self {
            Entry(self.0)
        }
    }

    pub fn create_window() -> Result<Window, Error> {
        let ptr = crate::backends::android::create_window()
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Window(ptr as *mut c_void))
    }

    pub fn create_button(label: &str) -> Result<Button, Error> {
        let ptr = crate::backends::android::create_button(label)
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Button(ptr as *mut c_void))
    }

    pub fn create_label(text: &str) -> Result<Label, Error> {
        let ptr = crate::backends::android::create_label(text)
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Label(ptr as *mut c_void))
    }

    pub fn create_box(orientation: i32, spacing: i32) -> Result<BoxWidget, Error> {
        let ptr = crate::backends::android::create_box(orientation, spacing)
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(BoxWidget(ptr as *mut c_void))
    }

    pub fn create_entry() -> Result<Entry, Error> {
        let ptr = crate::backends::android::create_entry()
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Entry(ptr as *mut c_void))
    }

    // ---- Grid ----

    #[repr(transparent)]
    pub struct Grid(pub *mut c_void);

    impl Widget for Grid {
        fn raw_handle(&self) -> *mut c_void { self.0 }
    }

    impl AsRef<*mut c_void> for Grid {
        fn as_ref(&self) -> &*mut c_void { &self.0 }
    }

    impl Grid {
        pub fn attach(&self, child: &impl AsRef<*mut c_void>, _left: i32, _top: i32, _width: i32, _height: i32) {
            let child_ptr = *child.as_ref();
            if child_ptr.is_null() { return; }
            let _ = crate::backends::android::with_env_and_activity(|env, _activity| {
                let grid = unsafe { jni::objects::JObject::from_raw(self.0 as jni::sys::jobject) };
                let child_obj = unsafe { jni::objects::JObject::from_raw(child_ptr as jni::sys::jobject) };
                env.call_method(
                    &grid, "addView",
                    "(Landroid/view/View;)V",
                    &[(&child_obj).into()],
                )?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            });
        }
    }

    pub fn create_grid() -> Result<Grid, Error> {
        let ptr = crate::backends::android::create_grid()
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Grid(ptr as *mut c_void))
    }

    // ---- DropDown ----

    #[repr(transparent)]
    pub struct DropDown(pub *mut c_void);

    impl Widget for DropDown {
        fn raw_handle(&self) -> *mut c_void { self.0 }
    }

    impl AsRef<*mut c_void> for DropDown {
        fn as_ref(&self) -> &*mut c_void { &self.0 }
    }

    impl DropDown {
        pub fn set_active(&self, index: u32) {
            let _ = crate::backends::android::with_env_and_activity(|env, _activity| {
                let spinner = unsafe { jni::objects::JObject::from_raw(self.0 as jni::sys::jobject) };
                env.call_method(
                    &spinner, "setSelection", "(I)V",
                    &[(index as i32).into()],
                )?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            });
        }

        pub fn get_active(&self) -> i32 {
            let r = crate::backends::android::with_env_and_activity(|env, _activity| {
                let spinner = unsafe { jni::objects::JObject::from_raw(self.0 as jni::sys::jobject) };
                let pos = env.call_method(
                    &spinner, "getSelectedItemPosition", "()I", &[],
                )?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(pos.i()?)
            });
            r.unwrap_or(-1)
        }

        pub fn connect_changed(&self, _f: impl FnMut() + 'static) -> Result<u64, Error> {
            Ok(0) // TODO: OnItemSelectedListener via JNI trampoline
        }
    }

    impl Clone for DropDown {
        fn clone(&self) -> Self { DropDown(self.0) }
    }

    pub fn create_dropdown(items: &[&str]) -> Result<DropDown, Error> {
        let ptr = crate::backends::android::create_dropdown(items)
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(DropDown(ptr as *mut c_void))
    }

    // ---- CheckButton ----

    #[repr(transparent)]
    pub struct CheckButton(pub *mut c_void);

    impl Widget for CheckButton {
        fn raw_handle(&self) -> *mut c_void { self.0 }
    }

    impl AsRef<*mut c_void> for CheckButton {
        fn as_ref(&self) -> &*mut c_void { &self.0 }
    }

    impl CheckButton {
        pub fn is_active(&self) -> bool {
            let r = crate::backends::android::with_env_and_activity(|env, _activity| {
                let cb = unsafe { jni::objects::JObject::from_raw(self.0 as jni::sys::jobject) };
                let checked = env.call_method(&cb, "isChecked", "()Z", &[])?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(checked.z()?)
            });
            r.unwrap_or(false)
        }

        pub fn set_active(&self, active: bool) {
            let _ = crate::backends::android::with_env_and_activity(|env, _activity| {
                let cb = unsafe { jni::objects::JObject::from_raw(self.0 as jni::sys::jobject) };
                env.call_method(&cb, "setChecked", "(Z)V", &[active.into()])?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            });
        }

        pub fn connect_toggled(&self, _f: impl FnMut() + 'static) -> Result<u64, Error> {
            Ok(0) // TODO: OnCheckedChangeListener via JNI trampoline
        }
    }

    impl Clone for CheckButton {
        fn clone(&self) -> Self { CheckButton(self.0) }
    }

    pub fn create_checkbutton(label: &str) -> Result<CheckButton, Error> {
        let ptr = crate::backends::android::create_checkbutton(label)
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(CheckButton(ptr as *mut c_void))
    }

    // ---- RadioButton ----

    #[repr(transparent)]
    pub struct RadioButton(pub *mut c_void);

    impl Widget for RadioButton {
        fn raw_handle(&self) -> *mut c_void { self.0 }
    }

    impl AsRef<*mut c_void> for RadioButton {
        fn as_ref(&self) -> &*mut c_void { &self.0 }
    }

    impl RadioButton {
        pub fn is_active(&self) -> bool {
            let r = crate::backends::android::with_env_and_activity(|env, _activity| {
                let rb = unsafe { jni::objects::JObject::from_raw(self.0 as jni::sys::jobject) };
                let checked = env.call_method(&rb, "isChecked", "()Z", &[])?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(checked.z()?)
            });
            r.unwrap_or(false)
        }

        pub fn set_active(&self, active: bool) {
            let _ = crate::backends::android::with_env_and_activity(|env, _activity| {
                let rb = unsafe { jni::objects::JObject::from_raw(self.0 as jni::sys::jobject) };
                env.call_method(&rb, "setChecked", "(Z)V", &[active.into()])?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            });
        }

        pub fn connect_toggled(&self, _f: impl FnMut() + 'static) -> Result<u64, Error> {
            Ok(0) // TODO: OnCheckedChangeListener via JNI trampoline
        }
    }

    impl Clone for RadioButton {
        fn clone(&self) -> Self { RadioButton(self.0) }
    }

    pub fn create_radiobutton(group: Option<&RadioButton>, label: &str) -> Result<RadioButton, Error> {
        let group_ptr = group.map(|g| g.0).unwrap_or(std::ptr::null_mut());
        let ptr = crate::backends::android::create_radiobutton(group_ptr, label)
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(RadioButton(ptr as *mut c_void))
    }

    // ---- Dialog ----

    #[repr(transparent)]
    pub struct Dialog(pub *mut c_void);

    impl Widget for Dialog {
        fn raw_handle(&self) -> *mut c_void { self.0 }
    }

    impl AsRef<*mut c_void> for Dialog {
        fn as_ref(&self) -> &*mut c_void { &self.0 }
    }

    impl Dialog {
        pub fn set_title(&self, title: &str) {
            let _ = crate::backends::android::with_env_and_activity(|env, _activity| {
                let builder = unsafe { jni::objects::JObject::from_raw(self.0 as jni::sys::jobject) };
                let j_title = env.new_string(title)?;
                env.call_method(
                    &builder, "setTitle",
                    "(Ljava/lang/CharSequence;)Landroid/app/AlertDialog$Builder;",
                    &[(&j_title).into()],
                )?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            });
        }

        pub fn set_default_size(&self, _w: i32, _h: i32) {}

        pub fn add_button(&self, text: &str, response_id: i32) {
            let _ = crate::backends::android::with_env_and_activity(|env, _activity| {
                let builder = unsafe { jni::objects::JObject::from_raw(self.0 as jni::sys::jobject) };
                let j_text = env.new_string(text)?;
                let which = if response_id == -1 { -1i32 } else { -2i32 }; // -1=OK, -2=CANCEL
                env.call_method(
                    &builder, "setPositiveButton",
                    "(Ljava/lang/CharSequence;Landroid/content/DialogInterface$OnClickListener;)Landroid/app/AlertDialog$Builder;",
                    &[(&j_text).into(), (&jni::objects::JObject::null()).into()],
                )?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            });
        }

        pub fn present(&self) {
            let _ = crate::backends::android::with_env_and_activity(|env, _activity| {
                let builder = unsafe { jni::objects::JObject::from_raw(self.0 as jni::sys::jobject) };
                let dialog = env.call_method(
                    &builder, "create",
                    "()Landroid/app/AlertDialog;",
                    &[],
                )?;
                env.call_method(
                    dialog.l()?, "show",
                    "()V",
                    &[],
                )?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            });
        }

        pub fn connect_response(&self, _f: impl FnMut(i32) + 'static) -> Result<u64, Error> {
            Ok(0) // TODO: DialogInterface.OnClickListener via JNI trampoline
        }
    }

    pub fn create_dialog() -> Result<Dialog, Error> {
        let ptr = crate::backends::android::create_dialog()
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(Dialog(ptr as *mut c_void))
    }

    // ---- TextView ----

    #[repr(transparent)]
    pub struct TextView(pub *mut c_void);

    impl Widget for TextView {
        fn raw_handle(&self) -> *mut c_void { self.0 }
    }

    impl AsRef<*mut c_void> for TextView {
        fn as_ref(&self) -> &*mut c_void { &self.0 }
    }

    impl TextView {
        pub fn set_text(&self, text: &str) {
            let _ = crate::backends::android::with_env_and_activity(|env, _activity| {
                let tv = unsafe { jni::objects::JObject::from_raw(self.0 as jni::sys::jobject) };
                let j_text = env.new_string(text)?;
                env.call_method(
                    &tv, "setText",
                    "(Ljava/lang/CharSequence;)V",
                    &[(&j_text).into()],
                )?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            });
        }

        pub fn get_text(&self) -> Option<String> {
            let r = crate::backends::android::with_env_and_activity(|env, _activity| {
                let tv = unsafe { jni::objects::JObject::from_raw(self.0 as jni::sys::jobject) };
                let j_value = env.call_method(&tv, "getText", "()Ljava/lang/CharSequence;", &[])?;
                let j_obj_ref = j_value.l()?;
                let j_obj = unsafe { jni::objects::JObject::from_raw(j_obj_ref.as_raw()) };
                let j_str = jni::objects::JString::from(j_obj);
                let text: String = env.get_string(&j_str)?.into();
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(text)
            });
            r.ok()
        }

        pub fn set_wrap_mode(&self, _wrap_mode: i32) {}

        pub fn set_size_request(&self, _w: i32, _h: i32) {}

        pub fn set_hexpand(&self, _expand: bool) {}

        pub fn set_vexpand(&self, _expand: bool) {}
    }

    impl Clone for TextView {
        fn clone(&self) -> Self { TextView(self.0) }
    }

    pub fn create_textview() -> Result<TextView, Error> {
        let ptr = crate::backends::android::create_textview()
            .map_err(|e| Error::Backend(format!("{}", e)))?;
        Ok(TextView(ptr as *mut c_void))
    }
}

#[cfg(target_os = "android")]
pub use android_adapter::*;

#[cfg(test)]
#[cfg(target_os = "android")]
mod tests {
    use crate::core::Widget;

    #[test]
    fn test_null_window_handle() {
        let w = super::Window(std::ptr::null_mut());
        assert!(w.raw_handle().is_null());
    }

    #[test]
    fn test_null_button_handle() {
        let b = super::Button(std::ptr::null_mut());
        assert!(b.as_ref().is_null());
    }

    #[test]
    fn test_null_label_handle() {
        let l = super::Label(std::ptr::null_mut());
        assert!(l.as_ref().is_null());
    }

    #[test]
    fn test_null_box_handle() {
        let b = super::BoxWidget(std::ptr::null_mut());
        assert!(b.as_ref().is_null());
    }

    #[test]
    fn test_null_entry_handle() {
        let e = super::Entry(std::ptr::null_mut());
        assert!(e.as_ref().is_null());
    }

    #[test]
    fn test_clone_button() {
        let b1 = super::Button(0x1234 as *mut _);
        let b2 = b1.clone();
        assert_eq!(b1.as_ref(), b2.as_ref());
    }

    #[test]
    fn test_clone_label() {
        let l1 = super::Label(0x5678 as *mut _);
        let l2 = l1.clone();
        assert_eq!(l1.as_ref(), l2.as_ref());
    }

    #[test]
    fn test_clone_entry() {
        let e1 = super::Entry(0x9abc as *mut _);
        let e2 = e1.clone();
        assert_eq!(e1.as_ref(), e2.as_ref());
    }
}
