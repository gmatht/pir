use rustxwidgets::prelude::*;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = App::init()?;
    println!("init ok");
    let window = app.create_window()?;
    window.set_title("RustXWidgets Top Menu Demo");
    let mut fm = app.create_menu()?;
    fm.append("New", "app.new");
    fm.append("Quit", "app.quit");
    let mut mb_model = app.create_menu()?;
    mb_model.append_submenu("File", &fm);
    let _menubar = unsafe { app.create_menubar(&mb_model, window.hwnd())? };
    let mut vbox = app.create_box(rustxwidgets::prelude::Orientation::Vertical, 0)?;
    vbox.append(&_menubar);
    let sl = app.create_label("Ready")?;
    vbox.append(&sl);
    window.set_child(&vbox);
    window.present();
    println!("Running topmenu demo.");
    app.run()?;
    Ok(())
}
