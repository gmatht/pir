use gtk_dynamic_loader::{Loader, Window, BoxWidget, Label, Button, Orientation};

#[test]
fn create_ui_and_click_programmatically() {
    // This test reproduces the sequence that previously caused a crash or blank UI:
    // create loader, create window/box/label/button, connect click handler, emit clicked, and verify label text.
    let loader = Loader::new().expect("Loader::new failed");

    let win = Window::new(loader.clone()).expect("Window::new failed");
    win.set_title("test");

    let hbox = BoxWidget::new(loader.clone(), Orientation::Horizontal, 4).expect("Box::new failed");
    let label = Label::new(loader.clone(), "Count: 0").expect("Label::new failed");
    let button = Button::with_label(loader.clone(), "Click").expect("Button::with_label failed");

    // connect a closure that updates the label
    let label2 = label.clone();
    button.connect_clicked(move || {
        label2.set_text("Count: 1");
    }).expect("connect_clicked failed");

    // append children and set child
    hbox.append(&label);
    hbox.append(&button);
    win.set_child(&hbox);

    // programmatically emit clicked and verify label text changed
    button.emit_clicked().expect("emit_clicked failed");
    let got = label.get_text().unwrap_or_default();
    assert_eq!(got, "Count: 1");
}
