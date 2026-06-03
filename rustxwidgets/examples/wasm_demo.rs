use rustxwidgets::prelude::*;
use std::rc::Rc;
use std::cell::RefCell;

fn main() {
    let app = App::init().expect("init");
    let win = app.create_window().expect("window");
    win.set_title("rustxwidgets WASM demo");

    let hbox = app.create_box(Orientation::Horizontal, 6).expect("box");
    let label = app.create_label("Count: 0").expect("label");
    let button = app.create_button("Click me").expect("button");

    let counter = Rc::new(RefCell::new(0));
    let c2 = counter.clone();
    let label2 = label.clone();

    button.on_click(move || {
        *c2.borrow_mut() += 1;
        let v = *c2.borrow();
        label2.set_text(&format!("Count: {}", v));
        web_sys::console::log_1(&format!("click {}", v).into());
    }).expect("on_click");

    button.emit_clicked().expect("emit_clicked");

    hbox.append(&label);
    hbox.append(&button);
    win.set_child(&hbox);
    win.present();

    web_sys::console::log_1(&"WASM demo running".into());
    app.run().expect("run");
}
