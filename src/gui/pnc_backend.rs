use crate::grid::CellAddr;
use crate::ui_core;
use rustxwidgets::backends_pancurses_adapter::*;

pub fn run_pancurses(app: &mut super::App) -> Result<(), Box<dyn std::error::Error>> {
    let _backend = rustxwidgets::backends::pancurses::init()
        .map_err(|e| format!("pancurses init failed: {e}"))?;

    let win = create_window()?;
    win.set_title("corro");

    let sheet_rec = app.core.workbook.active_sheet().clone();
    let main_rows = sheet_rec.grid.main_rows();
    let main_cols = sheet_rec.grid.main_cols();
    let header_rows = crate::grid::HEADER_ROWS;
    let margin_cols = crate::grid::MARGIN_COLS;
    let right_margin_cols = margin_cols; // symmetric

    // Total virtual rows: main data rows + extra footer rows (match ratatui: min 50 total)
    let total_rows = main_rows.max(50) as u32;
    let total_cols = (margin_cols + main_cols + right_margin_cols).max(10) as u32;

    let spreadsheet = create_spreadsheet(total_rows, total_cols)?;

    // Populate cells with formatted display text (only main data rows, main columns)
    for r in 0..main_rows {
        for c in 0..main_cols {
            let addr = CellAddr::Main { row: r as u32, col: c as u32 };
            if let Some(val) = sheet_rec.grid.get(&addr) {
                let display = ui_core::format_cell_display(&sheet_rec.grid, &addr, val);
                spreadsheet.set_cell(r as u32, c as u32, &display);
            }
        }
    }

    // Build column layout: margin col (width 4), main cols (width 10), right-margin cols (width 4)
    let default_col_w = 10u32;
    let margin_w = 4u32;
    let right_w = 4u32;
    let mut layout: Vec<(u32, u32, String)> = Vec::new();
    // Last left margin column
    if margin_cols > 0 {
        let label = crate::addr::ui_column_fragment(margin_cols - 1, main_cols);
        layout.push(((margin_cols - 1) as u32, margin_w, label));
    }
    // Main columns
    for ci in 0..main_cols {
        let global_ci = margin_cols + ci;
        let label = crate::addr::ui_column_fragment(global_ci, main_cols);
        layout.push((global_ci as u32, default_col_w, label));
    }
    // Right-margin columns (as many as fit)
    for ci in 0..right_margin_cols {
        let global_ci = margin_cols + main_cols + ci;
        let label = crate::addr::ui_column_fragment(global_ci, main_cols);
        layout.push((global_ci as u32, right_w, label));
    }
    spreadsheet.set_column_layout(layout);

    // Row labels for all virtual rows
    let mut row_labels: Vec<(u32, String)> = Vec::new();
    for r in 0..total_rows {
        let logical_row = header_rows + r as usize;
        let label = crate::addr::ui_row_label(logical_row, main_rows);
        row_labels.push((r, label));
    }
    spreadsheet.set_row_labels(row_labels);

    // Grid config
    spreadsheet.set_grid_config(margin_cols as u32, main_cols as u32);

    // Border title — match ratatui
    let total_ops = (app.core.offset as usize + main_rows + 1).max(1);
    let border_title = format!("corro  {}r × {}c  ops {}",
        main_rows, main_cols, total_ops.saturating_sub(1));
    spreadsheet.set_border_title(&border_title);

    // Menu — match ratatui default
    spreadsheet.set_menu_text(" [File]   Edit    Insert    Format    Sheet    Help");

    // Status bar — match ratatui hints_line for normal mode
    spreadsheet.set_status_text("  type/F2·edit; Ctrl+C·copy; Ctrl+X·cut; Ctrl+V·paste; Ctrl+;·date; Ctrl+:·time; Ctrl+S·save; F1·help");

    // Formula bar trailing — show loaded workbook status
    if let Some(ref path) = app.core.path {
        let status = format!("   ·  Loaded workbook {}", path.display());
        spreadsheet.set_formula_bar_trailing(&status);
    }

    win.set_child(&spreadsheet);
    rustxwidgets::backends::pancurses::set_focus(spreadsheet.id());
    win.present();

    _backend.run().map_err(|e| format!("pancurses error: {e}"))?;
    Ok(())
}
