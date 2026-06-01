use rustxwidgets::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = App::init()?;

    let dialog = app.create_dialog()?;
    dialog.set_title("Widget Test Dialog");
    dialog.set_default_size(450, 500);

    let vbox = rustxwidgets::backends_gtk_adapter::create_box(
        rustxwidgets::backends_gtk_adapter::Orientation::Vertical, 6,
    )?;

    // --- DropDown ---
    let dd = app.create_dropdown(&["Choice 1", "Choice 2", "Choice 3"])?;
    dd.set_active(1);
    assert_eq!(dd.get_active(), 1);
    println!("DropDown: OK (active={})", dd.get_active());
    let dd_cb = dd.clone();
    dd.connect_changed(move || println!("  [user] DropDown changed to active={}", dd_cb.get_active()))?;
    vbox.append(&dd);

    // --- CheckButton ---
    let cb = app.create_checkbutton("Enable feature")?;
    cb.set_active(true);
    assert!(cb.is_active());
    cb.set_active(false);
    assert!(!cb.is_active());
    println!("CheckButton: OK");
    let cb_cb = cb.clone();
    cb.connect_toggled(move || println!("  [user] CheckButton toggled: active={}", cb_cb.is_active()))?;
    vbox.append(&cb);

    // --- RadioButton ---
    let rb_a = app.create_radiobutton("Option A")?;
    let rb_b = app.create_radiobutton("Option B")?;
    let rb_c = app.create_radiobutton("Option C")?;
    rb_a.set_active(true);
    assert!(rb_a.is_active());
    println!("RadioButton: OK");
    let a_cb = rb_a.clone();
    rb_a.connect_toggled(move || println!("  [user] RadioButton A toggled: active={}", a_cb.is_active()))?;
    let b_cb = rb_b.clone();
    rb_b.connect_toggled(move || println!("  [user] RadioButton B toggled: active={}", b_cb.is_active()))?;
    let c_cb = rb_c.clone();
    rb_c.connect_toggled(move || println!("  [user] RadioButton C toggled: active={}", c_cb.is_active()))?;
    vbox.append(&rb_a);
    vbox.append(&rb_b);
    vbox.append(&rb_c);

    // --- Entry ---
    let entry = app.create_entry()?;
    entry.set_text("Hello");
    assert_eq!(entry.get_text(), Some("Hello".to_string()));
    entry.set_text("World");
    assert_eq!(entry.get_text(), Some("World".to_string()));
    println!("Entry: OK");
    let entry_cb = entry.clone();
    entry.connect_changed(move || {
        let text = entry_cb.get_text().unwrap_or_default();
        println!("  [user] Entry changed: \"{}\"", text);
    })?;
    vbox.append(&entry);

    // --- TextView ---
    let tv = app.create_textview()?;
    tv.set_text("Multi-line\ntext\narea");
    assert!(tv.get_text().unwrap_or_default().contains("Multi-line"));
    tv.set_wrap_mode(0);
    tv.set_size_request(200, 80);
    println!("TextView: OK (text={:?})", tv.get_text());
    vbox.append(&tv);

    // Append the vbox into the dialog's content area
    dialog.append_content_area(&vbox);

    dialog.add_button("OK", 1);
    dialog.add_button("Cancel", 0);

    dialog.connect_response(|response_id| {
        println!("Dialog closed with response: {}", response_id);
        std::process::exit(0);
    })?;

    println!("\n=== All widget tests passed! ===");

    dialog.present();
    app.run().map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}
