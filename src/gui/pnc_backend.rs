use crate::formula::cell_effective_display;
use crate::grid::{CellAddr, ColumnAddr, HEADER_ROWS, MARGIN_COLS};
use crate::ui_core::align_cell_display;
use crate::ui_core::{
    self, main_col_window, right_nonblank_end,
};
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
    let (_main_lo, main_hi) = main_col_window(&sheet_rec, cursor);

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

    // ── Column layout with widths matching ratatui's grid.col_width() ──
    let mut layout: Vec<(u32, u32, String)> = Vec::new();
    let mut col_widths: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for &c in &col_ixs {
        let w = g.col_width(c).max(1);
        col_widths.insert(c, w);
        let label = crate::addr::ui_column_fragment(c, mc);
        layout.push((c as u32, w as u32, label));
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

    // ── Cell data for ALL visible rows and columns ────────────────────
    for (ri, &logical_row) in display_rows.iter().enumerate() {
        for &c in &col_ixs {
            // Determine the CellAddr for this visible cell
            let addr = if logical_row < hr {
                let hdr_row = (hr - 1 - logical_row) as u32;
                if c < lm {
                    CellAddr::Header { row: hdr_row, col: ColumnAddr::Left(c) }
                } else if c < lm + mc {
                    CellAddr::Header { row: hdr_row, col: ColumnAddr::Main((c - lm) as u32) }
                } else {
                    CellAddr::Header { row: hdr_row, col: ColumnAddr::Right(c - lm - mc) }
                }
            } else if logical_row < hr + mr {
                let main_row = (logical_row - hr) as u32;
                if c < lm {
                    CellAddr::Left { row: main_row, col: c }
                } else if c < lm + mc {
                    CellAddr::Main { row: main_row, col: (c - lm) as u32 }
                } else {
                    CellAddr::Right { row: main_row, col: c - lm - mc }
                }
            } else {
                let ftr_row = (logical_row - hr - mr) as u32;
                if c < lm {
                    CellAddr::Footer { row: ftr_row, col: ColumnAddr::Left(c) }
                } else if c < lm + mc {
                    CellAddr::Footer { row: ftr_row, col: ColumnAddr::Main((c - lm) as u32) }
                } else {
                    CellAddr::Footer { row: ftr_row, col: ColumnAddr::Right(c - lm - mc) }
                }
            };

            let cw = *col_widths.get(&c).unwrap_or(&4);

            // Widget column index in the cells map:
            // For main columns (c in [lm, lm+mc)): c - lm
            // For right-margin columns: c - lm = mc + right_idx
            // Left-margin columns are not stored (widget doesn't render them)
            let widget_col = c.saturating_sub(lm);

            let raw_opt = g.get(&addr);
            if let Some(ref raw) = raw_opt {
                // Only store cell content for main and right-margin columns
                if c >= lm {
                    // Store raw value in raw_cells for formula bar display
                    spreadsheet.set_raw_cell(ri as u32, widget_col as u32, raw);

                    // For display: use evaluated/formatted text
                    let effective = cell_effective_display(g, &addr);
                    let formatted = ui_core::format_cell_display(g, &addr, effective);
                    let fw = formatted.width();
                    let align = ui_core::effective_cell_align(g, &addr, &formatted);

                    // When text fits within the column width, pad it for
                    // proper alignment.  When it overflows, let the widget
                    // handle overflow/truncation (matching ratatui).
                    let display_text = if fw <= cw {
                        align_cell_display(formatted.to_string(), cw, align)
                    } else {
                        formatted.to_string()
                    };

                    spreadsheet.set_cell(ri as u32, widget_col as u32, &display_text);
                }
            } else if c >= lm && c < lm + mc {
                // Empty cell marker for main columns (stops overflow from previous column)
                spreadsheet.set_cell(ri as u32, widget_col as u32, "");
            }
        }
    }

    spreadsheet.set_column_layout(layout);
    spreadsheet.set_grid_config(lm as u32, mc as u32);

    // Store cursor cell raw value at (0, 0) for formula bar lookup
    {
        let cursor_main_row = cursor.row.saturating_sub(hr);
        let cursor_main_col = cursor.col.saturating_sub(lm);
        let cursor_addr = if cursor.row < hr {
            CellAddr::Header { row: cursor.row as u32, col: ColumnAddr::Main(cursor_main_col as u32) }
        } else if cursor.row < hr + mr {
            CellAddr::Main { row: cursor_main_row as u32, col: cursor_main_col as u32 }
        } else {
            CellAddr::Footer { row: (cursor.row - hr - mr) as u32, col: ColumnAddr::Main(cursor_main_col as u32) }
        };
        if let Some(raw_val) = g.get(&cursor_addr) {
            spreadsheet.set_raw_cell(0, 0, &raw_val);
        } else {
            spreadsheet.set_raw_cell(0, 0, "");
        }
    }

    // Tab bar (match ratatui format: " Sheet1    Sheet2    Sheet3    Sheet1 Copy ")
    if app.core.workbook.sheet_count() > 1 {
        let tabs: String = app.core.workbook.sheets.iter().enumerate()
            .flat_map(|(idx, sheet)| {
                let mut parts = Vec::new();
                if idx > 0 {
                    parts.push("  ".to_string());
                }
                parts.push(format!(" {} ", sheet.title));
                parts
            })
            .collect();
        spreadsheet.set_tab_text(&tabs);
    }

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
