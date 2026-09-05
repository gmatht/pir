fn main() {
    #[cfg(all(feature = "gtk", target_os = "linux", not(feature = "zork")))]
    {
        let _ = demo_main();
    }
    #[cfg(not(all(feature = "gtk", target_os = "linux", not(feature = "zork"))))]
    println!("skipped (requires GTK on Linux)");
}

#[cfg(all(feature = "gtk", target_os = "linux", not(feature = "zork")))]
fn demo_main() -> Result<(), Box<dyn std::error::Error>> {
    use rustxwidgets::prelude::*;
    use std::rc::Rc;
    use std::cell::RefCell;

    let app = App::init()?;
    let win = app.create_window()?;
    win.set_title("Rust rustxwidgets demo");

    let hbox = rustxwidgets::backends_gtk_adapter::create_box(
        rustxwidgets::backends_gtk_adapter::Orientation::Horizontal,
        6,
    )?;
    let label = app.create_label("Count: 0")?;
    let button = app.create_button("Click me")?;

    let counter = Rc::new(RefCell::new(0));
    let c2 = counter.clone();
    let label2 = label.clone();

    button.on_click(move || {
        *c2.borrow_mut() += 1;
        let v = *c2.borrow();
        label2.set_text(&format!("Count: {}", v));
        println!("closure invoked, updated label to Count: {}", v);
    })?;

    button.emit_clicked()?;

    hbox.append(&label);
    hbox.append(&button);
    win.set_child(&hbox);
    win.present();

    app.run().map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}
