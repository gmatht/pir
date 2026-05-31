use rustxwidgets::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--prefer-gtk3" || a == "-3") {
        std::env::set_var("GTK_DLOPEN_PREFER_GTK3", "1");
    }

    let app = App::init()?;
    let loader = rustxwidgets::backends::gtk::loader().expect("loader initialized");

    // Build File menu
    let mut file_menu = app.create_menu()?;
    file_menu.append("New",     "app.new");
    file_menu.append("Open",    "app.open");
    file_menu.append("Save",    "app.save");
    file_menu.append("Save As...", "app.save_as");

    // Insert submenu under File
    let mut insert_menu = app.create_menu()?;
    insert_menu.append("Row", "app.insert_row");
    insert_menu.append("Col", "app.insert_col");
    file_menu.append_submenu("Insert", &insert_menu);

    // Disabled entry (action doesn't exist → greyed out)
    file_menu.append("Unavailable", "app.unavailable");

    file_menu.append("Quit",    "app.quit");

    // Build Edit menu
    let mut edit_menu = app.create_menu()?;
    edit_menu.append("Undo",        "app.undo");
    edit_menu.append("Redo",        "app.redo");
    edit_menu.append("Cut",         "app.cut");
    edit_menu.append("Copy",        "app.copy");
    edit_menu.append("Paste",       "app.paste");
    edit_menu.append("Select All",  "app.select_all");

    // Build Help menu
    let mut help_menu = app.create_menu()?;
    help_menu.append("About", "app.about");

    // Assemble menu bar model
    let mut menubar_model = app.create_menu()?;
    menubar_model.append_submenu("File", &file_menu);
    menubar_model.append_submenu("Edit", &edit_menu);
    menubar_model.append_submenu("Help", &help_menu);

    // Create GApplication to hold actions
    let gapp = gtk_dynamic_loader::Application::new(loader.clone(), Some("org.example.TopMenuDemo"))?;

    // Status label that shows which action was triggered
    let status_label = app.create_label("Ready")?;

    // Register the application so actions can be added and queried
    gapp.register()?;

    // Create actions and wire them to update the status label
    for name in &["new", "open", "save", "save_as", "insert_row", "insert_col", "quit",
                  "undo", "redo", "cut", "copy", "paste", "select_all",
                  "about"] {
        let action = gtk_dynamic_loader::SimpleAction::new(loader.clone(), name)?;
        let action_name = name.to_string();
        let label = status_label.clone();
        action.connect_activate(move |_param| {
            let msg = format!("Action: {}", action_name);
            println!("{}", msg);
            label.set_text(&msg);
            if action_name == "quit" {
                std::process::exit(0);
            }
        })?;
        gapp.add_action(&action)?;
    }

    // Create window
    let window = app.create_window()?;
    window.set_title("RustXWidgets Top Menu Demo");

    // Insert the application's action group so menu items can resolve "app.*" actions
    window.insert_action_group("app", gapp.as_ptr());

    // Create MenuBar widget from the model (pass GApplication as action group for GTK3)
    let menubar = app.create_menubar(&menubar_model, gapp.as_ptr())?;

    // Pack menubar + status label in a vertical box
    let vbox = rustxwidgets::backends_gtk_adapter::create_box(
        rustxwidgets::backends_gtk_adapter::Orientation::Vertical, 0,
    )?;
    vbox.append(&menubar);
    vbox.append(&status_label);

    window.set_child(&vbox);
    window.present();

    app.run().map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}
