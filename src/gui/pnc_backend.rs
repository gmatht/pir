use crate::agg::compute_aggregate;
use crate::agg::helpers::{data_main_col_count, left_margin_main_col_aggregate};
use crate::formula::cell_effective_display;
use crate::formula::effective_numeric;
use crate::grid::{CellAddr, ColumnAddr, GridBox, MainRange, NumberFormat, HEADER_ROWS, MARGIN_COLS};
use crate::ops::{margin_key_agg_func, AggFunc, AggregateDef};
use crate::ui_core::align_cell_display;
use crate::ui_core::{
    self, exponential_numeric_display_with_hint, main_col_window, take_display_prefix,
    truncate_with_ellipsis, would_ellipsis_hide_decimal_point,
};
use std::collections::HashMap;
use rustxwidgets::backends_pancurses_adapter::*;
use unicode_width::UnicodeWidthStr;

// ── Aggregate helpers (mirroring ratatui's visible_row_indices) ────────────

fn left_margin_agg(grid: &GridBox, main_row: u32) -> Option<AggFunc> {
    let key_col = MARGIN_COLS - 1;
    let val = grid.get(&CellAddr::Left { col: key_col, row: main_row })?;
    margin_key_agg_func(&val)
}

fn footer_agg(grid: &GridBox, footer_row: u32) -> Option<AggFunc> {
    let val = grid.get(&CellAddr::Footer {
        row: footer_row,
        col: ColumnAddr::Left(MARGIN_COLS - 1),
    })?;
    margin_key_agg_func(&val)
}

fn right_col_agg(grid: &GridBox, global_col: usize) -> Option<AggFunc> {
    let main_cols = grid.main_cols();
    let mut labels: Vec<(u32, String)> = grid
        .iter_nonempty()
        .filter_map(|(addr, val)| match addr {
            CellAddr::Header { row, col } if col.to_global(main_cols) == global_col => {
                Some((row, val))
            }
            _ => None,
        })
        .collect();
    labels.sort_unstable_by_key(|(row, _)| *row);
    for (_, val) in labels {
        if let Some(f) = margin_key_agg_func(&val) {
            return Some(f);
        }
    }
    None
}

/// Compute the total render width (col widths + separators) for a list of
/// visible column indices, matching ratatui's visible_cols_render_width.
fn visible_cols_render_width(g: &GridBox, cols: &[usize]) -> usize {
    let lm = MARGIN_COLS;
    let mc = g.main_cols();
    let show_right_divider = cols.contains(&(lm + mc));
    cols.iter()
        .enumerate()
        .map(|(i, &c)| {
            let sep = if i + 1 >= cols.len() {
                0
            } else if (c == lm - 1 && lm > 0 && cols.contains(&lm))
                || (c == lm + mc - 1 && show_right_divider)
            {
                2
            } else {
                1
            };
            g.col_width(c).max(1) + sep
        })
        .sum()
}

/// Trim trailing columns from `cols` until the total render width fits
/// within `width`, matching ratatui's trim_visible_cols_to_width.
fn trim_visible_cols_to_width(g: &GridBox, cols: &mut Vec<usize>, cursor_col: usize, width: usize) {
    while cols.len() > 1 && visible_cols_render_width(g, cols) > width {
        let first = cols.first().copied().unwrap_or(cursor_col);
        let last = cols.last().copied().unwrap_or(cursor_col);
        if last > cursor_col {
            cols.pop();
        } else if first < cursor_col {
            cols.remove(0);
        } else {
            break;
        }
    }
}

