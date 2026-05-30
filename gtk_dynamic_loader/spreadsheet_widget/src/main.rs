use gtk_compat::{Loader, Application, Window, BoxWidget, Orientation, Grid, Label};
use spreadsheet_widget::widget::sheet::{Sheet, col_label_to_index};
use spreadsheet_widget::SpreadsheetWidget;
use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--prefer-gtk3" || a == "-3") {
        std::env::set_var("GTK_DLOPEN_PREFER_GTK3", "1");
    }
    let loader = Loader::new()?;
    let app = Application::new(loader.clone(), Some("org.example.SpreadsheetWidgetDemo"))?;

    let win = Window::new(loader.clone())?;
    win.set_title("Spreadsheet Widget Demo");

    // Create a sheet with huge logical size
    let mut sheet = Sheet::new(1 << 30, 1_000_000);
    sheet.set_cell(1, 0, "This is a very long text that should overflow into adjacent blank cells for demo purposes".into());

    // create a viewport of 20 rows x 10 cols
    let mut widget = SpreadsheetWidget::new(loader.clone(), sheet, 20, 10)?;
    let widget = std::sync::Arc::new(std::sync::Mutex::new(widget));

    // Top controls: go-to
    let controls = Grid::new(loader.clone())?;
    let col_label = Label::new(loader.clone(), "Col")?;
    let row_label = Label::new(loader.clone(), "Row")?;
    let col_entry = gtk_compat::Entry::new(loader.clone())?;
    let row_entry = gtk_compat::Entry::new(loader.clone())?;
    let go_btn = gtk_compat::Button::with_label(loader.clone(), "Go")?;

    controls.attach(&col_label, 0, 0, 1, 1);
    controls.attach(&col_entry, 1, 0, 1, 1);
    controls.attach(&row_label, 2, 0, 1, 1);
    controls.attach(&row_entry, 3, 0, 1, 1);
    controls.attach(&go_btn, 4, 0, 1, 1);

    // Hook go button
    let w_clone = widget.clone();
    go_btn.connect_clicked(move || {
        let col_s = col_entry.get_text().unwrap_or_default();
        let row_s = row_entry.get_text().unwrap_or_default();
        if let Some(col_idx) = col_label_to_index(&col_s.trim().to_uppercase()) {
            if let Ok(row_idx) = row_s.trim().parse::<u32>() {
                if let Ok(mut w) = w_clone.lock() {
                    let _ = w.go_to(row_idx, col_idx);
                }
            }
        }
    })?;

    let vbox = BoxWidget::new(loader.clone(), Orientation::Vertical, 6)?;
    vbox.append(&controls);
    if let Ok(w) = widget.lock() {
        vbox.append(w.as_widget());
    }
    win.set_child(&vbox);
    win.present();

    app.run()?;
    Ok(())
}
