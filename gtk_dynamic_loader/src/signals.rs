use std::ffi::CString;
use std::os::raw::c_void;

// Trampoline and destroy notify for clicked handler
// We'll define a small C-ABI trampoline that converts user_data pointer into a Box<dyn FnMut()>

// trampoline for signals with (instance, user_data)
#[no_mangle]
pub extern "C" fn gtk_compat_trampoline_2(_instance: *mut c_void, user_data: *mut c_void) {
    unsafe {
        if user_data.is_null() { return; }
        let inner_ptr = user_data as *mut Box<dyn FnMut()>;
        if inner_ptr.is_null() { return; }
        let closure_ref: &mut dyn FnMut() = &mut **inner_ptr;
        closure_ref();
    }
}

// trampoline for signals with (instance, param, user_data)
#[no_mangle]
pub extern "C" fn gtk_compat_trampoline_3(_instance: *mut c_void, _param: *mut c_void, user_data: *mut c_void) {
    unsafe {
        if user_data.is_null() { return; }
        let inner_ptr = user_data as *mut Box<dyn FnMut()>;
        if inner_ptr.is_null() { return; }
        let closure_ref: &mut dyn FnMut() = &mut **inner_ptr;
        closure_ref();
    }
}

#[no_mangle]
pub extern "C" fn gtk_compat_destroy_notify(data: *mut c_void, _closure: *mut c_void) {
    unsafe {
        if data.is_null() { return; }
        // data is a *mut Box<dyn FnMut()> produced by Box::into_raw
        let inner_ptr = data as *mut Box<dyn FnMut()>;
        // reconstruct the outer Box<Box<dyn FnMut()>> and drop it (frees the closure)
        let _boxed: Box<Box<dyn FnMut()>> = Box::from_raw(inner_ptr);
        // dropped here
    }
}

// trampoline for signals that pass a param pointer to the closure (no return)
#[no_mangle]
pub extern "C" fn gtk_compat_trampoline_param(_instance: *mut c_void, param: *mut c_void, user_data: *mut c_void) {
    unsafe {
        if user_data.is_null() { return; }
        let inner_ptr = user_data as *mut Box<dyn FnMut(*mut c_void)>;
        if inner_ptr.is_null() { return; }
        let closure_ref: &mut dyn FnMut(*mut c_void) = &mut **inner_ptr;
        closure_ref(param);
    }
}

#[no_mangle]
pub extern "C" fn gtk_compat_destroy_notify_param(data: *mut c_void, _closure: *mut c_void) {
    unsafe {
        if data.is_null() { return; }
        let inner_ptr = data as *mut Box<dyn FnMut(*mut c_void)>;
        let _boxed: Box<Box<dyn FnMut(*mut c_void)>> = Box::from_raw(inner_ptr);
    }
}

// trampoline for signals that expect a gboolean/int return value (e.g. key-press-event)
#[no_mangle]
pub extern "C" fn gtk_compat_trampoline_bool(_instance: *mut c_void, param: *mut c_void, user_data: *mut c_void) -> i32 {
    unsafe {
        if user_data.is_null() { return 0; }
        let inner_ptr = user_data as *mut Box<dyn FnMut(*mut c_void) -> i32>;
        if inner_ptr.is_null() { return 0; }
        let closure_ref: &mut dyn FnMut(*mut c_void) -> i32 = &mut **inner_ptr;
        closure_ref(param)
    }
}

#[no_mangle]
pub extern "C" fn gtk_compat_destroy_notify_bool(data: *mut c_void, _closure: *mut c_void) {
    unsafe {
        if data.is_null() { return; }
        let inner_ptr = data as *mut Box<dyn FnMut(*mut c_void) -> i32>;
        let _boxed: Box<Box<dyn FnMut(*mut c_void) -> i32>> = Box::from_raw(inner_ptr);
    }
}

// helper to connect a "clicked" handler using either g_signal_connect_data or g_signal_connect
pub unsafe fn connect_signal(lib_symbols: &crate::symbols::Symbols, instance: *mut c_void, signal_name: &str, cb: Box<dyn FnMut()>, arity: u8) -> Result<u64, String> {
    // box twice so the pointer to Box remains stable (Box<dyn FnMut()> -> *mut Box<dyn FnMut()>)
    let boxed: Box<Box<dyn FnMut()>> = Box::new(Box::new(cb));
    let raw = Box::into_raw(boxed) as *mut c_void;

    let sig_name = CString::new(signal_name).unwrap();
    if let Some(gscd) = lib_symbols.g_signal_connect_data {
        // connect with destroy notify
        let handler_ptr = match arity {
            3 => gtk_compat_trampoline_3 as *const () as *mut c_void,
            _ => gtk_compat_trampoline_2 as *const () as *mut c_void,
        };
        let destroy_ptr = Some(gtk_compat_destroy_notify as unsafe extern "C" fn(*mut c_void, *mut c_void));
        let id = gscd(instance, sig_name.as_ptr(), handler_ptr, raw, destroy_ptr, 0);
        Ok(id)
    } else if let Some(gsc) = lib_symbols.g_signal_connect {
        let handler_ptr = match arity {
            3 => gtk_compat_trampoline_3 as *const () as *mut c_void,
            _ => gtk_compat_trampoline_2 as *const () as *mut c_void,
        };
        let id = gsc(instance, sig_name.as_ptr(), handler_ptr, raw);
        // We didn't register a destroy notify; closure will leak. It's acceptable for the demo.
        Ok(id)
    } else {
        Err("no g_signal_connect available".into())
    }
}

