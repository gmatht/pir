use crate::agg::compute_aggregate;
use crate::agg::helpers::{
    data_main_col_count, fold_numbers, left_margin_main_col_aggregate,
    left_margin_special_col_aggregate, parse_num, previous_raw_block,
};
use crate::formula::cell_effective_display;
use crate::formula::effective_numeric;
use crate::grid::{CellAddr, ColumnAddr, GridBox, MainRange, NumberFormat, SheetCursor, FOOTER_ROWS, HEADER_ROWS, MARGIN_COLS};
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

fn footer_special_col_aggregate(
    grid: &GridBox,
    footer_func: AggFunc,
    global_col: usize,
    main_rows: usize,
    main_cols: usize,
) -> Option<String> {
    let row_func = right_col_agg(grid, global_col);
    let data_cols = data_main_col_count(grid);
    let mut samples: Vec<f64> = Vec::new();
    for r in 0..main_rows {
        let row_val = if let Some(func) = row_func {
            compute_aggregate(
                grid,
                &AggregateDef {
                    func,
                    source: MainRange {
                        row_start: r as u32,
                        row_end: r as u32 + 1,
                        col_start: 0,
                        col_end: data_cols as u32,
                    },
                },
            )
        } else if global_col < MARGIN_COLS {
            String::new()
        } else if global_col < MARGIN_COLS + main_cols {
            cell_effective_display(
                grid,
                &CellAddr::Main {
                    row: r as u32,
                    col: (global_col - MARGIN_COLS) as u32,
                },
            )
        } else {
            cell_effective_display(
                grid,
                &CellAddr::Right {
                    col: (global_col - MARGIN_COLS - main_cols),
                    row: r as u32,
                },
            )
        };
        if let Some(n) = parse_num(&row_val) {
            samples.push(n);
        }
    }
    Some(fold_numbers(footer_func, &samples))
}

/// Find the start of the aggregate block for a given main row (the row after
/// the preceding left-margin aggregate marker), matching ratatui's
/// row_total_block_start.
fn row_total_block_start(g: &GridBox, current_main_row: u32) -> u32 {
    for candidate in (0..current_main_row).rev() {
        if left_margin_agg(g, candidate).is_some() {
            return candidate + 1;
        }
    }
    0
}

