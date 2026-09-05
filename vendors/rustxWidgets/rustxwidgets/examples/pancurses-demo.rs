use rustxwidgets::prelude::*;
use std::rc::Rc;
use std::cell::RefCell;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = App::init()?;
    let win = app.create_window()?;
    win.set_title("Pancurses Demo");

    let mut hbox = app.create_box(Orientation::Horizontal, 2)?;
    let label = app.create_label("Count: 0")?;
    let label2 = label.clone();
    let button = app.create_button("Click me")?;

    let counter = Rc::new(RefCell::new(0));
    let c2 = counter.clone();

    button.on_click(move || {
        *c2.borrow_mut() += 1;
        let v = *c2.borrow();
        label2.set_text(&format!("Count: {}", v));
        println!("Clicked! Count = {}", v);
    })?;

    hbox.append(&label);
    hbox.append(&button);

    // also add an entry widget
    let entry = app.create_entry()?;
    entry.set_text("Type here");
    let entry2 = entry.clone();
    entry.connect_changed(move || {
        if let Some(t) = entry2.get_text() {
            println!("Entry: {}", t);
        }
    })?;

    // add a checkbutton
    let cb = app.create_checkbutton("Check me")?;
    let _cb2 = cb.clone();
    cb.connect_toggled(move || {
        println!("Checkbox toggled");
    })?;

    // stack vertically: hbox on top, entry below, checkbox below
    let mut vbox = app.create_box(Orientation::Vertical, 1)?;
    vbox.append(&hbox);
    vbox.append(&entry);
    vbox.append(&cb);

    win.set_child(&vbox);
    win.present();

    println!("Running pancurses demo. Press ESC to quit.");
    app.run().map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}
