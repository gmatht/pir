use rustxwidgets::prelude::*;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = App::init()?;
    println!("init OK");
    app.run()?;
    Ok(())
}
