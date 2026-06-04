//! Aggregate functions over main-region numeric samples.

use crate::formula::{self, Number};
use crate::grid::{CellAddr, GridBox as Grid, MainRange};
use crate::ops::{AggFunc, AggregateDef};

pub mod helpers;

#[inline(always)]
/// Formatting for margin aggregates when only an [`f64`] is available (`Number::Approx` path).
fn format_aggregate_approx(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    let s = format!("{value:.10}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

#[inline(always)]
/// Preserve [`Number::Exact`] without a `float` round-trip; match cell-style rational display.
fn format_aggregate_number(n: &Number) -> String {
    n.format_eval_display(format_aggregate_approx)
}
#[inline(always)]

fn cmp_number_aggregate(a: &Number, b: &Number) -> std::cmp::Ordering {
    a.partial_cmp(b)
        .unwrap_or(std::cmp::Ordering::Equal)
}

#[inline(always)]
/// Median of sample values using the same ordering as formulas (IEEE-aware `Exact` vs `Approx`).
fn median_aggregate(mut xs: Vec<Number>) -> Option<Number> {
    if xs.is_empty() {
        return None;
    }
    xs.sort_by(|a, b| cmp_number_aggregate(a, b));
    let n = xs.len();
    if n % 2 == 1 {
        Some(xs[n / 2].clone())
    } else {
        let a = xs[n / 2 - 1].clone();
        let b = xs[n / 2].clone();
        Some(a.add(b).div(Number::from_i64(2)))
    }
}

#[inline(always)]
fn collect_numbers_summable(grid: &Grid, range: &MainRange) -> Vec<Number> {
    let mut v = Vec::new();
    if range.is_empty() {
        return v;
    }
    let mut visiting = Vec::new();
    let mut budget = formula::EVAL_BUDGET_AGG;
    for r in range.row_start..range.row_end {
        for c in range.col_start..range.col_end {
            let addr = CellAddr::Main { row: r, col: c };
            if let Some(n) = formula::summable_numeric(grid, &addr, &mut visiting, &mut budget) {
                v.push(n);
            }
        }
    }
    v
}

#[inline(always)]
fn count_numeric_cells(grid: &Grid, range: &MainRange) -> usize {
    let mut n = 0usize;
    if range.is_empty() {
        return n;
    }
    let mut visiting = Vec::new();
    let mut budget = formula::EVAL_BUDGET_AGG;
    for r in range.row_start..range.row_end {
        for c in range.col_start..range.col_end {
            let addr = CellAddr::Main { row: r, col: c };
            if formula::effective_numeric(grid, &addr, &mut visiting, &mut budget).is_some() {
                n += 1;
            }
        }
    }
    n
}

/// Compute display string for an aggregate over `source` main cells.
#[inline(always)]
pub fn compute_aggregate(grid: &Grid, def: &AggregateDef) -> String {
    match def.func {
        AggFunc::Count => {
            let n = count_numeric_cells(grid, &def.source);
            if n == 0 {
                String::new()
            } else {
                format!("{}", n)
            }
        }
        AggFunc::Sum => {
            let xs = collect_numbers_summable(grid, &def.source);
            if xs.is_empty() {
                String::new()
            } else {
                let s = xs
                    .iter()
                    .cloned()
                    .fold(Number::exact_zero(), |a, b| a.add(b));
                format_aggregate_number(&s)
            }
        }
        AggFunc::Mean => {
            let xs = collect_numbers_summable(grid, &def.source);
            if xs.is_empty() {
                String::new()
            } else {
                let sum = xs
                    .iter()
                    .cloned()
                    .fold(Number::exact_zero(), |a, b| a.add(b));
                let s = sum.div(Number::from_i64(xs.len() as i64));
                format_aggregate_number(&s)
            }
        }
        AggFunc::Median => median_aggregate(collect_numbers_summable(grid, &def.source))
            .map(|m| format_aggregate_number(&m))
            .unwrap_or_default(),
        AggFunc::Min => collect_numbers_summable(grid, &def.source)
            .into_iter()
            .min_by(|a, b| cmp_number_aggregate(a, b))
            .map(|n| format_aggregate_number(&n))
            .unwrap_or_default(),
        AggFunc::Max => collect_numbers_summable(grid, &def.source)
            .into_iter()
            .max_by(|a, b| cmp_number_aggregate(a, b))
            .map(|n| format_aggregate_number(&n))
            .unwrap_or_default(),
    }
}

/// Raw cell value for display.
#[inline(always)]
pub fn cell_display(grid: &Grid, addr: &CellAddr) -> String {
    // GridBox provides `text` which returns an owned String for the addr.
    grid.text(addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::GridBox;
    use crate::grid::{Grid, HEADER_ROWS, MARGIN_COLS};

    #[test]
    fn sum_mean() {
        let mut g = Grid::new(2, 2);
        g.set(&CellAddr::Main { row: 0, col: 0 }, "2".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "3".into());
        let def = AggregateDef {
            func: AggFunc::Sum,
            source: MainRange {
                row_start: 0,
                row_end: 2,
                col_start: 0,
                col_end: 2,
            },
        };
        let gb = GridBox::from(g);
        assert_eq!(compute_aggregate(&gb, &def), "5");
    }

    #[test]
    fn aggregate_includes_formula_numeric() {
        let mut g = Grid::new(1, 2);
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=1+1".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "3".into());
        let def = AggregateDef {
            func: AggFunc::Sum,
            source: MainRange {
                row_start: 0,
                row_end: 1,
                col_start: 0,
                col_end: 2,
            },
        };
        let gb = GridBox::from(g);
        assert_eq!(compute_aggregate(&gb, &def), "5");
    }

    #[test]
    fn aggregate_ignores_template_zero_from_blank_references() {
        let mut g = GridBox::from(Grid::new(2, 2));
        g.set(
            &CellAddr::Header {
                row: (HEADER_ROWS - 1) as u32,
                col: crate::grid::ColumnAddr::Main(1),
            },
            "=A*0.1 -- TAX".into(),
        );

        let def = AggregateDef {
            func: AggFunc::Sum,
            source: MainRange {
                row_start: 0,
                row_end: 2,
                col_start: 1,
                col_end: 2,
            },
        };
        assert_eq!(compute_aggregate(&g, &def), "");

        let def = AggregateDef {
            func: AggFunc::Min,
            source: MainRange {
                row_start: 0,
                row_end: 2,
                col_start: 1,
                col_end: 2,
            },
        };
        assert_eq!(compute_aggregate(&g, &def), "");

        let def = AggregateDef {
            func: AggFunc::Mean,
            source: MainRange {
                row_start: 0,
                row_end: 2,
                col_start: 1,
                col_end: 2,
            },
        };
        assert_eq!(compute_aggregate(&g, &def), "");

        g.set(&CellAddr::Main { row: 1, col: 0 }, "0".into());
        assert_eq!(compute_aggregate(&g, &def), "0");
    }

    #[test]
    fn aggregate_sum_median_exact_decimal_display() {
        let mut g = Grid::new(1, 5);
        for (c, lit) in ["0.1", "0.2", "0.3", "0.4", "0.5"].iter().enumerate() {
            g.set(&CellAddr::Main { row: 0, col: c as u32 }, (*lit).into());
        }
        let range = MainRange {
            row_start: 0,
            row_end: 1,
            col_start: 0,
            col_end: 5,
        };

        let median_def = AggregateDef {
            func: AggFunc::Median,
            source: range,
        };
        let gb = GridBox::from(g);
        assert_eq!(compute_aggregate(&gb, &median_def), "0.3");

        let sum_def = AggregateDef {
            func: AggFunc::Sum,
            source: MainRange {
                row_start: 0,
                row_end: 1,
                col_start: 0,
                col_end: 5,
            },
        };
        assert_eq!(compute_aggregate(&gb, &sum_def), "1.5");
    }
}
