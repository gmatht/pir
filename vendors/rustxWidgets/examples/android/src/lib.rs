use jni::objects::{JClass, JObject};
use jni::JNIEnv;

#[no_mangle]
pub extern "system" fn Java_com_example_MainActivity_nativeInit(
    mut env: JNIEnv,
    _class: JClass,
    activity: JObject,
    root_layout: JObject,
) {
    // Initialize backend with JVM, Activity, and root ViewGroup
    rustxwidgets::backends::android::init_with_layout(&mut env, &activity, &root_layout)
        .expect("Backend init failed");

    // Create the App (non-blocking on Android)
    let app = rustxwidgets::prelude::App::init()
        .expect("App init failed");

    // Create and configure widgets
    let win = app.create_window().expect("Window failed");
    win.set_title("Hello from RustxWidgets");

    let label = app.create_label("Hello, Android!").expect("Label failed");
    let btn = app.create_button("Click me!").expect("Button failed");

    btn.on_click(move || {
        let _ = rustxwidgets::backends::android::with_env_and_activity(|env, _activity| {
            let log_cls = env.find_class("android/util/Log")?;
            let tag = env.new_string("RustxWidgets")?;
            let msg = env.new_string("Button clicked from Rust!")?;
            env.call_static_method(&log_cls, "d",
                "(Ljava/lang/String;Ljava/lang/String;)I",
                &[(&tag).into(), (&msg).into()],
            )?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        });
    }).expect("on_click failed");

    win.set_child(&label);
    win.set_child(&btn);

    // Returns immediately — Android drives the event loop
    let _ = app.run();
}
