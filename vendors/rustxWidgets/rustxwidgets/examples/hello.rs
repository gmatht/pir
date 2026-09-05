use rustxwidgets::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = App::init()?;
    // For now just run — real example requires backend wiring which is in-progress
    println!("rustxwidgets initialized: {:?}", std::any::type_name::<App>());
    // Not calling app.run() to avoid platform-specific blocking in tests.
    Ok(())
}