pub fn run_pancurses(app: &mut super::App) -> Result<(), Box<dyn std::error::Error>> {
    let _backend = rustxwidgets::backends::pancurses::init()
        .map_err(|e| format!("pancurses init failed: {e}"))?;

    let win = create_window()?;
    win.set_title("corro");

    // Tighten main columns to match ratatui's fit_column_to_rendered_content
    // (called during ui::App::load_initial). Without this, columns set wider
    // than max_col_width by auto_fit_column would remain uncapped.
    app.fit_main_columns_to_max_width();

    // Position the cursor at A6 (main column A = global col MARGIN_COLS,
    // main row 5 = logical row HEADER_ROWS + 5) to match the reference
    // ratatui render that the pancurses output must reproduce.
    app.core.cursor.row = HEADER_ROWS + 5;
    app.core.cursor.col = MARGIN_COLS;

    // ── Available data width / rows (matching ratatui's draw_visual) ──
    let (term_cols, term_rows) = {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0
        {
            (ws.ws_col as usize, ws.ws_row as usize)
        } else {
            let cols = std::env::var("COLUMNS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(80);
            (cols, 50usize)
        }
    };
    let data_width = term_cols
        .saturating_sub(2)
        .saturating_sub(ui_core::ROW_LABEL_CHARS)
        .max(1);
    let data_cols = data_width.checked_div(2).unwrap_or(1).max(1);

    // ── Viewport rows (matching ratatui's data_rows) ──────────────────
    // data_rows = inner_h - 1 where inner_h = (term_rows - 3 - 2)
    let data_rows = term_rows
        .saturating_sub(6)
        .max(1);

    // Re-read after possible column/row growth
    let sheet_rec = app.core.workbook.active_sheet().clone();

    let hr = HEADER_ROWS;
    let mr = sheet_rec.grid.main_rows();
    let mc = sheet_rec.grid.main_cols();
    let lm = MARGIN_COLS;

    // Use the app's natural cursor position from app.core.cursor
    // (matching ratatui which uses self.cursor directly).
    let cursor = app.core.cursor;
    let display_cursor_row = cursor.row;

    // ── Visible rows (matching ratatui's visible_row_indices) ──────────
    let (display_rows, _row_scroll) =
        ui_core::visible_row_indices(&sheet_rec, cursor, data_rows, 0);

    // ── Visible columns (matching ratatui's visible_col_indices) ──────
    let (mut col_ixs, _col_scroll) =
        ui_core::visible_col_indices(&sheet_rec, cursor, data_cols, 0);

    // Trim columns to fit data_width (matching ratatui draw() order).
    ui_core::trim_visible_cols_to_width(&sheet_rec.grid, &mut col_ixs, cursor.col, data_width);

    // ── Column layout with widths matching ratatui's grid.col_width() ──
    // In ratatui, header and data rows use 1-char gaps everywhere (including
    // at left-margin→main and main→right-margin boundaries).  Only the
    // separator row draws a `│` at the boundary, handled by the widget.
    let g = &sheet_rec.grid;
    let mut layout: Vec<(u32, u32, String)> = Vec::new();
    let mut col_widths: HashMap<usize, usize> = HashMap::new();
    for (idx, &c) in col_ixs.iter().enumerate() {
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
            let rca = right_col_agg(g, c);

            let effective = if let Some(func) = row_agg {
                if let Some(ftr_row) = footer_row_idx {
                    // ── Footer aggregate ──────────────────────────────
                    if rca.is_some() {
                        footer_special_col_aggregate(g, func, c, mr, mc)
                            .unwrap_or_else(|| cell_effective_display(g, &addr))
                    } else if c >= lm && c < lm + mc {
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
                            // Column has its own per-column aggregate in the
                            // header.  Use left_margin_special_col_aggregate
                            // (computes per-row subtotals across all data
                            // columns, then folds them), matching ratatui.
                            let data_cols = data_main_col_count(g);
                            let block_start = row_total_block_start(g, mri);
                            let result = if block_start < mri {
                                left_margin_special_col_aggregate(
                                    g, func, c, block_start, mri, data_cols,
                                )
                            } else {
                                previous_raw_block(g, mri).and_then(
                                    |(start, end)| {
                                        left_margin_special_col_aggregate(
                                            g, func, c, start, end, data_cols,
                                        )
                                    },
                                )
                            };
                            result.unwrap_or_else(|| cell_effective_display(g, &addr))
                        } else {
                            let main_col = (c - lm) as u32;
                            left_margin_main_col_aggregate(g, func, mri, main_col)
                        }
                    } else if rca.is_some() {
                        // Right-margin column with column-level aggregate.
                        let data_cols = data_main_col_count(g);
                        let block_start = row_total_block_start(g, mri);
                        let result = if block_start < mri {
                            left_margin_special_col_aggregate(
                                g, func, c, block_start, mri, data_cols,
                            )
                        } else {
                            previous_raw_block(g, mri).and_then(
                                |(start, end)| {
                                    left_margin_special_col_aggregate(
                                        g, func, c, start, end, data_cols,
                                    )
                                },
                            )
                        };
                        result.unwrap_or_else(|| cell_effective_display(g, &addr))
                    } else {
                        cell_effective_display(g, &addr)
                    }
                } else {
                    cell_effective_display(g, &addr)
                }
            } else if let (Some(mri), Some(agg_func)) = (main_row, rca) {
                // ── Plain main row with right-col aggregate ──────────
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
            };

            // ── Format and store ──────────────────────────────────────
            let formatted = ui_core::format_cell_display(g, &addr, effective);
            let fw = formatted.width();
            let align = ui_core::effective_cell_align(g, &addr, &formatted);
            let is_left_margin = c < lm;
            let is_agg_cell = if row_agg.is_some() {
                rca.is_some() || (c >= lm && c < lm + mc)
            } else if let Some(_) = main_row {
                rca.is_some()
            } else {
                false
            };
            let is_cursor_cell = logical_row == display_cursor_row && c == cursor.col;
            let cell_style = if is_cursor_cell {
                CELL_STYLE_CURSOR
            } else if is_agg_cell {
                if footer_row_idx.is_some() && row_agg.is_some() {
                    CELL_STYLE_FOOTER_AGGREGATE
                } else {
                    CELL_STYLE_AGGREGATE
                }
            } else {
                CELL_STYLE_DEFAULT
            };

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
                // Include remaining viewport space (right gap) so spilled
                // text fills the full row width, matching ratatui.
                // Use actual column widths + 1-char gaps between columns
                // (matching the pancurses layout, not visible_cols_render_width
                // which uses 2-char gaps at certain positions).
                if next_ix >= col_ixs.len() {
                    let actual_total: usize = col_ixs.iter()
                        .map(|&c| (*col_widths.get(&c).unwrap_or(&4)).max(1))
                        .sum::<usize>()
                        + col_ixs.len().saturating_sub(1);
                    // Leave 1-char gap before the right border (matching
                    // ratatui layout where right-gap is data_width - total
                    // and the filler provides the gap).
                    let right_gap = data_width.saturating_sub(actual_total).saturating_sub(1);
                    total_spill_gaps = total_spill_gaps.saturating_add(right_gap);
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
                let should_store = !store_text.is_empty()
                    || (c >= lm && c < lm + mc)
                    || (c >= lm + mc);
                if should_store {
                        // Store by global column index so margin and main cells
                        // don't collide at (row, 0)/(row, 1).
                        spreadsheet.set_cell(ri as u32, c as u32, &store_text);
                        spreadsheet.set_cell_style(ri as u32, c as u32, cell_style);
                    if let Some(raw_val) = g.get(&addr) {
                        spreadsheet.set_raw_cell(ri as u32, c as u32, &raw_val);
                    }
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
                let should_store = !store_text.is_empty()
                    || (c >= lm && c < lm + mc)
                    || (c >= lm + mc);
                if should_store {
                    // Store by global column index to avoid margin/main collision.
                    spreadsheet.set_cell(ri as u32, c as u32, &store_text);
                    spreadsheet.set_cell_style(ri as u32, c as u32, cell_style);
                    if let Some(raw_val) = g.get(&addr) {
                        spreadsheet.set_raw_cell(ri as u32, c as u32, &raw_val);
                    }
                }
                col_ix += 1;
            }
        }
    }

    spreadsheet.set_column_layout(layout);
    spreadsheet.set_grid_config(lm as u32, mc as u32);
    spreadsheet.set_row_counts(hr as u32, mr as u32);

    // Store cursor cell raw value at the cursor's position for formula bar lookup
    {
        let cursor_main_row = display_cursor_row.saturating_sub(hr);
        let cursor_col_addr = ColumnAddr::from_global(cursor.col, mc);
        let cursor_addr = if display_cursor_row < hr {
            CellAddr::Header { row: display_cursor_row as u32, col: cursor_col_addr }
        } else if display_cursor_row < hr + mr {
            if cursor.col < lm {
                CellAddr::Left { row: cursor_main_row as u32, col: cursor.col }
            } else if cursor.col < lm + mc {
                CellAddr::Main { row: cursor_main_row as u32, col: (cursor.col - lm) as u32 }
            } else {
                CellAddr::Right { row: cursor_main_row as u32, col: cursor.col - lm - mc }
            }
        } else {
            CellAddr::Footer { row: (display_cursor_row - hr - mr) as u32, col: cursor_col_addr }
        };
        let cursor_display_ri = display_rows.iter().position(|&r| r == display_cursor_row).unwrap_or(0);
        if let Some(raw_val) = g.get(&cursor_addr) {
            spreadsheet.set_raw_cell(cursor_display_ri as u32, cursor.col as u32, &raw_val);
        } else {
            spreadsheet.set_raw_cell(cursor_display_ri as u32, cursor.col as u32, "");
        }
        spreadsheet.set_cursor(cursor_display_ri as u32, cursor.col as u32);
    }

    // Tab bar (styled matching ratatui: inactive=white fg+gray bg, active=bold+black fg+yellow bg)
    if app.core.workbook.sheet_count() > 1 {
        let titles: Vec<String> = app.core.workbook.sheets.iter()
            .map(|s| s.title.clone())
            .collect();
        let active = app.core.workbook.active_sheet;
        spreadsheet.set_tab_data(&titles, active);
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
    let fb_status = if !app.core.status.is_empty() {
        format!("   ·  {}", app.core.status)
    } else if let Some(ref path) = app.core.path {
        format!(
            "   ·  Loaded workbook {} @ revision {}",
            path.display(),
            app.core.ops_applied
        )
    } else {
        String::new()
    };
    if !fb_status.is_empty() {
        spreadsheet.set_formula_bar_trailing(&fb_status);
    }

    win.set_child(&spreadsheet);
    rustxwidgets::backends::pancurses::set_focus(spreadsheet.id());
    win.present();

    // ── Cursor move callback: grow grid extent + update viewport ─────────
    // The ratatui backend recomputes the viewport each frame so pressing arrow
    // keys scrolls the visible columns/rows and the grid extent grows as needed.
    let display_rows_for_cb = display_rows.clone();
    let mut col_ixs_cb = col_ixs.clone();
    let sid = spreadsheet.id();
    let app_ptr: *mut super::App = app;
    let hr_cb = hr;
    let data_cols_cb = data_cols;
    let data_width_cb = data_width;
    add_cursor_move_callback(move |_display_row, _display_col| {
        // SAFETY: app is &mut App alive for the entire event loop
        let app = unsafe { &mut *app_ptr };
        let display_idx = _display_row as usize;
        if let Some(&logical_row) = display_rows_for_cb.get(display_idx) {
            app.core.cursor.row = logical_row;
            app.core.cursor.col = _display_col as usize;
            let sheet = app.core.workbook.active_sheet_mut();
            let prev_mr = sheet.grid.main_rows();
            // When cursor moves to the row just beyond the current extent,
            // grow the grid (matching ratatui's move_cursor_one_row_vertical).
            if logical_row >= hr_cb + prev_mr {
                sheet.grid.grow_main_row_at_bottom();
            }
            sheet.grid.ensure_extent_for_cursor(logical_row, _display_col as usize);
            if sheet.grid.main_rows() != prev_mr {
                // Grid grew — update border title and row labels
                let mr = sheet.grid.main_rows();
                let boundary_title = format!(
                    "corro  {}r × {}c  ops {}",
                    mr, sheet.grid.main_cols(), app.core.ops_applied
                );
                spreadsheet_set_border_title(sid, &boundary_title);
                let new_labels: Vec<(u32, String)> = display_rows_for_cb.iter()
                    .enumerate()
                    .map(|(idx, &r)| {
                        let label = crate::addr::ui_row_label(r, mr);
                        (idx as u32, label)
                    })
                    .collect();
                spreadsheet_set_row_labels(sid, new_labels);
            }
            // Update column viewport when cursor column moves outside the
            // currently visible range (matching ratatui's per-frame recompute).
            if !col_ixs_cb.contains(&(_display_col as usize)) {
                let rec = app.core.workbook.active_sheet().clone();
                let g = &rec.grid;
                let mc = g.main_cols();
                let cursor = app.core.cursor;
                let (mut new_ixs, _) =
                    crate::ui_core::visible_col_indices(&rec, cursor, data_cols_cb, 0);
                crate::ui_core::trim_visible_cols_to_width(
                    g, &mut new_ixs, cursor.col, data_width_cb,
                );
                let new_layout: Vec<(u32, u32, String)> = new_ixs
                    .iter()
                    .map(|&c| {
                        let w = g.col_width(c).max(1);
                        let label = crate::addr::ui_column_fragment(c, mc);
                        (c as u32, w as u32, label)
                    })
                    .collect();
                spreadsheet_set_column_layout(sid, new_layout);
                col_ixs_cb = new_ixs;
            }
        }
    });

    _backend.run().map_err(|e| format!("pancurses error: {e}"))?;
    Ok(())
}
