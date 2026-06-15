use crate::agg::compute_aggregate;
use crate::agg::helpers::{
    data_main_col_count, fold_numbers, left_margin_main_col_aggregate,
    left_margin_special_col_aggregate, parse_num, previous_raw_block,
};
use crate::formula::cell_effective_display;
use crate::formula::effective_numeric;
use crate::grid::{CellAddr, ColumnAddr, GridBox, MainRange, NumberFormat, SheetCursor, HEADER_ROWS, MARGIN_COLS};
use crate::ops::{margin_key_agg_func, AggFunc, AggregateDef, Op, WorkbookOp};
use crate::ui_core::align_cell_display;
use crate::ui_core::{
    self, exponential_numeric_display_with_hint, take_display_prefix,
    truncate_with_ellipsis, would_ellipsis_hide_decimal_point,
};
use std::collections::HashMap;
use rustxwidgets::backends_pancurses_adapter::*;
use unicode_width::UnicodeWidthStr;

/// Populate the spreadsheet widget with cell data for the given viewport.
/// This is called from both the initial render and the cursor-move callback
/// whenever the viewport changes.
#[allow(clippy::too_many_arguments)]
fn fill_cells(
    spreadsheet: &Spreadsheet,
    display_rows: &[usize],
    col_ixs: &[usize],
    col_widths: &HashMap<usize, usize>,
    g: &GridBox,
    hr: usize, mr: usize, mc: usize, lm: usize,
    data_width: usize,
    display_cursor_row: usize, display_cursor_col: usize,
    row_agg_func: &[Option<AggFunc>],
) {
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

            let rca = right_col_agg(g, c);

            let effective = if let Some(func) = row_agg {
                if let Some(_ftr_row) = footer_row_idx {
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
                    if c >= lm && c < lm + mc {
                        if rca.is_some() {
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

            let formatted = ui_core::format_cell_display(g, &addr, effective);
            let fw = formatted.width();
            let align = ui_core::effective_cell_align(g, &addr, &formatted);
            let is_agg_cell = if row_agg.is_some() {
                rca.is_some() || (c >= lm && c < lm + mc)
            } else if let Some(_) = main_row {
                rca.is_some()
            } else {
                false
            };
            let is_cursor_cell = logical_row == display_cursor_row && c == display_cursor_col;
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

            let mut did_spill = false;
            if allow_spill {
                let mut next_ix = col_ix + 1;
                let mut total_spill_gaps = cw;
                // Compute two widths:
                //   narrow – up to and including the PipeAndSpace at main→right boundary
                //   wide   – all visible columns + right_gap
                let mut narrow_spill = cw;
                let mut beyond_boundary = false;
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
                    // Add trailing separator AFTER the previous column (matching ratatui)
                    let prev_vp = next_ix - 1;
                    let prev_col = col_ixs[prev_vp];
                    let trailing = ui_core::inter_column_trailing_after_data_cell(
                        prev_vp, prev_col, col_ixs, lm, mc, col_ixs.contains(&(lm + mc)),
                    );
                    match trailing {
                        ui_core::InterColumnTrailing::AsciiSpace => {
                            total_spill_gaps += 1;
                            if !beyond_boundary {
                                narrow_spill += 1;
                            }
                        }
                        ui_core::InterColumnTrailing::PipeAndSpace => {
                            total_spill_gaps += 2;
                            if !beyond_boundary {
                                narrow_spill += 2;
                            }
                        }
                        _ => {}
                    }
                    if !cell_effective_display(g, &next_addr).trim().is_empty() {
                        break;
                    }
                    // Left-margin boundary: stop, add remaining space.
                    if c_next == lm {
                        let render_w = ui_core::visible_cols_render_width(g, col_ixs);
                        let right_gap = data_width.saturating_sub(render_w);
                        total_spill_gaps = total_spill_gaps.saturating_add(right_gap);
                        narrow_spill = narrow_spill.saturating_add(right_gap);
                        break;
                    }
                    // Right-margin boundary: if text fits within main+pipe, stop here;
                    // otherwise continue into right-margin columns.
                    if c_next == lm + mc {
                        beyond_boundary = true;
                        // Add this boundary column's width too
                        let cw_rm = *col_widths.get(&c_next).unwrap_or(&4);
                        total_spill_gaps += cw_rm;
                        // Add trailing between boundary column and first right-margin col
                        // (matching ratatui, which adds this in the next loop iteration)
                        if next_ix + 1 < col_ixs.len() {
                            let t = ui_core::inter_column_trailing_after_data_cell(
                                next_ix, c_next, col_ixs,
                                lm, mc, col_ixs.contains(&(lm + mc)),
                            );
                            match t {
                                ui_core::InterColumnTrailing::AsciiSpace => total_spill_gaps += 1,
                                ui_core::InterColumnTrailing::PipeAndSpace => total_spill_gaps += 2,
                                _ => {}
                            }
                        }
                        // Continue wide calculation for remaining right-margin cols.
                        let mut wide_ix = next_ix + 1;
                        while wide_ix < col_ixs.len() {
                            let cw_more = *col_widths.get(&col_ixs[wide_ix]).unwrap_or(&4);
                            total_spill_gaps += cw_more;
                            if wide_ix + 1 < col_ixs.len() {
                                let t = ui_core::inter_column_trailing_after_data_cell(
                                    wide_ix, col_ixs[wide_ix], col_ixs,
                                    lm, mc, col_ixs.contains(&(lm + mc)),
                                );
                                match t {
                                    ui_core::InterColumnTrailing::AsciiSpace => total_spill_gaps += 1,
                                    ui_core::InterColumnTrailing::PipeAndSpace => total_spill_gaps += 2,
                                    _ => {}
                                }
                            }
                            wide_ix += 1;
                        }
                        let render_w = ui_core::visible_cols_render_width(g, col_ixs);
                        let right_gap = data_width.saturating_sub(render_w);
                        total_spill_gaps = total_spill_gaps.saturating_add(right_gap);
                        break;
                    }
                    let cw_next = *col_widths.get(&c_next).unwrap_or(&4);
                    total_spill_gaps += cw_next;
                    if !beyond_boundary {
                        narrow_spill += cw_next;
                    }
                    next_ix += 1;
                }
                if !beyond_boundary && next_ix >= col_ixs.len() {
                    let render_w = ui_core::visible_cols_render_width(g, col_ixs);
                    let right_gap = data_width.saturating_sub(render_w);
                    narrow_spill = narrow_spill.saturating_add(right_gap);
                    total_spill_gaps = narrow_spill;
                }
                // Use narrow_spill when text fits within it (preserves structural pipe).
                // Use total_spill_gaps (wide) when text overflows the boundary.
                // For narrow text the PipeAndSpace at the boundary is drawn by the
                // widget as a structural element, so exclude it from the text padding.
                let use_wide = fw > narrow_spill;
                let pipe_gap = if !use_wide && beyond_boundary { 2 } else { 0 };
                let pad_spill = if use_wide { total_spill_gaps } else { narrow_spill.saturating_sub(pipe_gap) };
                if pad_spill > cw {
                    did_spill = true;
                    // Store the full formatted text; the widget will handle
                    // overflow/truncation during rendering.  Truncating here
                    // with pad_spill can lose characters when the visible
                    // columns barely fit the render width.
                    let store_text = if formatted.trim().is_empty() {
                        String::new()
                    } else {
                        formatted.clone()
                    };
                let should_store = !store_text.is_empty()
                    || (c >= lm && c < lm + mc)
                    || (c >= lm + mc);
                if should_store {
                        spreadsheet.set_cell(ri as u32, c as u32, &store_text);
                        spreadsheet.set_cell_style(ri as u32, c as u32, cell_style);
                    if let Some(raw_val) = g.get(&addr) {
                        spreadsheet.set_raw_cell(ri as u32, c as u32, &raw_val);
                    }
                    }
                    // Determine how far to advance.  For wide text past the boundary,
                    // consume all right-margin columns.
                    let advance_to = if use_wide && beyond_boundary {
                        let mut adv = next_ix + 1;
                        while adv < col_ixs.len() {
                            adv += 1;
                        }
                        adv
                    } else {
                        next_ix
                    };
                    for skip in (col_ix + 1)..advance_to {
                        let skip_col = col_ixs[skip];
                        spreadsheet.set_cell(ri as u32, skip_col as u32, "");
                        spreadsheet.set_cell_style(ri as u32, skip_col as u32, CELL_STYLE_DEFAULT);
                    }
                    col_ix = advance_to;
                }
            }

            if !did_spill {
                let display_text = if formatted.is_empty() {
                    String::new()
                } else if fw > cw {
                    let store_width = cw;
                    let cell_fmt = g.format_for_addr(&addr);
                    let rational_hint = if matches!(cell_fmt.number, None | Some(NumberFormat::Rational | NumberFormat::DecimalGeneric))
                        && would_ellipsis_hide_decimal_point(&formatted, store_width)
                    {
                        effective_numeric(g, &addr, &mut Vec::new(), &mut 10_000usize)
                            .map(|n| n.to_f64())
                            .filter(|v| v.is_finite())
                    } else {
                        None
                    };
                    let exp_preferred = if would_ellipsis_hide_decimal_point(&formatted, store_width) {
                        exponential_numeric_display_with_hint(&formatted, store_width, rational_hint)
                    } else {
                        None
                    };
                    let inner = exp_preferred
                        .or_else(|| ui_core::shrink_numeric_display(&formatted, store_width))
                        .or_else(|| ui_core::exponential_numeric_display(&formatted, store_width))
                        .unwrap_or_else(|| ui_core::truncate_with_ellipsis(&formatted, store_width));
                    align_cell_display(inner, store_width, align)
                } else {
                    let aligned = align_cell_display(formatted.to_string(), cw, align);
                    // Ensure the text fills the full column width — the widget
                    // pads with trailing spaces, so pre-padded text is required
                    // for right-alignment and center-alignment to render correctly.
                    if aligned.chars().count() < cw && align == Some(crate::grid::TextAlign::Right) {
                        " ".repeat(cw.saturating_sub(aligned.chars().count())) + &aligned
                    } else if aligned.chars().count() < cw && align == Some(crate::grid::TextAlign::Center) {
                        let left = (cw - aligned.chars().count()) / 2;
                        let right = cw - aligned.chars().count() - left;
                        " ".repeat(left) + &aligned + &" ".repeat(right)
                    } else {
                        aligned
                    }
                };

                let store_text = if display_text.trim().is_empty() {
                    String::new()
                } else {
                    display_text
                };
                let should_store = true;
                if should_store {
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
}

/// Compute row aggregate info for each display row.
fn compute_row_agg_func(
    g: &GridBox,
    display_rows: &[usize],
    hr: usize,
    mr: usize,
) -> Vec<Option<AggFunc>> {
    let mut row_agg_func: Vec<Option<AggFunc>> = Vec::with_capacity(display_rows.len());
    for &lr in display_rows {
        let func = if lr < hr {
            None
        } else if lr < hr + mr {
            left_margin_agg(g, (lr - hr) as u32)
        } else {
            footer_agg(g, (lr - hr - mr) as u32)
        };
        row_agg_func.push(func);
    }
    row_agg_func
}

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

    // ── Available data width / rows (matching ratatui's draw_visual) ──
    // Use environment variables to allow test backends to control size.
    let (term_cols, term_rows) = {
        let env_cols = std::env::var("CORRO_TERM_COLS").ok().and_then(|s| s.parse().ok());
        let env_rows = std::env::var("CORRO_TERM_ROWS").ok().and_then(|s| s.parse().ok());
        if let (Some(c), Some(r)) = (env_cols, env_rows) {
            (c, r)
        } else {
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
        }
    };
    let data_width = term_cols
        .saturating_sub(2)
        .saturating_sub(ui_core::ROW_LABEL_CHARS)
        .max(1);
    let data_cols = data_width.checked_div(2).unwrap_or(1).max(1);

    // ── Viewport rows (matching ratatui's draw_visual) ──────────────────
    // The widget layout is: menu(1) + formula(1) + border(1) + header(1) +
    // separator(1) + data_rows + border_bottom(1) + status(1) = term_rows
    // Ratatui computes data_rows as inner_h - 1, where inner_h is the grid
    // block's inner height (term_rows - 2 for borders, minus menu(1) and
    // formula(1) and hints(1) = term_rows - 5 for inner, then -1 for
    // separator = term_rows - 6).
    let data_rows = term_rows
        .saturating_sub(6)
        .max(1);

    let sheet_rec = app.core.workbook.active_sheet().clone();

    // Keep the cursor at its natural initial position (first main row,
    // first main column = cell A1), matching the ratatui backend which
    // does not grow the grid or reposition the cursor on initial load.
    app.core.cursor.clamp(&sheet_rec.grid);

    let hr = HEADER_ROWS;
    let mr = sheet_rec.grid.main_rows();
    let mc = sheet_rec.grid.main_cols();
    let lm = MARGIN_COLS;

    // Position cursor at A1 (first main row, first main column) matching the
    // normal-mode reference output from the ratatui backend.
    if mr >= 1 {
        app.core.cursor.row = hr;
        app.core.cursor.col = MARGIN_COLS;
    }

    let display_cursor_row = app.core.cursor.row;
    let display_cursor_col = app.core.cursor.col;
    let cursor = SheetCursor {
        row: display_cursor_row,
        col: display_cursor_col,
    };

    // ── Visible rows (matching ratatui's visible_row_indices) ──────────
    let (display_rows, _row_scroll) =
        ui_core::visible_row_indices(&sheet_rec, cursor, data_rows, 0);

    // ── Visible columns (matching ratatui's visible_col_indices) ──────
    let g = &sheet_rec.grid;
    let (mut col_ixs, _col_scroll) =
        ui_core::visible_col_indices(&sheet_rec, cursor, data_cols, 0);
    ui_core::trim_visible_cols_to_width(g, &mut col_ixs, cursor.col, data_width);

    // ── Column layout with widths matching ratatui's grid.col_width() ──
    // In ratatui, header and data rows use 1-char gaps everywhere (including
    // at left-margin→main and main→right-margin boundaries).  Only the
    // separator row draws a `│` at the boundary, handled by the widget.
    let mut layout: Vec<(u32, u32, String)> = Vec::new();
    let mut col_widths: HashMap<usize, usize> = HashMap::new();
    for &c in col_ixs.iter() {
        let w = g.col_width(c).max(1);
        col_widths.insert(c, w);
        let label = crate::addr::ui_column_fragment(c, mc);
        layout.push((c as u32, w as u32, label));
    }

    // ── Precompute aggregate info for each visible row ────────────────
    let row_agg_func = compute_row_agg_func(g, &display_rows, hr, mr);

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
    fill_cells(&spreadsheet, &display_rows, &col_ixs, &col_widths,
        g, hr, mr, mc, lm, data_width,
        display_cursor_row, display_cursor_col, &row_agg_func);

    spreadsheet.set_column_layout(layout);
    spreadsheet.set_grid_config(lm as u32, mc as u32);
    spreadsheet.set_row_counts(hr as u32, mr as u32);

    // Store cursor cell raw value at the cursor's position for formula bar lookup
    {
        let cursor_main_row = display_cursor_row.saturating_sub(hr);
        let cursor_col_addr = ColumnAddr::from_global(display_cursor_col, mc);
        let cursor_addr = if display_cursor_row < hr {
            CellAddr::Header { row: display_cursor_row as u32, col: cursor_col_addr }
        } else if display_cursor_row < hr + mr {
            if display_cursor_col < lm {
                CellAddr::Left { row: cursor_main_row as u32, col: display_cursor_col }
            } else if display_cursor_col < lm + mc {
                CellAddr::Main { row: cursor_main_row as u32, col: (display_cursor_col - lm) as u32 }
            } else {
                CellAddr::Right { row: cursor_main_row as u32, col: display_cursor_col - lm - mc }
            }
        } else {
            CellAddr::Footer { row: (display_cursor_row - hr - mr) as u32, col: cursor_col_addr }
        };
        let cursor_display_ri = display_rows.iter().position(|&r| r == display_cursor_row).unwrap_or(0);
        let cursor_raw_val = g.get(&cursor_addr).unwrap_or_default();
        spreadsheet.set_raw_cell(cursor_display_ri as u32, display_cursor_col as u32, &cursor_raw_val);
        spreadsheet.set_cursor(cursor_display_ri as u32, display_cursor_col as u32);

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

    // Formula bar trailing: show app status text (matches ratatui's
    // mode_prompt_widget which appends "   ·  {status}" after the cell value).
    if !app.core.status.is_empty() {
        spreadsheet.set_formula_bar_trailing(&format!("   ·  {}", app.core.status));
    } else {
        spreadsheet.set_formula_bar_trailing("");
    }

    win.set_child(&spreadsheet);
    rustxwidgets::backends::pancurses::set_focus(spreadsheet.id());
    win.present();

    // ── Cursor move callback: grow grid extent + update viewport ─────────
    // The ratatui backend recomputes the viewport each frame so pressing arrow
    // keys scrolls the visible columns/rows and the grid extent grows as needed.
    let display_rows_for_cb = std::rc::Rc::new(std::cell::RefCell::new(display_rows.clone()));
    let display_rows_for_ce = display_rows_for_cb.clone();
    let mut col_ixs_cb = col_ixs.clone();
    let sheet_cb = spreadsheet.clone();
    let sid = spreadsheet.id();
    let app_ptr: *mut super::App = app;
    let hr_cb = hr;
    let hr_ce = hr;
    let mr_cb = mr;
    let lm_ce = lm;
    let data_rows_cb = data_rows;
    let data_cols_cb = data_cols;
    let data_width_cb = data_width;
    let mut prev_cursor_col = cursor.col;
    let mut prev_cursor_row = cursor.row;
    add_cursor_move_callback(move |_display_row, _display_col| {
        // SAFETY: app is &mut App alive for the entire event loop
        let app = unsafe { &mut *app_ptr };
        let display_idx = _display_row as usize;
        let mut need_viewport_recompute = false;

        // Handle sentinel values for scrolling past viewport boundaries
        if _display_row == u32::MAX {
            // Scroll up sentinel: user pressed Up at the first visible row
            if app.core.cursor.row > 0 {
                app.core.cursor.row -= 1;
            }
            need_viewport_recompute = true;
        } else if _display_row == u32::MAX - 1 {
            // Scroll down sentinel: user pressed Down at the last visible row.
            // Grow the grid if the cursor is at the last main row with few
            // trailing blanks (matching ratatui's move_cursor_one_row_vertical).
            let cursor_row = app.core.cursor.row;
            {
                let sheet = app.core.workbook.active_sheet_mut();
                let mr = sheet.grid.main_rows();
                if cursor_row == hr_cb + mr.saturating_sub(1)
                    && trailing_blank_main_rows(&sheet.grid) < crate::ui_core::NAV_BLANK_ROWS
                {
                    sheet.grid.grow_main_row_at_bottom();
                }
            }
            app.core.cursor.row += 1;
            need_viewport_recompute = true;
        } else if let Some(&logical_row) = display_rows_for_cb.borrow().get(display_idx) {
            app.core.cursor.row = logical_row;

            // Grow the grid if cursor moves from the last main row with few
            // trailing blanks (matching ratatui's move_cursor_one_row_vertical).
            if logical_row >= hr_cb + mr_cb {
                let sheet = app.core.workbook.active_sheet_mut();
                let cur_mr = sheet.grid.main_rows();
                if prev_cursor_row == hr_cb + cur_mr.saturating_sub(1)
                    && trailing_blank_main_rows(&sheet.grid) < crate::ui_core::NAV_BLANK_ROWS
                {
                    sheet.grid.grow_main_row_at_bottom();
                }
            }

            // Check if cursor moved into header or footer region; if so,
            // recompute the viewport so those rows become visible.
            if logical_row < hr_cb || logical_row >= hr_cb + mr_cb {
                need_viewport_recompute = true;
            }
        }
        app.core.cursor.col = _display_col as usize;

        if need_viewport_recompute {
            let rec = app.core.workbook.active_sheet().clone();
            let cursor = app.core.cursor;
            let (new_display_rows, _) =
                crate::ui_core::visible_row_indices(&rec, cursor, data_rows_cb, 0);
            let new_mr = rec.grid.main_rows();
            let new_mc = rec.grid.main_cols();
            // Update border title when grid grew (matching the non-recompute path)
            let boundary_title = format!(
                "corro  {}r × {}c  ops {}",
                new_mr, new_mc, app.core.ops_applied
            );
            spreadsheet_set_border_title(sid, &boundary_title);
            let new_labels: Vec<(u32, String)> = new_display_rows.iter()
                .enumerate()
                .map(|(idx, &r)| {
                    let label = crate::addr::ui_row_label(r, new_mr);
                    (idx as u32, label)
                })
                .collect();
            spreadsheet_set_row_labels(sid, new_labels);
            // Update column layout when main columns grew
            {
                let g = &rec.grid;
                let cur_cursor = app.core.cursor;
                let (mut new_ixs, _) =
                    crate::ui_core::visible_col_indices(&rec, cur_cursor, data_cols_cb, 0);
                crate::ui_core::trim_visible_cols_to_width(
                    g, &mut new_ixs, cur_cursor.col, data_width_cb,
                );
                let new_layout: Vec<(u32, u32, String)> = new_ixs
                    .iter()
                    .map(|&c| {
                        let w = g.col_width(c).max(1);
                        let label = crate::addr::ui_column_fragment(c, new_mc);
                        (c as u32, w as u32, label)
                    })
                    .collect();
                spreadsheet_set_column_layout(sid, new_layout);
                col_ixs_cb = new_ixs;
                spreadsheet_set_grid_config(sid, MARGIN_COLS as u32, new_mc as u32);
            }
            // Repopulate all visible cells for the new viewport
            let new_col_widths: HashMap<usize, usize> = col_ixs_cb.iter()
                .map(|&c| (c, rec.grid.col_width(c).max(1)))
                .collect();
            let new_row_agg = compute_row_agg_func(&rec.grid, &new_display_rows, hr_cb, new_mr);
            fill_cells(
                &sheet_cb, &new_display_rows, &col_ixs_cb, &new_col_widths,
                &rec.grid, hr_cb, new_mr, new_mc, MARGIN_COLS, data_width_cb,
                cursor.row, cursor.col, &new_row_agg,
            );
            if let Some(new_display_ri) = new_display_rows.iter().position(|&r| r == cursor.row) {
                let cursor_addr = crate::addr::sheet_cursor_to_addr(
                    crate::addr::LogicalRow(cursor.row),
                    crate::addr::GlobalCol(cursor.col),
                    crate::addr::MainRows(new_mr),
                    crate::addr::MainCols(new_mc),
                );
                if let Some(raw_val) = rec.grid.get(&cursor_addr) {
                    sheet_cb.set_raw_cell(new_display_ri as u32, cursor.col as u32, &raw_val);
                } else {
                    sheet_cb.set_raw_cell(new_display_ri as u32, cursor.col as u32, "");
                }
                sheet_cb.set_cursor(new_display_ri as u32, cursor.col as u32);
            }
            *display_rows_for_cb.borrow_mut() = new_display_rows;
            prev_cursor_row = app.core.cursor.row;
            prev_cursor_col = app.core.cursor.col;
            return;
        }

        if let Some(&logical_row) = display_rows_for_cb.borrow().get(display_idx) {
            let sheet = app.core.workbook.active_sheet_mut();
            let prev_mr = sheet.grid.main_rows();
            let prev_mc = sheet.grid.main_cols();
            let lm = MARGIN_COLS;

            // Expand main columns when moving right past the last main column
            // (matching ratatui's move_cursor_one_col_horizontal).
            let new_col = _display_col as usize;
            if new_col > prev_cursor_col {
                if prev_cursor_col == lm + prev_mc.saturating_sub(1)
                    && trailing_blank_main_cols(&sheet.grid) < crate::ui_core::NAV_BLANK_COLS
                {
                    sheet.grid.grow_main_col_at_right();
                }
            }

            // Expand main rows when moving down from the last main row
            // (matching ratatui's move_cursor_one_row_vertical).
            if logical_row > prev_cursor_row {
                if prev_cursor_row == hr_cb + prev_mr.saturating_sub(1)
                    && trailing_blank_main_rows(&sheet.grid) < crate::ui_core::NAV_BLANK_ROWS
                {
                    sheet.grid.grow_main_row_at_bottom();
                }
            }

            prev_cursor_col = new_col;
            prev_cursor_row = logical_row;

            // When cursor moves to the row just beyond the current extent,
            // grow the grid (matching ratatui's move_cursor_one_row_vertical)
            // for cases where the cursor jumps multiple rows at once.
            // Use the current main row count (after any growth above) to
            // avoid redundant growth: prev_mr may be stale if the earlier
            // row-growth condition at line ~753 already fired.
            let cur_mr = sheet.grid.main_rows();
            if logical_row >= hr_cb + cur_mr {
                sheet.grid.grow_main_row_at_bottom();
            }
            sheet.grid.ensure_extent_for_cursor(logical_row, _display_col as usize);
            if sheet.grid.main_rows() != prev_mr || sheet.grid.main_cols() != prev_mc {
                // Grid grew — update border title and row labels
                let mr = sheet.grid.main_rows();
                let mc = sheet.grid.main_cols();
                let boundary_title = format!(
                    "corro  {}r × {}c  ops {}",
                    mr, mc, app.core.ops_applied
                );
                spreadsheet_set_border_title(sid, &boundary_title);
                let new_labels: Vec<(u32, String)> = display_rows_for_cb.borrow().iter()
                    .enumerate()
                    .map(|(idx, &r)| {
                        let label = crate::addr::ui_row_label(r, mr);
                        (idx as u32, label)
                    })
                    .collect();
                spreadsheet_set_row_labels(sid, new_labels);
                // Also update column layout when main columns grew
                let rec = app.core.workbook.active_sheet().clone();
                let g = &rec.grid;
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
                // Keep the widget's margin_cols and main_cols in sync
                spreadsheet_set_grid_config(sid, lm as u32, mc as u32);
                // Repopulate cells with updated column layout after growth
                let dr: Vec<usize> = display_rows_for_cb.borrow().clone();
                let new_col_widths: HashMap<usize, usize> = col_ixs_cb.iter()
                    .map(|&c| (c, g.col_width(c).max(1)))
                    .collect();
                let new_row_agg = compute_row_agg_func(g, &dr, hr_cb, mr);
                fill_cells(
                    &sheet_cb, &dr, &col_ixs_cb, &new_col_widths,
                    g, hr_cb, mr, mc, MARGIN_COLS, data_width_cb,
                    cursor.row, cursor.col, &new_row_agg,
                );
            } else if !col_ixs_cb.contains(&(_display_col as usize)) {
                // Update column viewport when cursor column moves outside the
                // currently visible range (matching ratatui's per-frame recompute).
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
                // Repopulate cells with updated column viewport
                let dr: Vec<usize> = display_rows_for_cb.borrow().clone();
                let new_col_widths: HashMap<usize, usize> = col_ixs_cb.iter()
                    .map(|&c| (c, g.col_width(c).max(1)))
                    .collect();
                let new_row_agg = compute_row_agg_func(g, &dr, hr_cb, mc);
                fill_cells(
                    &sheet_cb, &dr, &col_ixs_cb, &new_col_widths,
                    g, hr_cb, g.main_rows(), mc, MARGIN_COLS, data_width_cb,
                    cursor.row, cursor.col, &new_row_agg,
                );
            } else {
                // Cursor moved within the current viewport — refresh cells to ensure
                // formatted display values are used (commits overwrite cells with raw values).
                let rec = app.core.workbook.active_sheet().clone();
                let g = &rec.grid;
                let mc = g.main_cols();
                let cursor = app.core.cursor;
                let dr: Vec<usize> = display_rows_for_cb.borrow().clone();
                let new_col_widths: HashMap<usize, usize> = col_ixs_cb.iter()
                    .map(|&c| (c, g.col_width(c).max(1)))
                    .collect();
                let new_row_agg = compute_row_agg_func(g, &dr, hr_cb, g.main_rows());
                fill_cells(
                    &sheet_cb, &dr, &col_ixs_cb, &new_col_widths,
                    g, hr_cb, g.main_rows(), mc, MARGIN_COLS, data_width_cb,
                    cursor.row, cursor.col, &new_row_agg,
                );
            }
        }
    });

    // ── Commit edit callback: persist cell edits to workbook ──────────
    let app_ptr_ce = app_ptr;
    add_commit_edit_callback(move |display_row, col, value| {
        let app = unsafe { &mut *app_ptr_ce };
        let dr = display_rows_for_ce.borrow();
        let logical_row = dr.get(display_row as usize).copied().unwrap_or(0);
        let main_row = logical_row.saturating_sub(hr_ce);
        let main_col = col.saturating_sub(lm_ce as u32);
        let addr = CellAddr::Main { row: main_row as u32, col: main_col as u32 };
        let sheet_id = app.core.workbook.sheet_id(app.core.workbook.active_sheet);
        let op = Op::SetCell { addr, value };
        let wbo = WorkbookOp::SheetOp { sheet_id, op };
        if let Some(ref p) = app.core.path.clone() {
            let mut active_sheet = sheet_id;
            let _ = crate::io::commit_workbook_op(
                p,
                &mut app.core.offset,
                &mut app.core.workbook,
                &mut active_sheet,
                &wbo,
            );
            app.core.ops_applied = app.core.ops_applied.saturating_add(1);
        }
    });

    _backend.run().map_err(|e| format!("pancurses error: {e}"))?;
    Ok(())
}

/// Count trailing blank main columns (matching ratatui's trailing_blank_main_cols).
fn trailing_blank_main_cols(grid: &crate::grid::GridBox) -> usize {
    let lm = crate::grid::MARGIN_COLS;
    let mc = grid.main_cols();
    match (0..mc).rev().find(|&c| grid.logical_col_has_content(lm + c)) {
        None => mc,
        Some(last) => mc.saturating_sub(last + 1),
    }
}

/// Count trailing blank main rows (matching ratatui's trailing_blank_main_rows).
fn trailing_blank_main_rows(grid: &crate::grid::GridBox) -> usize {
    let hr = crate::grid::HEADER_ROWS;
    let mr = grid.main_rows();
    match (0..mr).rev().find(|&r| grid.logical_row_has_content(hr + r)) {
        None => mr,
        Some(last) => mr.saturating_sub(last + 1),
    }
}
