use rustxwidgets::prelude::*;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = App::init()?;
    println!("init ok");
    let mut fm = app.create_menu()?;
    fm.append("New", "app.new");
    let mut mm = app.create_menu()?;
    mm.append_submenu("File", &fm);
    let w = app.create_window()?;
    let _mb = unsafe { app.create_menubar(&mm, w.hwnd())? };
    println!("all ok");
    app.run()?;
    Ok(())
}
