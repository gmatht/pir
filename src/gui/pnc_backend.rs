use crate::grid::{CellAddr, HEADER_ROWS, MARGIN_COLS};
use crate::ui_core::{self, main_col_window, right_nonblank_end};
use rustxwidgets::backends_pancurses_adapter::*;
use unicode_width::UnicodeWidthStr;

pub fn run_pancurses(app: &mut super::App) -> Result<(), Box<dyn std::error::Error>> {
    let _backend = rustxwidgets::backends::pancurses::init()
        .map_err(|e| format!("pancurses init failed: {e}"))?;

    let win = create_window()?;
    win.set_title("corro");

    let sheet_rec = app.core.workbook.active_sheet().clone();
    let g = &sheet_rec.grid;
    let hr = HEADER_ROWS;
    let mr = g.main_rows();
    let mc = g.main_cols();
    let lm = MARGIN_COLS;
    let rm = MARGIN_COLS;
    let cursor = app.core.cursor;

    // ── Visible rows (matching ratatui's visible_row_indices) ──────────
    let mut header_rows: Vec<usize> = Vec::new();
    let mut footer_rows: Vec<usize> = Vec::new();
    for (addr, _) in g.iter_nonempty() {
        match addr {
            CellAddr::Header { row, .. } => header_rows.push(row as usize),
            CellAddr::Footer { row, .. } => footer_rows.push(hr + mr + row as usize),
            _ => {}
        }
    }
    let main_order = g.sorted_main_rows();
    header_rows.sort_unstable();
    header_rows.dedup();
    footer_rows.sort_unstable();
    footer_rows.dedup();

    // Fill remaining viewport space with blank footer rows (~ 43 total visible rows)
    let content_count = header_rows.len() + main_order.len() + footer_rows.len();
    let dim_rows = 43usize;
    let blank_needed = dim_rows.saturating_sub(content_count);
    for i in 0..blank_needed {
        footer_rows.push(hr + mr + i);
    }
    footer_rows.sort_unstable();
    footer_rows.dedup();

    let mut display_rows: Vec<usize> =
        Vec::with_capacity(header_rows.len() + main_order.len() + footer_rows.len());
    display_rows.extend(header_rows.iter());
    display_rows.extend(main_order.iter().map(|r| hr + r));
    display_rows.extend(footer_rows.iter());

    // ── Visible columns (matching ratatui's visible_col_indices) ──────
    let right_start = lm + mc;
    let (main_lo, main_hi) = main_col_window(&sheet_rec, cursor);

    let right_band: Vec<usize> = match right_nonblank_end(&sheet_rec) {
        Some(end) => (0..=end).map(|i| right_start + i).collect(),
        None => Vec::new(),
    };

    let mut col_ixs: Vec<usize> = Vec::new();
    if lm > 0 {
        col_ixs.push(lm - 1);
    }
    col_ixs.extend((0..=main_hi as usize).map(|ci| lm + ci));
    for i in 0..rm {
        let gc = right_start + i;
        if !col_ixs.contains(&gc) {
            col_ixs.push(gc);
        }
    }
    col_ixs.extend(right_band.iter());
    col_ixs.sort_unstable();
    col_ixs.dedup();

    // ── Column layout with correct widths ──────────────────────────────
    let mut layout: Vec<(u32, u32, String)> = Vec::new();
    for &c in &col_ixs {
        let w = g.col_width(c).max(1) as u32;
        let label = crate::addr::ui_column_fragment(c, mc);
        layout.push((c as u32, w, label));
    }

    // ── Spreadsheet ────────────────────────────────────────────────────
    let total_rows = display_rows.len() as u32;
    let total_cols = layout.len() as u32;
    let spreadsheet = create_spreadsheet(total_rows, total_cols)?;

    // Row labels
    let mut row_labels: Vec<(u32, String)> = Vec::new();
    for (idx, &r) in display_rows.iter().enumerate() {
        let label = crate::addr::ui_row_label(r, mr);
        row_labels.push((idx as u32, label));
    }
    spreadsheet.set_row_labels(row_labels);

    // Cell data for main data rows
    for (ri, &logical_row) in display_rows.iter().enumerate() {
        if logical_row >= hr && logical_row < hr + mr {
            let main_row = (logical_row - hr) as u32;
            for &c in &col_ixs {
                if c >= lm && c < lm + mc {
                    let main_col = (c - lm) as u32;
                    let addr = CellAddr::Main {
                        row: main_row,
                        col: main_col,
                    };
                    if let Some(val) = g.get(&addr) {
                        let formatted =
                            ui_core::format_cell_display(g, &addr, val);
                        let cw = g.col_width(c).max(1) as usize;
                        let fw = formatted.width();
                        let align =
                            ui_core::effective_cell_align(g, &addr, &formatted);
                        let inner = if fw > cw {
                            ui_core::shrink_numeric_display(&formatted, cw)
                                .or_else(|| {
                                    ui_core::exponential_numeric_display(
                                        &formatted, cw,
                                    )
                                })
                                .unwrap_or_else(|| {
                                    ui_core::truncate_with_ellipsis(
                                        &formatted, cw,
                                    )
                                })
                        } else {
                            formatted
                        };
                        let disp =
                            ui_core::align_cell_display(inner.clone(), cw, align);
                        spreadsheet.set_cell(ri as u32, main_col, &disp);
                        spreadsheet.set_raw_cell(ri as u32, main_col, &inner);
                        // Set raw cell at (0, 0) for A1 so the formula bar
                        // (cursor defaults to row 0) shows the correct value
                        if main_row == 0 && main_col == 0 {
                            spreadsheet.set_raw_cell(0, 0, &inner);
                        }
                    }
                }
            }
        }
    }

    spreadsheet.set_column_layout(layout);
    spreadsheet.set_grid_config(lm as u32, mc as u32);

    // Border title
    let total_ops = app.core.ops_applied;
    let border_title =
        format!("corro  {}r × {}c  ops {}", mr, mc, total_ops);
    spreadsheet.set_border_title(&border_title);

    // Menu
    spreadsheet.set_menu_text(" [File]   Edit    Insert    Format    Sheet    Help");

    // Status bar
    spreadsheet.set_status_text(
        "  type/F2·edit; Ctrl+C·copy; Ctrl+X·cut; Ctrl+V·paste; Ctrl+;·date; Ctrl+:·time; Ctrl+S·save; F1·help",
    );

    // Formula bar trailing
    if let Some(ref path) = app.core.path {
        let status = format!(
            "   ·  Loaded workbook {} @ revision {}",
            path.display(),
            app.core.ops_applied
        );
        spreadsheet.set_formula_bar_trailing(&status);
    }

    win.set_child(&spreadsheet);
    rustxwidgets::backends::pancurses::set_focus(spreadsheet.id());
    win.present();

    _backend.run().map_err(|e| format!("pancurses error: {e}"))?;
    Ok(())
}
