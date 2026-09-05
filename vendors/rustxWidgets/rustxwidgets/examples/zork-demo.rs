use rustxwidgets::prelude::*;
use std::rc::Rc;
use std::cell::RefCell;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = App::init()?;
    let win = app.create_window()?;
    win.set_title("Zork Demo");

    let mut hbox = app.create_box(Orientation::Horizontal, 2)?;
    let label = app.create_label("Count: 0")?;
    let label2 = label.clone();
    let cancel = app.create_button("Cancel")?;
    let ok = app.create_button("OK")?;

    let counter = Rc::new(RefCell::new(0));
    let c2 = counter.clone();

    ok.on_click(move || {
        *c2.borrow_mut() += 1;
        let v = *c2.borrow();
        label2.set_text(&format!("Count: {}", v));
        println!("(OK button clicked! Count = {})", v);
    })?;

    let c3 = counter.clone();
    cancel.on_click(move || {
        println!("(Cancel button clicked! Count = {})", c3.borrow());
    })?;

    hbox.append(&cancel);
    hbox.append(&label);
    hbox.append(&ok);

    let entry = app.create_entry()?;
    entry.set_text("Hello, Zork!");

    let mut vbox = app.create_box(Orientation::Vertical, 1)?;
    vbox.append(&hbox);
    vbox.append(&entry);

    win.set_child(&vbox);
    win.present();

    println!("Welcome to Zork!");
    println!("Type 'help' for commands, 'quit' to exit.");
    app.run().map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}
