use crate::agg::compute_aggregate;
use crate::agg::helpers::{
    data_main_col_count, fold_numbers, left_margin_main_col_aggregate,
    left_margin_special_col_aggregate, parse_num, previous_raw_block,
};
use crate::formula::cell_effective_display;
use crate::grid::{CellAddr, ColumnAddr, GridBox, MainRange, HEADER_ROWS, MARGIN_COLS};
use crate::ops::{margin_key_agg_func, AggFunc, AggregateDef};

/// Compute row aggregate info for each display row.
pub fn compute_row_agg_func(
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

/// Check if the left-margin key column for the given main row has an aggregate marker.
fn left_margin_agg(grid: &GridBox, main_row: u32) -> Option<AggFunc> {
    let key_col = MARGIN_COLS - 1;
    let val = grid.get(&CellAddr::Left { col: key_col, row: main_row })?;
    margin_key_agg_func(&val)
}

/// Check if the footer row has an aggregate marker.
fn footer_agg(grid: &GridBox, footer_row: u32) -> Option<AggFunc> {
    let val = grid.get(&CellAddr::Footer {
        row: footer_row,
        col: ColumnAddr::Left(MARGIN_COLS - 1),
    })?;
    margin_key_agg_func(&val)
}

/// Check if the header for a given global column has an aggregate marker (right-col agg).
pub fn right_col_agg(grid: &GridBox, global_col: usize) -> Option<AggFunc> {
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

/// Compute a footer aggregate value across all main rows for a given column.
pub fn footer_special_col_aggregate(
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
pub fn row_total_block_start(g: &GridBox, current_main_row: u32) -> u32 {
    for candidate in (0..current_main_row).rev() {
        if left_margin_agg(g, candidate).is_some() {
            return candidate + 1;
        }
    }
    0
}

/// Check if the header at `HEADER_ROWS - 1` for the given main column has content
/// (matching ratatui's `header_template_applies`).
pub fn header_template_applies(grid: &GridBox, main_col: usize) -> bool {
    grid.get(&CellAddr::Header {
        row: (HEADER_ROWS - 1) as u32,
        col: ColumnAddr::Main(main_col as u32),
    })
    .as_deref()
    .is_some()
}

/// Count trailing blank main columns (matching ratatui's trailing_blank_main_cols).
pub fn trailing_blank_main_cols(grid: &GridBox) -> usize {
    let lm = MARGIN_COLS;
    let mc = grid.main_cols();
    match (0..mc).rev().find(|&c| {
        grid.logical_col_has_content(lm + c)
            || header_template_applies(grid, c)
            || right_col_agg(grid, lm + c).is_some()
    }) {
        None => mc,
        Some(last) => mc.saturating_sub(last + 1),
    }
}

/// Count trailing blank main rows (matching ratatui's trailing_blank_main_rows).
pub fn trailing_blank_main_rows(grid: &GridBox) -> usize {
    let hr = HEADER_ROWS;
    let mr = grid.main_rows();
    match (0..mr).rev().find(|&r| grid.logical_row_has_content(hr + r)) {
        None => mr,
        Some(last) => mr.saturating_sub(last + 1),
    }
}

/// Result of computing a cell's effective display text, style, and metadata.
pub struct CellInfo {
    /// The formatted display text (via format_cell_display), NOT truncated for column width.
    /// The caller (fill_cells) handles truncation/ellipsis/alignment.
    pub formatted: String,
    pub style: CellDisplayStyle,
    pub raw_value: Option<String>,
    pub is_agg_cell: bool,
}

/// Determine the effective display text, style, and metadata for a single cell.
/// Returns the FORMATTED (but not truncated) text, the display style, and the raw value.
/// fill_cells uses this and handles truncation/alignment/spill.
pub fn compute_cell_info(
    g: &GridBox,
    addr: &CellAddr,
    is_cursor_cell: bool,
    row_agg: Option<AggFunc>,
    main_row: Option<u32>,
    footer_row_idx: Option<u32>,
    rca: Option<AggFunc>,
    global_col: usize,
    lm: usize,
    mc: usize,
    mr: usize,
) -> CellInfo {
    let effective = if is_cursor_cell {
        cell_effective_display(g, addr)
    } else if let Some(func) = row_agg {
        if let Some(_) = footer_row_idx {
            if rca.is_some() {
                footer_special_col_aggregate(g, func, global_col, mr, mc)
                    .unwrap_or_else(|| cell_effective_display(g, addr))
            } else if global_col >= lm && global_col < lm + mc {
                let main_col = (global_col - lm) as u32;
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
                cell_effective_display(g, addr)
            }
        } else if let Some(mri) = main_row {
            if global_col >= lm && global_col < lm + mc {
                if rca.is_some() {
                    let data_cols = data_main_col_count(g);
                    let block_start = row_total_block_start(g, mri);
                    let result = if block_start < mri {
                        left_margin_special_col_aggregate(
                            g, func, global_col, block_start, mri, data_cols,
                        )
                    } else {
                        previous_raw_block(g, mri).and_then(
                            |(start, end)| {
                                left_margin_special_col_aggregate(
                                    g, func, global_col, start, end, data_cols,
                                )
                            },
                        )
                    };
                    result.unwrap_or_else(|| cell_effective_display(g, addr))
                } else {
                    let main_col = (global_col - lm) as u32;
                    left_margin_main_col_aggregate(g, func, mri, main_col)
                }
            } else if rca.is_some() {
                let data_cols = data_main_col_count(g);
                let block_start = row_total_block_start(g, mri);
                let result = if block_start < mri {
                    left_margin_special_col_aggregate(
                        g, func, global_col, block_start, mri, data_cols,
                    )
                } else {
                    previous_raw_block(g, mri).and_then(
                        |(start, end)| {
                            left_margin_special_col_aggregate(
                                g, func, global_col, start, end, data_cols,
                            )
                        },
                    )
                };
                result.unwrap_or_else(|| cell_effective_display(g, addr))
            } else {
                cell_effective_display(g, addr)
            }
        } else {
            cell_effective_display(g, addr)
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
        cell_effective_display(g, addr)
    };

    let is_agg_cell = if row_agg.is_some() {
        rca.is_some() || (global_col >= lm && global_col < lm + mc)
    } else if let Some(_) = main_row {
        rca.is_some()
    } else {
        false
    };

    let style = if is_cursor_cell {
        CellDisplayStyle::Cursor
    } else if is_agg_cell {
        if footer_row_idx.is_some() && row_agg.is_some() {
            CellDisplayStyle::FooterAggregate
        } else {
            CellDisplayStyle::Aggregate
        }
    } else {
        CellDisplayStyle::Default
    };

    let raw_value = g.get(addr);
    let formatted = crate::ui_core::format_cell_display(g, addr, effective);

    CellInfo { formatted, style, raw_value, is_agg_cell }
}

/// Display style for a cell, mapped to backend-specific rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellDisplayStyle {
    Default,
    Cursor,
    Aggregate,
    FooterAggregate,
    Selected,
    ActiveHeader,
    InactiveHeader,
}

impl CellDisplayStyle {
    /// Map to pancurses CELL_STYLE_* constants.
    #[cfg(feature = "pancurses")]
    pub fn to_pancurses_style(self) -> u8 {
        match self {
            CellDisplayStyle::Default => 0,
            CellDisplayStyle::Cursor => 1,
            CellDisplayStyle::Aggregate => 2,
            CellDisplayStyle::FooterAggregate => 3,
            CellDisplayStyle::Selected => 4,
            CellDisplayStyle::ActiveHeader => 5,
            CellDisplayStyle::InactiveHeader => 6,
        }
    }
}
