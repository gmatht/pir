use std::collections::HashMap;

use crate::grid::{CellAddr, ColumnAddr, GridBox, NumberFormat};
use crate::ops::AggFunc;
use crate::ui_core;
use unicode_width::UnicodeWidthStr;

use super::compute::{self, right_col_agg, CellDisplayStyle};

/// Abstract sink for cell data produced by `fill_cells`.
/// Each backend implements this to route data to its native rendering system.
pub trait CellSink {
    fn set_cell(&mut self, display_row: u32, display_col: u32, text: &str);
    fn set_cell_style(&mut self, display_row: u32, display_col: u32, style: CellDisplayStyle);
    fn set_raw_cell(&mut self, display_row: u32, display_col: u32, text: &str);
    fn set_cursor(&mut self, display_row: u32, display_col: u32);
}

/// Populate a CellSink with cell data for the given viewport.
/// Generic version shared by pancurses and canvas backends.
#[allow(clippy::too_many_arguments)]
pub fn fill_cells(
    sink: &mut dyn CellSink,
    display_rows: &[usize],
    col_ixs: &[usize],
    col_widths: &HashMap<usize, usize>,
    g: &GridBox,
    hr: usize,
    mr: usize,
    mc: usize,
    lm: usize,
    data_width: usize,
    display_cursor_row: usize,
    display_cursor_col: usize,
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
            let addr = cell_addr_for_coords(logical_row, hr, mr, c, lm, mc);

            let cw = g.col_width(c).max(1);

            let rca = right_col_agg(g, c);
            let is_cursor_cell = logical_row == display_cursor_row && c == display_cursor_col;

            let cell_info = compute::compute_cell_info(
                g, &addr, is_cursor_cell, row_agg, main_row, footer_row_idx,
                rca, c, lm, mc, mr,
            );

            let formatted = &cell_info.formatted;
            let fw = formatted.width();
            let align = ui_core::effective_cell_align(g, &addr, formatted);

            let allow_spill = fw > cw
                && (align.is_none() || align == Some(crate::grid::TextAlign::Left))
                && !cell_info.is_agg_cell;

            let mut did_spill = false;
            if allow_spill {
                let mut next_ix = col_ix + 1;
                let mut total_spill_gaps = cw;
                let mut narrow_spill = cw;
                let mut beyond_boundary = false;

                while next_ix < col_ixs.len() {
                    let c_next = col_ixs[next_ix];
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
                    if !cell_display_at(g, logical_row, hr, mr, c_next, lm, mc).trim().is_empty() {
                        break;
                    }
                    if c_next == lm {
                        let render_w = ui_core::visible_cols_render_width(g, col_ixs);
                        let right_gap = data_width.saturating_sub(render_w);
                        total_spill_gaps = total_spill_gaps.saturating_add(right_gap);
                        narrow_spill = narrow_spill.saturating_add(right_gap);
                        break;
                    }
                    if c_next == lm + mc {
                        beyond_boundary = true;
                        let cw_rm = *col_widths.get(&c_next).unwrap_or(&4);
                        total_spill_gaps += cw_rm;
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

                let use_wide = fw > narrow_spill;
                let pipe_gap = if !use_wide && beyond_boundary { 2 } else { 0 };
                let pad_spill = if use_wide { total_spill_gaps } else { narrow_spill.saturating_sub(pipe_gap) };

                if pad_spill > cw {
                    did_spill = true;
                    let store_text = if formatted.trim().is_empty() {
                        String::new()
                    } else {
                        formatted.clone()
                    };
                    let should_store = !store_text.is_empty()
                        || (c >= lm && c < lm + mc)
                        || (c >= lm + mc);
                    if should_store {
                        sink.set_cell(ri as u32, c as u32, &store_text);
                        sink.set_cell_style(ri as u32, c as u32, cell_info.style);
                        if let Some(ref raw_val) = cell_info.raw_value {
                            sink.set_raw_cell(ri as u32, c as u32, raw_val);
                        }
                    }

                    let advance_to = if use_wide && beyond_boundary {
                        col_ixs.len()
                    } else {
                        next_ix
                    };

                    for skip in (col_ix + 1)..advance_to {
                        let skip_col = col_ixs[skip];
                        sink.set_cell(ri as u32, skip_col as u32, "");
                        sink.set_cell_style(ri as u32, skip_col as u32, CellDisplayStyle::Default);
                    }
                    col_ix = advance_to;
                }
            }

            if !did_spill {
                let display_text = if fw > cw {
                    truncate_and_align(g, &addr, formatted, cw, align)
                } else {
                    ui_core::align_cell_display(formatted.to_string(), cw, align)
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
                    sink.set_cell(ri as u32, c as u32, &store_text);
                    sink.set_cell_style(ri as u32, c as u32, cell_info.style);
                    if let Some(ref raw_val) = cell_info.raw_value {
                        sink.set_raw_cell(ri as u32, c as u32, raw_val);
                    }
                }
                col_ix += 1;
            }
        }
    }
}

/// Truncate and align text to fit within a column width.
fn truncate_and_align(
    g: &GridBox,
    addr: &CellAddr,
    formatted: &str,
    cw: usize,
    align: Option<crate::grid::TextAlign>,
) -> String {
    let cell_fmt = g.format_for_addr(addr);
    let rational_hint = if matches!(cell_fmt.number, None | Some(NumberFormat::Rational | NumberFormat::DecimalGeneric))
        && ui_core::would_ellipsis_hide_decimal_point(formatted, cw)
    {
        crate::formula::effective_numeric(g, addr, &mut Vec::new(), &mut 10_000usize)
            .map(|n| n.to_f64())
            .filter(|v| v.is_finite())
    } else {
        None
    };
    let exp_preferred = if ui_core::would_ellipsis_hide_decimal_point(formatted, cw) {
        ui_core::exponential_numeric_display_with_hint(formatted, cw, rational_hint)
    } else {
        None
    };
    let inner = exp_preferred
        .or_else(|| ui_core::shrink_numeric_display(formatted, cw))
        .or_else(|| ui_core::exponential_numeric_display(formatted, cw))
        .unwrap_or_else(|| ui_core::truncate_with_ellipsis(formatted, cw));
    ui_core::align_cell_display(inner, cw, align)
}

/// Get the effective display value for a cell at a given (logical_row, global_col).
fn cell_display_at(g: &GridBox, logical_row: usize, hr: usize, mr: usize, c: usize, lm: usize, mc: usize) -> String {
    let addr = cell_addr_for_coords(logical_row, hr, mr, c, lm, mc);
    crate::formula::cell_effective_display(g, &addr)
}

/// Build a CellAddr from logical row/col coordinates.
fn cell_addr_for_coords(logical_row: usize, hr: usize, mr: usize, c: usize, lm: usize, mc: usize) -> CellAddr {
    if logical_row < hr {
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
    }
}