pub fn run_pancurses(app: &mut super::App) -> Result<(), Box<dyn std::error::Error>> {
    let _backend = rustxwidgets::backends::pancurses::init()
        .map_err(|e| format!("pancurses init failed: {e}"))?;

    let win = create_window()?;
    win.set_title("corro");

    // Tighten main columns against max_col_width (matching ratatui's
    // fit_column_to_rendered_content during load_initial). Without this,
    // auto_fit_column leaves columns wider than max_col_width in place.
    app.fit_main_columns_to_max_width();

    let mut sheet_rec = app.core.workbook.active_sheet().clone();
    let hr = HEADER_ROWS;
    let mr = sheet_rec.grid.main_rows();
    let mc = sheet_rec.grid.main_cols();
    let lm = MARGIN_COLS;
    let rm = MARGIN_COLS;
    let cursor = app.core.cursor;

    // ── Visible rows (matching ratatui's visible_row_indices) ──────────
    let mut header_rows: Vec<usize> = Vec::new();
    let mut footer_rows: Vec<usize> = Vec::new();
    for (addr, _) in sheet_rec.grid.iter_nonempty() {
        match addr {
            CellAddr::Header { row, .. } => header_rows.push(row as usize),
            CellAddr::Footer { row, .. } => footer_rows.push(hr + mr + row as usize),
            _ => {}
        }
    }
    let main_order = sheet_rec.grid.sorted_main_rows();
    header_rows.sort_unstable();
    header_rows.dedup();
    footer_rows.sort_unstable();
    footer_rows.dedup();
    // Fill remaining viewport space with blank footer rows to fill the
    // visible grid area (matching ratatui's visible_row_indices dim).
    let content_count = header_rows.len() + main_order.len() + footer_rows.len();
    let dim_rows = 44usize;
    let blank_needed = dim_rows.saturating_sub(content_count);
    if blank_needed > 0 {
        let base = footer_rows.last().copied().map(|r| r + 1).unwrap_or(hr + mr);
        for i in 0..blank_needed {
            footer_rows.push(base + i);
        }
    }
    footer_rows.sort_unstable();
    footer_rows.dedup();

    let mut display_rows: Vec<usize> =
        Vec::with_capacity(header_rows.len() + main_order.len() + footer_rows.len());
    display_rows.extend(header_rows.iter());
    display_rows.extend(main_order.iter().map(|r| hr + r));
    display_rows.extend(footer_rows.iter());

    // ── Available data width (matching ratatui's data_width) ──────────
    let term_cols = {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0
        {
            ws.ws_col as usize
        } else {
            std::env::var("COLUMNS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(80)
        }
    };
    let data_width = term_cols
        .saturating_sub(2)
        .saturating_sub(ui_core::ROW_LABEL_CHARS)
        .max(1);

    // ── Visible columns (matching ratatui's visible_col_indices) ──────
    let right_start = lm + mc;
    let (_main_lo, main_hi) = main_col_window(&sheet_rec, cursor);

    let mut col_ixs: Vec<usize> = Vec::new();
    if lm > 0 {
        col_ixs.push(lm - 1);
    }
    col_ixs.extend((0..=main_hi as usize).map(|ci| lm + ci));
    // Include right-margin columns with content (matching ratatui's
    // right_nonblank_end approach).
    if let Some(end) = ui_core::right_nonblank_end(&sheet_rec) {
        for i in 0..=end {
            let gc = right_start + i;
            if !col_ixs.contains(&gc) {
                col_ixs.push(gc);
            }
        }
    }
    // Fill remaining viewport space with blank right-margin columns
    // (matching ratatui's fill-until-dim approach).
    let total_so_far = col_ixs.len();
    let dim = data_width.checked_div(2).unwrap_or(1).max(1);
    let blank_cols_needed = dim.saturating_sub(total_so_far).max(1);
    for i in 0..blank_cols_needed.min(rm) {
        let gc = right_start + i;
        if !col_ixs.contains(&gc) {
            col_ixs.push(gc);
        }
    }
    col_ixs.sort_unstable();
    col_ixs.dedup();

    // Trim columns to fit data_width (matching ratatui draw() order;
    // ratatui's draw() does NOT call fit_visible_columns_capped).
    trim_visible_cols_to_width(&sheet_rec.grid, &mut col_ixs, cursor.col, data_width);

    // ── Column layout with widths matching ratatui's grid.col_width() ──
    let g = &sheet_rec.grid;
    let mut layout: Vec<(u32, u32, String)> = Vec::new();
    let mut col_widths: HashMap<usize, usize> = HashMap::new();
    for &c in &col_ixs {
        let w = g.col_width(c).max(1);
        col_widths.insert(c, w);
        let label = crate::addr::ui_column_fragment(c, mc);
        layout.push((c as u32, w as u32, label));
    }

    // ── Precompute aggregate info for each visible row ────────────────
    // For each display row index, determine if it's a left-margin aggregate
    // row or a footer aggregate row (the row_index is the main-relative or
    // footer-relative index).
    let mut row_agg_func: Vec<Option<AggFunc>> = Vec::with_capacity(display_rows.len());
    for &lr in &display_rows {
        let func = if lr < hr {
            None
        } else if lr < hr + mr {
            left_margin_agg(g, (lr - hr) as u32)
        } else {
            footer_agg(g, (lr - hr - mr) as u32)
        };
        row_agg_func.push(func);
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
        let main_row = if logical_row >= hr && logical_row < hr + mr {
            Some((logical_row - hr) as u32)
        } else {
            None
        };
        let footer_row_idx = if logical_row >= hr + mr {
            Some((logical_row - hr - mr) as u32)
        } else {
            None
        };
        let row_agg = row_agg_func[ri];

        let mut col_ix = 0usize;
        while col_ix < col_ixs.len() {
            let c = col_ixs[col_ix];
            // Determine the CellAddr for this visible cell
            let addr = if logical_row < hr {
                let hdr_row = logical_row as u32;
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
            // For main columns (c in [lm, lm+mc)): c - lm (the main column index)
            // For right-margin columns: c - lm = mc + right_idx
            // For left-margin columns: not used; stored by global col index instead.
            let widget_col = c.saturating_sub(lm);

            // ── Aggregate computation ──────────────────────────────────
            // Determine if this cell should show an aggregate result.
            let rca = if c >= lm && c < lm + mc {
                right_col_agg(g, c)
            } else {
                None
            };

            let effective = if let Some(func) = row_agg {
                if let Some(ftr_row) = footer_row_idx {
                    // ── Footer aggregate ──────────────────────────────
                    if c >= lm && c < lm + mc {
                        let main_col = (c - lm) as u32;
                        compute_aggregate(
                            g,
                            &AggregateDef {
                                func,
                                source: MainRange {
                                    row_start: 0,
                                    row_end: mr as u32,
                                    col_start: main_col,
                                    col_end: main_col + 1,
                                },
                            },
                        )
                    } else {
                        cell_effective_display(g, &addr)
                    }
                } else if let Some(mri) = main_row {
                    // ── Left-margin aggregate ─────────────────────────
                    if c >= lm && c < lm + mc {
                        if rca.is_some() {
                            let data_cols = data_main_col_count(g);
                            left_margin_main_col_aggregate(g, func, mri, (c - lm) as u32)
                        } else {
                            let main_col = (c - lm) as u32;
                            left_margin_main_col_aggregate(g, func, mri, main_col)
                        }
                    } else {
                        cell_effective_display(g, &addr)
                    }
                } else {
                    cell_effective_display(g, &addr)
                }
            } else if let (Some(mri), Some(agg_func)) = (main_row, rca) {
                // ── Plain main row with right-col aggregate ──────────
                if c >= lm && c < lm + mc {
                    let data_cols = data_main_col_count(g);
                    compute_aggregate(
                        g,
                        &AggregateDef {
                            func: agg_func,
                            source: MainRange {
                                row_start: mri,
                                row_end: mri + 1,
                                col_start: 0,
                                col_end: data_cols as u32,
                            },
                        },
                    )
                } else {
                    cell_effective_display(g, &addr)
                }
            } else {
                cell_effective_display(g, &addr)
            };

            // ── Format and store ──────────────────────────────────────
            let formatted = ui_core::format_cell_display(g, &addr, effective);
            let fw = formatted.width();
            let align = ui_core::effective_cell_align(g, &addr, &formatted);
            let is_agg_cell = row_agg.is_some() || rca.is_some();

            let allow_spill = fw > cw
                && (align.is_none() || align == Some(crate::grid::TextAlign::Left))
                && !is_agg_cell;

            // ── Spill rendering ──────────────────────────────────────────────
            // Matching ratatui: include the inter-column gap (1 char) in the
            // available width so text flows continuously without visible gaps
            // between columns.  The pancurses layout always uses a 1-char gap
            // between adjacent columns.
            let gap_width = if col_ix + 1 < col_ixs.len() { 1 } else { 0 };

            let mut did_spill = false;
            if allow_spill {
                let mut next_ix = col_ix + 1;
                let mut total_spill = cw;
                let mut total_spill_gaps = cw + gap_width;
                while next_ix < col_ixs.len() {
                    let c_next = col_ixs[next_ix];
                    let next_addr = if logical_row < hr {
                        let hdr_row = logical_row as u32;
                        if c_next < lm {
                            CellAddr::Header { row: hdr_row, col: ColumnAddr::Left(c_next) }
                        } else if c_next < lm + mc {
                            CellAddr::Header { row: hdr_row, col: ColumnAddr::Main((c_next - lm) as u32) }
                        } else {
                            CellAddr::Header { row: hdr_row, col: ColumnAddr::Right(c_next - lm - mc) }
                        }
                    } else if logical_row < hr + mr {
                        let main_row = (logical_row - hr) as u32;
                        if c_next < lm {
                            CellAddr::Left { row: main_row, col: c_next }
                        } else if c_next < lm + mc {
                            CellAddr::Main { row: main_row, col: (c_next - lm) as u32 }
                        } else {
                            CellAddr::Right { row: main_row, col: c_next - lm - mc }
                        }
                    } else {
                        let ftr_row = (logical_row - hr - mr) as u32;
                        if c_next < lm {
                            CellAddr::Footer { row: ftr_row, col: ColumnAddr::Left(c_next) }
                        } else if c_next < lm + mc {
                            CellAddr::Footer { row: ftr_row, col: ColumnAddr::Main((c_next - lm) as u32) }
                        } else {
                            CellAddr::Footer { row: ftr_row, col: ColumnAddr::Right(c_next - lm - mc) }
                        }
                    };
                    if !cell_effective_display(g, &next_addr).trim().is_empty() {
                        break;
                    }
                    let cw_next = *col_widths.get(&c_next).unwrap_or(&4);
                    total_spill += cw_next;
                    total_spill_gaps += cw_next;
                    // Add 1-char gap after this empty column
                    if next_ix + 1 < col_ixs.len() {
                        total_spill_gaps += 1;
                    }
                    next_ix += 1;
                }
                if total_spill_gaps > cw && next_ix > col_ix {
                    did_spill = true;
                    let (pre_total, _suf_total) = take_display_prefix(&formatted, total_spill_gaps);
                    // Store the full overflowing text in the FIRST column of the
                    // spill range.  Leave subsequent columns empty so the pancurses
                    // renderer's overflow logic draws the text across columns.
                    let store_text = if pre_total.trim().is_empty() {
                        String::new()
                    } else {
                        align_cell_display(pre_total, total_spill_gaps, align)
                    };
                    if !store_text.is_empty() || (c >= lm && c < lm + mc) {
                        // Store by global column index so margin and main cells
                        // don't collide at (row, 0)/(row, 1).
                        spreadsheet.set_cell(ri as u32, c as u32, &store_text);
                    }
                    col_ix = next_ix;
                }
            }

            if !did_spill {
                let display_text = if formatted.is_empty() {
                    String::new()
                } else if fw > cw {
                    let cell_fmt = g.format_for_addr(&addr);
                    let rational_hint = if matches!(cell_fmt.number, None | Some(NumberFormat::Rational | NumberFormat::DecimalGeneric))
                        && would_ellipsis_hide_decimal_point(&formatted, cw)
                    {
                        effective_numeric(g, &addr, &mut Vec::new(), &mut 10_000usize)
                            .map(|n| n.to_f64())
                            .filter(|v| v.is_finite())
                    } else {
                        None
                    };
                    let exp_preferred = if would_ellipsis_hide_decimal_point(&formatted, cw) {
                        exponential_numeric_display_with_hint(&formatted, cw, rational_hint)
                    } else {
                        None
                    };
                    let inner = exp_preferred
                        .or_else(|| ui_core::shrink_numeric_display(&formatted, cw))
                        .or_else(|| ui_core::exponential_numeric_display(&formatted, cw))
                        .unwrap_or_else(|| ui_core::truncate_with_ellipsis(&formatted, cw));
                    align_cell_display(inner, cw, align)
                } else {
                    align_cell_display(formatted.to_string(), cw, align)
                };

                let store_text = if display_text.trim().is_empty() {
                    String::new()
                } else {
                    display_text
                };
                if !store_text.is_empty() || (c >= lm && c < lm + mc) {
                    // Store by global column index to avoid margin/main collision.
                    spreadsheet.set_cell(ri as u32, c as u32, &store_text);
                }
                col_ix += 1;
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