// Connect a signal where the closure wants the raw param pointer (no return value)
pub unsafe fn connect_signal_param(lib_symbols: &crate::symbols::Symbols, instance: *mut c_void, signal_name: &str, cb: Box<dyn FnMut(*mut c_void)>) -> Result<u64, String> {
    // box twice so the pointer to Box remains stable
    let boxed: Box<Box<dyn FnMut(*mut c_void)>> = Box::new(Box::new(cb));
    let raw = Box::into_raw(boxed) as *mut c_void;
    let sig_name = CString::new(signal_name).unwrap();
    if let Some(gscd) = lib_symbols.g_signal_connect_data {
        let handler_ptr = gtk_compat_trampoline_param as *const () as *mut c_void;
        let destroy_ptr = Some(gtk_compat_destroy_notify_param as unsafe extern "C" fn(*mut c_void, *mut c_void));
        let id = gscd(instance, sig_name.as_ptr(), handler_ptr, raw, destroy_ptr, 0);
        Ok(id)
    } else if let Some(gsc) = lib_symbols.g_signal_connect {
        let handler_ptr = gtk_compat_trampoline_param as *const () as *mut c_void;
        let id = gsc(instance, sig_name.as_ptr(), handler_ptr, raw);
        Ok(id)
    } else { Err("no g_signal_connect available".into()) }
}

// trampoline for gesture signals (e.g. GtkGestureClick::pressed/released) with (n_press, x, y, user_data)
#[no_mangle]
pub extern "C" fn gtk_compat_trampoline_gesture(_instance: *mut c_void, n_press: i32, x: f64, y: f64, user_data: *mut c_void) {
    unsafe {
        if user_data.is_null() { return; }
        let inner_ptr = user_data as *mut Box<dyn FnMut(i32, f64, f64)>;
        if inner_ptr.is_null() { return; }
        let closure_ref: &mut dyn FnMut(i32, f64, f64) = &mut **inner_ptr;
        closure_ref(n_press, x, y);
    }
}

#[no_mangle]
pub extern "C" fn gtk_compat_destroy_notify_gesture(data: *mut c_void, _closure: *mut c_void) {
    unsafe {
        if data.is_null() { return; }
        let inner_ptr = data as *mut Box<dyn FnMut(i32, f64, f64)>;
        let _boxed: Box<Box<dyn FnMut(i32, f64, f64)>> = Box::from_raw(inner_ptr);
    }
}

pub unsafe fn connect_signal_gesture(lib_symbols: &crate::symbols::Symbols, instance: *mut c_void, signal_name: &str, cb: Box<dyn FnMut(i32, f64, f64)>) -> Result<u64, String> {
    let boxed: Box<Box<dyn FnMut(i32, f64, f64)>> = Box::new(Box::new(cb));
    let raw = Box::into_raw(boxed) as *mut c_void;
    let sig_name = CString::new(signal_name).unwrap();
    if let Some(gscd) = lib_symbols.g_signal_connect_data {
        let handler_ptr = gtk_compat_trampoline_gesture as *const () as *mut c_void;
        let destroy_ptr = Some(gtk_compat_destroy_notify_gesture as unsafe extern "C" fn(*mut c_void, *mut c_void));
        let id = gscd(instance, sig_name.as_ptr(), handler_ptr, raw, destroy_ptr, 0);
        Ok(id)
    } else if let Some(gsc) = lib_symbols.g_signal_connect {
        let handler_ptr = gtk_compat_trampoline_gesture as *const () as *mut c_void;
        let id = gsc(instance, sig_name.as_ptr(), handler_ptr, raw);
        Ok(id)
    } else { Err("no g_signal_connect available".into()) }
}

// Connect a signal that expects a gboolean/int return from the handler
pub unsafe fn connect_signal_bool(lib_symbols: &crate::symbols::Symbols, instance: *mut c_void, signal_name: &str, cb: Box<dyn FnMut(*mut c_void) -> i32>) -> Result<u64, String> {
    let boxed: Box<Box<dyn FnMut(*mut c_void) -> i32>> = Box::new(Box::new(cb));
    let raw = Box::into_raw(boxed) as *mut c_void;
    let sig_name = CString::new(signal_name).unwrap();
    if let Some(gscd) = lib_symbols.g_signal_connect_data {
        let handler_ptr = gtk_compat_trampoline_bool as *const () as *mut c_void;
        let destroy_ptr = Some(gtk_compat_destroy_notify_bool as unsafe extern "C" fn(*mut c_void, *mut c_void));
        let id = gscd(instance, sig_name.as_ptr(), handler_ptr, raw, destroy_ptr, 0);
        Ok(id)
    } else if let Some(gsc) = lib_symbols.g_signal_connect {
        let handler_ptr = gtk_compat_trampoline_bool as *const () as *mut c_void;
        let id = gsc(instance, sig_name.as_ptr(), handler_ptr, raw);
        Ok(id)
    } else { Err("no g_signal_connect available".into()) }
}
