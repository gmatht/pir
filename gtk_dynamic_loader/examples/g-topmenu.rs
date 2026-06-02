use gtk_dynamic_loader::{Loader, Application, Window, BoxWidget, Orientation, Label, Menu, MenuBar, SimpleAction};
use std::cell::RefCell;
use std::rc::Rc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--prefer-gtk3" || a == "-3") {
        std::env::set_var("GTK_DLOPEN_PREFER_GTK3", "1");
    }

    let loader = Loader::new()?;
    let app = Application::new(loader.clone(), Some("org.example.TopMenuDemo"))?;

    // Build File menu
    let mut file_menu = Menu::new(loader.clone())?;
    file_menu.append("New",       "app.new");
    file_menu.append("Open",      "app.open");
    file_menu.append("Save",      "app.save");
    file_menu.append("Save As...","app.save_as");

    // Insert submenu under File
    let mut insert_menu = Menu::new(loader.clone())?;
    insert_menu.append("Row", "app.insert_row");
    insert_menu.append("Col", "app.insert_col");
    file_menu.append_submenu("Insert", &insert_menu);

    // Disabled entry (action doesn't exist → greyed out)
    file_menu.append("Unavailable", "app.unavailable");

    file_menu.append("Quit",      "app.quit");

    // Build Edit menu
    let mut edit_menu = Menu::new(loader.clone())?;
    edit_menu.append("Undo",       "app.undo");
    edit_menu.append("Redo",       "app.redo");
    edit_menu.append("Cut",        "app.cut");
    edit_menu.append("Copy",       "app.copy");
    edit_menu.append("Paste",      "app.paste");
    edit_menu.append("Select All", "app.select_all");

    // Build Help menu
    let mut help_menu = Menu::new(loader.clone())?;
    help_menu.append("About", "app.about");

    // Assemble menu bar model
    let mut menubar_model = Menu::new(loader.clone())?;
    menubar_model.append_submenu("File", &file_menu);
    menubar_model.append_submenu("Edit", &edit_menu);
    menubar_model.append_submenu("Help", &help_menu);

    // Create window
    let win = Window::new(loader.clone())?;
    win.set_title("GTK Dynamic Loader — Top Menu Demo");

    // Status label that shows which action was triggered
    let status_label = Label::new(loader.clone(), "Ready")?;
    let status = Rc::new(RefCell::new(status_label));

    // Register the application so actions can be added and queried
    app.register()?;

    // Create actions and wire them to update the status label
    for name in &["new", "open", "save", "save_as", "insert_row", "insert_col",
                  "undo", "redo", "cut", "copy", "paste", "select_all",
                  "about"] {
        let action = SimpleAction::new(loader.clone(), name)?;
        let action_name = name.to_string();
        let status = status.clone();
        action.connect_activate(move |_param| {
            let msg = format!("Action: {}", action_name);
            println!("{}", msg);
            if let Ok(s) = status.try_borrow_mut() {
                s.set_text(&msg);
            }
            if action_name == "quit" {
                std::process::exit(0);
            }
        })?;
        app.add_action(&action)?;
    }

    // Insert the application's action group so menu items can resolve "app.*" actions
    unsafe { win.insert_action_group("app", app.as_ptr()); }

    let menubar = unsafe { MenuBar::new(loader.clone(), &menubar_model, app.as_ptr())? };

    // Pack menubar + status label in a vertical box
    let vbox = BoxWidget::new(loader.clone(), Orientation::Vertical, 0)?;
    vbox.append(&menubar);
    vbox.append(&*status.borrow());

    win.set_child(&vbox);
    win.present();

    app.run()?;
    Ok(())
}
