use crate::grid::CellAddr;
use crate::ui_core;
use rustxwidgets::backends_pancurses_adapter::*;
use unicode_width::UnicodeWidthStr;

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

    // First pass: measure content widths for each main column
    let margin_w = 4u32;
    let right_w = 4u32;
    let mut main_col_widths: Vec<u32> = Vec::new();
    for ci in 0..main_cols {
        let mut max_w = 0usize;
        for r in 0..main_rows {
            let addr = CellAddr::Main { row: r as u32, col: ci as u32 };
            if let Some(val) = sheet_rec.grid.get(&addr) {
                let display = ui_core::format_cell_display(&sheet_rec.grid, &addr, val);
                max_w = max_w.max(display.chars().count());
            }
        }
        let label = crate::addr::ui_column_fragment(margin_cols + ci, main_cols);
        let label_w = label.chars().count();
        let min_w = (label_w + 3).max(4);
        let w = max_w.max(min_w).min(crate::grid::DEFAULT_MAX_COL_WIDTH) as u32;
        main_col_widths.push(w);
    }

    // Second pass: populate cells with aligned, width-aware formatted display text
    for r in 0..main_rows {
        for c in 0..main_cols {
            let addr = CellAddr::Main { row: r as u32, col: c as u32 };
            if let Some(val) = sheet_rec.grid.get(&addr) {
                let formatted = ui_core::format_cell_display(&sheet_rec.grid, &addr, val);
                let cw = main_col_widths[c] as usize;
                let fw = formatted.width();
                let align = ui_core::effective_cell_align(&sheet_rec.grid, &addr, &formatted);
                let inner = if fw > cw {
                    ui_core::shrink_numeric_display(&formatted, cw)
                        .or_else(|| ui_core::exponential_numeric_display(&formatted, cw))
                        .unwrap_or_else(|| ui_core::truncate_with_ellipsis(&formatted, cw))
                } else {
                    formatted
                };
                let disp = ui_core::align_cell_display(inner.clone(), cw, align);
                spreadsheet.set_cell(r as u32, c as u32, &disp);
                // Store the raw (unpadded) display value for the formula bar
                spreadsheet.set_raw_cell(r as u32, c as u32, &inner);
            }
        }
    }
    let mut layout: Vec<(u32, u32, String)> = Vec::new();
    // Last left margin column
    if margin_cols > 0 {
        let label = crate::addr::ui_column_fragment(margin_cols - 1, main_cols);
        layout.push(((margin_cols - 1) as u32, margin_w, label));
    }
    // Main columns
    for (ci, &w) in main_col_widths.iter().enumerate() {
        let global_ci = margin_cols + ci;
        let label = crate::addr::ui_column_fragment(global_ci, main_cols);
        layout.push((global_ci as u32, w, label));
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

    // Border title — match ratatui: use ops_applied
    let total_ops = app.core.ops_applied;
    let border_title = format!("corro  {}r × {}c  ops {}",
        main_rows, main_cols, total_ops);
    spreadsheet.set_border_title(&border_title);

    // Menu — match ratatui default
    spreadsheet.set_menu_text(" [File]   Edit    Insert    Format    Sheet    Help");

    // Status bar — match ratatui hints_line for normal mode
    spreadsheet.set_status_text("  type/F2·edit; Ctrl+C·copy; Ctrl+X·cut; Ctrl+V·paste; Ctrl+;·date; Ctrl+:·time; Ctrl+S·save; F1·help");

    // Formula bar trailing — show loaded workbook status with revision
    if let Some(ref path) = app.core.path {
        let status = format!("   ·  Loaded workbook {} @ revision {}", path.display(), app.core.ops_applied);
        spreadsheet.set_formula_bar_trailing(&status);
    }

    win.set_child(&spreadsheet);
    rustxwidgets::backends::pancurses::set_focus(spreadsheet.id());
    win.present();

    _backend.run().map_err(|e| format!("pancurses error: {e}"))?;
    Ok(())
}
