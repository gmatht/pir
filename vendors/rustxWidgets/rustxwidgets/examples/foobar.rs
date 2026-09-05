use rustxwidgets::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = App::init()?;

    // Status label
    let status_label = app.create_label("Ready")?;
    let _label = status_label.clone();

    // Build simple menu bar items
    let mut file_menu = app.create_menu()?;
    file_menu.append("New", "app.new");
    file_menu.append("Quit", "app.quit");

    let mut edit_menu = app.create_menu()?;
    edit_menu.append("Undo", "app.undo");
    edit_menu.append("Redo", "app.redo");

    // Assemble menu bar model
    let mut menubar_model = app.create_menu()?;
    menubar_model.append_submenu("File", &file_menu);
    menubar_model.append_submenu("Edit", &edit_menu);

    // Create window
    let window = app.create_window()?;
    window.set_title("RustXWidgets Top Menu Demo");

    // Create MenuBar
    let menubar = unsafe { app.create_menubar(&menubar_model, window.hwnd())? };

    // Pack menubar + status label in a vertical box
    let mut vbox = app.create_box(
        rustxwidgets::prelude::Orientation::Vertical, 0,
    )?;
    vbox.append(&menubar);
    vbox.append(&status_label);

    window.set_child(&vbox);
    window.present();

    println!("Running topmenu demo. Press ESC to quit.");
    app.run().map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}
