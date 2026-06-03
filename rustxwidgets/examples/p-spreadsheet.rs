use rustxwidgets::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = App::init()?;

    let window = app.create_window()?;
    window.set_title("Pancurses Spreadsheet Demo");
    window.set_default_size(80, 24);

    let ss = app.create_spreadsheet(100, 26)?;

    // Fill some cells
    ss.set_cell(0, 0, "Hello");
    ss.set_cell(0, 1, "World");
    ss.set_cell(0, 2, "Spreadsheet");
    ss.set_cell(1, 0, "This is a long text that demonstrates overflow into adjacent empty cells in the terminal");
    ss.set_cell(2, 0, "42");
    ss.set_cell(2, 1, "= 6 * 7");
    ss.set_cell(3, 0, "Tab");
    ss.set_cell(3, 1, "moves");
    ss.set_cell(3, 2, "right");
    ss.set_cell(5, 0, "Try:");
    ss.set_cell(5, 1, "Arrows navigate, Enter edits, Esc cancels");

    window.set_child(&ss);
    window.present();

    println!("=== Pancurses Spreadsheet Demo ===");
    println!("Arrow keys: navigate cells");
    println!("Enter: edit cell / commit and move down");
    println!("Tab: move right");
    println!("Backspace/Delete: edit text");
    println!("Escape: cancel edit / quit");
    println!("Ctrl+C: quit");

    app.run().map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}
