use crate::formula::cell_effective_display;
use crate::grid::{CellAddr, ColumnAddr, GridBox as Grid, MainRange, MARGIN_COLS};
use crate::ops::{AggFunc, AggregateDef};

// Re-exported for the always-compiled default UI (ui/mod.rs, ui_core.rs) which
// cannot reach the gui-gated gui::compute version.
pub(crate) use crate::ods::footer_row_agg_func;

/// Compute a footer aggregate value across all main rows for a given column.
/// Mirrors gui::compute::footer_special_col_aggregate but lives in this always
/// compiled module so the default ratatui UI can use it.
pub(crate) fn footer_special_col_aggregate(
    grid: &Grid,
    footer_func: AggFunc,
    global_col: usize,
    main_rows: usize,
    main_cols: usize,
) -> Option<String> {
    let row_func = right_col_agg_func(grid, global_col);
    let data_cols = data_main_col_count(grid);
    let mut samples: Vec<f64> = Vec::new();
    for r in 0..main_rows {
        let row_val = if let Some(func) = row_func {
            crate::agg::compute_aggregate(
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

// Internal helpers kept private to this module
pub(crate) fn right_col_agg_func(grid: &Grid, global_col: usize) -> Option<AggFunc> {
    let main_cols = grid.main_cols();
    let mut labels: Vec<(u32, String)> = grid
        .iter_nonempty()
        .filter_map(|(addr, val)| match addr {
            CellAddr::Header { row, col } if col.to_global(main_cols) == global_col => Some((row, val)),
            _ => None,
        })
        .collect();
    labels.sort_unstable_by_key(|(row, _)| *row);
    for (_, val) in labels {
        if let Some(f) = crate::ops::margin_key_agg_func(&val) {
            return Some(f);
        }
    }
    None
}

pub(crate) fn left_margin_agg_func(grid: &Grid, main_row: u32) -> Option<AggFunc> {
    let key_col = MARGIN_COLS - 1;
    let val = grid.get(&CellAddr::Left { col: key_col, row: main_row })?;
    crate::ops::margin_key_agg_func(&val)
}

pub(crate) fn row_total_block_start(grid: &Grid, current_main_row: u32) -> u32 {
    for candidate in (0..current_main_row).rev() {
        if left_margin_agg_func(grid, candidate).is_some() {
            return candidate + 1;
        }
    }
    0
}

pub(crate) fn parse_num(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    t.parse::<f64>().ok()
}

pub(crate) fn fold_numbers(func: AggFunc, xs: &[f64]) -> String {
    if xs.is_empty() {
        return String::new();
    }
    match func {
        AggFunc::Sum => format!("{}", xs.iter().sum::<f64>()),
        AggFunc::Mean => format!("{}", xs.iter().sum::<f64>() / xs.len() as f64),
        AggFunc::Median => {
            let mut ys = xs.to_vec();
            ys.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let n = ys.len();
            let m = if n % 2 == 1 { ys[n / 2] } else { (ys[n / 2 - 1] + ys[n / 2]) / 2.0 };
            format!("{m}")
        }
        AggFunc::Min => xs
            .iter()
            .copied()
            .min_by(|a, b| a.partial_cmp(b).unwrap())
            .map(|v| format!("{v}"))
            .unwrap_or_default(),
        AggFunc::Max => xs
            .iter()
            .copied()
            .max_by(|a, b| a.partial_cmp(b).unwrap())
            .map(|v| format!("{v}"))
            .unwrap_or_default(),
        AggFunc::Count => format!("{}", xs.len()),
    }
}

// Shared helper functions used by UI and ODS
pub(crate) fn data_main_col_count(grid: &Grid) -> usize {
    let mc = grid.main_cols();
    for c in 0..mc {
        if right_col_agg_func(grid, MARGIN_COLS + c).is_some() {
            return c + 1;
        }
    }
    mc
}

pub(crate) fn previous_raw_block(grid: &Grid, current_main_row: u32) -> Option<(u32, u32)> {
    let mut end = current_main_row;
    while end > 0 {
        let last_agg = (0..end)
            .rev()
            .find(|&r| left_margin_agg_func(grid, r).is_some())
            .unwrap_or(0);
        let prev_agg = if last_agg == 0 {
            None
        } else {
            (0..last_agg)
                .rev()
                .find(|&r| left_margin_agg_func(grid, r).is_some())
        };
        let start = prev_agg.map_or(0, |r| r + 1);
        if start < last_agg {
            return Some((start, last_agg));
        }
        if last_agg == 0 {
            return Some((0, end));
        }
        end = last_agg;
    }
    Some((0, current_main_row))
}

pub(crate) fn left_margin_main_col_aggregate(
    grid: &Grid,
    subtotal_func: AggFunc,
    main_row: u32,
    main_col: u32,
) -> String {
    let block_start = row_total_block_start(grid, main_row);
    let Some((start, end)) = (if block_start < main_row {
        Some((block_start, main_row))
    } else {
        previous_raw_block(grid, main_row)
    }) else {
        return String::new();
    };
    crate::agg::compute_aggregate(
        grid,
        &AggregateDef {
            func: subtotal_func,
            source: MainRange {
                row_start: start,
                row_end: end,
                col_start: main_col,
                col_end: main_col + 1,
            },
        },
    )
}

pub(crate) fn left_margin_special_col_aggregate(
    grid: &Grid,
    subtotal_func: AggFunc,
    global_col: usize,
    row_start: u32,
    row_end: u32,
    data_cols: usize,
) -> Option<String> {
    let row_func = right_col_agg_func(grid, global_col)?;
    let collect = |row_start: u32, row_end: u32| -> Vec<f64> {
        let mut samples: Vec<f64> = Vec::new();
        for r in row_start..row_end {
            let row_val = crate::agg::compute_aggregate(
                grid,
                &AggregateDef {
                    func: row_func,
                    source: MainRange {
                        row_start: r,
                        row_end: r + 1,
                        col_start: 0,
                        col_end: data_cols as u32,
                    },
                },
            );
            if let Some(n) = parse_num(&row_val) {
                samples.push(n);
            }
        }
        samples
    };

    let mut samples = collect(row_start, row_end);
    let mut end = row_start;
    while samples.is_empty() && end > 0 {
        let Some((fallback_start, fallback_end)) = previous_raw_block(grid, end) else {
            break;
        };
        samples = collect(fallback_start, fallback_end);
        if fallback_start == 0 {
            break;
        }
        end = fallback_start;
    }
    Some(fold_numbers(subtotal_func, &samples))
}
