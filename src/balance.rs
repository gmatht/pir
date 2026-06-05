//! Book balancing by row reordering.

use crate::formula::{cell_effective_display, translate_formula_text, FormulaCopyContext};
use crate::grid::{CellAddr, GridBox as Grid};
use crate::ops::SheetState;
use balance_core as core;

pub use balance_core::{BalanceDirection, BalanceItem};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BalanceGroup {
    pub row_indices: Vec<usize>,
    pub total_cents: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BalanceSourceRow {
    pub row_index: usize,
    pub amount_cents: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BalanceReport {
    pub direction: BalanceDirection,
    pub amount_col: usize,
    pub groups: Vec<BalanceGroup>,
    pub leftovers: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BalanceCopyPlan {
    pub source_sheet_id: u32,
    pub source_sheet_title: String,
    pub target_sheet_id: u32,
    pub target_title: String,
    pub amount_col: usize,
    pub row_order: Vec<usize>,
    pub unmatched_start: usize,
    pub show_unmatched_heading: bool,
    pub preserve_formulas: bool,
}

pub fn parse_amount_cents(raw: &str) -> Option<i64> {
    core::parse_amount_cents(raw)
}

pub fn format_amount_cents(cents: i64) -> String {
    core::format_amount_cents(cents)
}

pub fn choose_balance_column(grid: &Grid) -> Option<usize> {
    let mut first_numeric = None;
    for col in 0..grid.main_cols() {
        let mut saw_pos = false;
        let mut saw_neg = false;
        for row in 0..grid.main_rows() {
            let addr = CellAddr::Main {
                row: row as u32,
                col: col as u32,
            };
            if let Some(raw) = grid.get(&addr) {
                if let Some(amount) = parse_amount_cents(&raw) {
                    first_numeric.get_or_insert(col);
                    saw_pos |= amount > 0;
                    saw_neg |= amount < 0;
                }
            }
            if saw_pos && saw_neg {
                return Some(col);
            }
        }
    }
    first_numeric
}

pub fn source_rows_from_grid(grid: &Grid, col: usize) -> Vec<BalanceSourceRow> {
    let mut rows = Vec::new();
    for row in 0..grid.main_rows() {
        let amount_addr = CellAddr::Main {
            row: row as u32,
            col: col as u32,
        };
        let amount = grid
            .get(&amount_addr)
            .and_then(|s| parse_amount_cents(&s))
            .unwrap_or(0);
        rows.push(BalanceSourceRow {
            row_index: row,
            amount_cents: amount,
        });
    }
    rows
}

pub fn balance_books(
    rows: &[BalanceSourceRow],
    direction: BalanceDirection,
    amount_col: usize,
) -> BalanceReport {
    let items: Vec<BalanceItem> = rows
        .iter()
        .map(|row| BalanceItem {
            id: row.row_index,
            amount_cents: row.amount_cents,
        })
        .collect();
    let sol = core::solve_balance_groups(&items, direction);
    BalanceReport {
        direction,
        amount_col,
        groups: sol
            .groups
            .into_iter()
            .map(|group| BalanceGroup {
                row_indices: group.item_ids,
                total_cents: group.total_cents,
            })
            .collect(),
        leftovers: sol.leftovers,
    }
}

pub fn build_balance_report(grid: &Grid, col: usize, direction: BalanceDirection) -> BalanceReport {
    balance_books(&source_rows_from_grid(grid, col), direction, col)
}

pub fn balance_copy_plan(
    source_sheet_id: u32,
    source_sheet_title: String,
    target_sheet_id: u32,
    target_title: String,
    amount_col: usize,
    total_rows: usize,
    report: &BalanceReport,
    preserve_formulas: bool,
) -> BalanceCopyPlan {
    let row_order = row_order_from_groups(report, total_rows);
    let unmatched_start = report
        .groups
        .iter()
        .map(|group| group.row_indices.len())
        .sum::<usize>();
    let show_unmatched_heading = !report.groups.is_empty() && !report.leftovers.is_empty();
    BalanceCopyPlan {
        source_sheet_id,
        source_sheet_title,
        target_sheet_id,
        target_title,
        amount_col,
        row_order,
        unmatched_start,
        show_unmatched_heading,
        preserve_formulas,
    }
}

pub fn apply_balance_copy(source: &SheetState, target: &mut SheetState, plan: &BalanceCopyPlan) {
    let mc = source.grid.main_cols();
    let mr = source.grid.main_rows();
    let extra_row = usize::from(plan.show_unmatched_heading);
    target
        .grid
        .set_main_size(mr.saturating_add(extra_row).max(1), mc.max(1));
    // Clear cell storage and spills on the target (use GridBox API).
    target.grid.clear_cells();
    target.grid.clear_spills();
    target.grid.set_view_sort_cols(Vec::new());

    let mut row_map = vec![0u32; mr];
    for (new_row, &old_row) in plan.row_order.iter().enumerate() {
        if old_row < row_map.len() {
            row_map[old_row] = if new_row >= plan.unmatched_start {
                (new_row + extra_row) as u32
            } else {
                new_row as u32
            };
        }
    }
    let ctx = FormulaCopyContext {
        source_sheet_id: plan.source_sheet_id,
        target_sheet_id: plan.target_sheet_id,
        row_map: row_map.clone(),
        main_cols: source.grid.main_cols(),
    };

    let copy_cell =
        |src: &SheetState, dst: &mut SheetState, src_addr: CellAddr, dst_addr: CellAddr| {
            if let Some(raw) = src.grid.get(&src_addr) {
                let value = if plan.preserve_formulas && raw.trim_start().starts_with('=') {
                    // translate_formula_text expects &str; borrow the owned String
                    translate_formula_text(&raw, &ctx)
                        .unwrap_or_else(|| cell_effective_display(&src.grid, &src_addr))
                } else {
                    raw.to_string()
                };
                if !value.is_empty() {
                    dst.grid.set(&dst_addr, value);
                }
            }
        };

    for (src_addr, _) in source.grid.iter_nonempty() {
        if matches!(src_addr, CellAddr::Header { .. }) {
            let dst_addr = src_addr.clone();
            copy_cell(source, target, src_addr, dst_addr);
        }
    }

    for src_row in 0..mr {
        let dst_row = *row_map.get(src_row).unwrap_or(&(src_row as u32));
        for col in 0..mc {
            let src_addr = CellAddr::Main {
                row: src_row as u32,
                col: col as u32,
            };
            let dst_addr = CellAddr::Main {
                row: dst_row,
                col: col as u32,
            };
            copy_cell(source, target, src_addr, dst_addr);
        }
        for col in 0..crate::grid::MARGIN_COLS {
            let src_left = CellAddr::Left {
                col,
                row: src_row as u32,
            };
            let dst_left = CellAddr::Left { col, row: dst_row };
            copy_cell(source, target, src_left, dst_left);

            let src_right = CellAddr::Right {
                col,
                row: src_row as u32,
            };
            let dst_right = CellAddr::Right { col, row: dst_row };
            copy_cell(source, target, src_right, dst_right);
        }
    }

    if plan.show_unmatched_heading {
        let heading_row = plan.unmatched_start;
        if heading_row < target.grid.main_rows() {
            target.grid.set(
                &CellAddr::Main {
                    row: heading_row as u32,
                    col: 0,
                },
                "UNMATCHED".into(),
            );
        }
    }

    for (src_addr, _) in source.grid.iter_nonempty() {
        if matches!(src_addr, CellAddr::Footer { .. }) {
            let dst_addr = src_addr.clone();
            copy_cell(source, target, src_addr, dst_addr);
        }
    }

    target.grid.set_max_col_width(source.grid.max_col_width());
    target
        .grid
        .set_col_width_overrides(source.grid.col_width_overrides());
    target.grid.set_volatile_seed(0);
}

pub fn materialize_report_sheet(source: &SheetState, plan: &BalanceCopyPlan) -> SheetState {
    let mut target = SheetState::new(source.grid.main_rows(), source.grid.main_cols());
    apply_balance_copy(source, &mut target, plan);
    target
}

pub fn row_order_from_groups(report: &BalanceReport, total_rows: usize) -> Vec<usize> {
    let mut order = Vec::new();
    for group in &report.groups {
        order.extend(group.row_indices.iter().copied());
    }
    order.extend(report.leftovers.iter().copied());
    for row in 0..total_rows {
        if !order.contains(&row) {
            order.push(row);
        }
    }
    order
}

pub fn reordered_row_order(report: &BalanceReport, total_rows: usize) -> Vec<usize> {
    row_order_from_groups(report, total_rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{CellAddr, Grid, GridBox};
    use std::collections::{BTreeSet, HashMap};

    #[derive(Clone, Debug)]
    struct TinyRng(u64);

    impl TinyRng {
        fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next_u32(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            (self.0 >> 32) as u32
        }

        fn usize_inclusive(&mut self, lo: usize, hi: usize) -> usize {
            if lo >= hi {
                return lo;
            }
            lo + (self.next_u32() as usize % (hi - lo + 1))
        }

        fn shuffle<T>(&mut self, xs: &mut [T]) {
            for i in (1..xs.len()).rev() {
                let j = self.next_u32() as usize % (i + 1);
                xs.swap(i, j);
            }
        }
    }

    #[derive(Clone, Debug)]
    struct PlannedGroup {
        anchor_row: usize,
        reimbursement_rows: Vec<usize>,
    }

    fn build_balancing_sheet(
        seed: u64,
        direction: BalanceDirection,
    ) -> (GridBox, Vec<PlannedGroup>, BTreeSet<usize>) {
        let mut rng = TinyRng::new(seed);
        // Build 3-7 independent balancing groups with disjoint power-of-two bit ranges.
        let group_count = rng.usize_inclusive(3, 7);

        // Each row stores: (signed cents, optional group id)
        let mut rows: Vec<(i64, Option<usize>)> = Vec::new();
        let mut planned = Vec::new();
        for g in 0..group_count {
            let bit_count = rng.usize_inclusive(2, 4);
            let bit_base = g * 6; // disjoint bit windows => unique subset sums per group
            let mut reimbursements = Vec::new();
            let mut target = 0i64;
            for bit in 0..bit_count {
                let cents = 1i64 << (bit_base + bit);
                target += cents;
                let signed = match direction {
                    BalanceDirection::PosToNeg => -cents,
                    BalanceDirection::NegToPos => cents,
                };
                let row_idx = rows.len();
                rows.push((signed, Some(g)));
                reimbursements.push(row_idx);
            }
            let anchor_signed = match direction {
                BalanceDirection::PosToNeg => target,
                BalanceDirection::NegToPos => -target,
            };
            let anchor_row = rows.len();
            rows.push((anchor_signed, Some(g)));
            planned.push(PlannedGroup {
                anchor_row,
                reimbursement_rows: reimbursements,
            });
        }

        // Add 1-4 unmatched noise rows (zero amounts never join supply/demand sets).
        let noise_count = rng.usize_inclusive(1, 4);
        let mut expected_leftovers = BTreeSet::new();
        for _ in 0..noise_count {
            let row_idx = rows.len();
            rows.push((0, None));
            expected_leftovers.insert(row_idx);
        }

        // Shuffle row order to fuzz row-position effects.
        let mut perm: Vec<usize> = (0..rows.len()).collect();
        rng.shuffle(&mut perm);
        let mut inverse = vec![0usize; rows.len()];
        for (new_pos, old_pos) in perm.iter().copied().enumerate() {
            inverse[old_pos] = new_pos;
        }

        let mut grid = GridBox::from(Grid::new(rows.len().max(1) as u32, 2));
        for (new_row, old_row) in perm.iter().copied().enumerate() {
            let cents = rows[old_row].0;
            grid.set(
                &CellAddr::Main {
                    row: new_row as u32,
                    col: 0,
                },
                format!("row-{old_row}"),
            );
            grid.set(
                &CellAddr::Main {
                    row: new_row as u32,
                    col: 1,
                },
                format_amount_cents(cents),
            );
        }

        let remapped_groups = planned
            .into_iter()
            .map(|g| PlannedGroup {
                anchor_row: inverse[g.anchor_row],
                reimbursement_rows: g
                    .reimbursement_rows
                    .into_iter()
                    .map(|r| inverse[r])
                    .collect(),
            })
            .collect::<Vec<_>>();
        let remapped_leftovers = expected_leftovers
            .into_iter()
            .map(|r| inverse[r])
            .collect::<BTreeSet<_>>();

        (grid, remapped_groups, remapped_leftovers)
    }

    fn report_groups_as_sets(report: &BalanceReport) -> Vec<BTreeSet<usize>> {
        report
            .groups
            .iter()
            .map(|g| g.row_indices.iter().copied().collect::<BTreeSet<_>>())
            .collect()
    }

    #[test]
    fn balance_books_fuzz_pos_to_neg_groups_match_expected_reimbursements() {
        for seed in 0..120u64 {
            let (grid, expected, expected_leftovers) =
                build_balancing_sheet(seed, BalanceDirection::PosToNeg);
            let report = build_balance_report(&grid, 1, BalanceDirection::PosToNeg);
            assert_eq!(report.amount_col, 1);

            let expected_sets = expected
                .iter()
                .map(|g| {
                    std::iter::once(g.anchor_row)
                        .chain(g.reimbursement_rows.iter().copied())
                        .collect::<BTreeSet<_>>()
                })
                .collect::<Vec<_>>();
            let got_sets = report_groups_as_sets(&report);

            for expected_set in &expected_sets {
                assert!(
                    got_sets.contains(expected_set),
                    "missing expected matched group for seed {seed}: {expected_set:?}"
                );
            }

            let got_leftovers = report.leftovers.iter().copied().collect::<BTreeSet<_>>();
            assert_eq!(got_leftovers, expected_leftovers, "seed {seed}");
        }
    }

    #[test]
    fn balance_books_fuzz_neg_to_pos_groups_match_expected_reimbursements() {
        for seed in 200..320u64 {
            let (grid, expected, expected_leftovers) =
                build_balancing_sheet(seed, BalanceDirection::NegToPos);
            let report = build_balance_report(&grid, 1, BalanceDirection::NegToPos);
            assert_eq!(report.amount_col, 1);

            // Also ensure grouping keeps each anchor with exactly its known balancing rows.
            let expected_by_anchor = expected
                .iter()
                .map(|g| {
                    let mut set = BTreeSet::new();
                    set.insert(g.anchor_row);
                    set.extend(g.reimbursement_rows.iter().copied());
                    (g.anchor_row, set)
                })
                .collect::<HashMap<_, _>>();

            for group in &report.groups {
                let set = group.row_indices.iter().copied().collect::<BTreeSet<_>>();
                let anchor = group
                    .row_indices
                    .iter()
                    .copied()
                    .find(|row| expected_by_anchor.contains_key(row));
                if let Some(anchor) = anchor {
                    assert_eq!(
                        Some(&set),
                        expected_by_anchor.get(&anchor),
                        "unexpected match composition for seed {seed}, anchor row {anchor}"
                    );
                }
            }

            let got_leftovers = report.leftovers.iter().copied().collect::<BTreeSet<_>>();
            assert_eq!(got_leftovers, expected_leftovers, "seed {seed}");

            // Sanity: balance column chooser still selects the amount column in these sheets.
            assert_eq!(choose_balance_column(&grid), Some(1));
        }
    }

    #[test]
    fn balance_copy_plan_fuzz_materializes_groups_then_unmatched_block() {
        for (seed, direction) in (400..460u64)
            .map(|s| (s, BalanceDirection::PosToNeg))
            .chain((460..520u64).map(|s| (s, BalanceDirection::NegToPos)))
        {
            let (grid, expected_groups, expected_leftovers) = build_balancing_sheet(seed, direction);
            let report = build_balance_report(&grid, 1, direction);

            let mut source = SheetState::new(grid.main_rows(), grid.main_cols());
            source.grid = grid.clone();
            let plan = balance_copy_plan(
                1,
                "source".into(),
                2,
                "balanced".into(),
                1,
                source.grid.main_rows(),
                &report,
                false,
            );
            let materialized = materialize_report_sheet(&source, &plan);

            // Verify the copied main rows follow the exact plan order, with an optional
            // UNMATCHED heading inserted between grouped and unmatched blocks.
            let expected_order = row_order_from_groups(&report, source.grid.main_rows());
            let heading_offset = usize::from(plan.show_unmatched_heading);
            for (dst_row, src_row) in expected_order.iter().copied().enumerate() {
                let mapped_dst_row = if dst_row >= plan.unmatched_start {
                    dst_row + heading_offset
                } else {
                    dst_row
                };
                let src_label = source
                    .grid
                    .get(&CellAddr::Main {
                        row: src_row as u32,
                        col: 0,
                    })
                    .unwrap_or_default();
                let dst_label = materialized
                    .grid
                    .get(&CellAddr::Main {
                        row: mapped_dst_row as u32,
                        col: 0,
                    })
                    .unwrap_or_default();
                assert_eq!(
                    dst_label, src_label,
                    "seed {seed} direction {:?}: row mapping mismatch for src row {src_row}",
                    direction
                );
            }

            if plan.show_unmatched_heading {
                assert_eq!(
                    materialized.grid.get(&CellAddr::Main {
                        row: plan.unmatched_start as u32,
                        col: 0
                    }),
                    Some("UNMATCHED".to_string()),
                    "seed {seed} direction {:?}: missing unmatched heading",
                    direction
                );
            }

            // Group blocks in materialized output should correspond to expected anchor+reimbursement sets.
            let expected_sets = expected_groups
                .iter()
                .map(|g| {
                    std::iter::once(g.anchor_row)
                        .chain(g.reimbursement_rows.iter().copied())
                        .collect::<BTreeSet<_>>()
                })
                .collect::<Vec<_>>();
            let got_sets = report_groups_as_sets(&report);
            for expected_set in &expected_sets {
                assert!(
                    got_sets.contains(expected_set),
                    "seed {seed} direction {:?}: missing expected group {expected_set:?}",
                    direction
                );
            }

            // Leftovers should be exactly the known unmatched rows and appear in the unmatched block.
            let got_leftovers = report.leftovers.iter().copied().collect::<BTreeSet<_>>();
            assert_eq!(
                got_leftovers, expected_leftovers,
                "seed {seed} direction {:?}: leftovers mismatch",
                direction
            );
            if !expected_leftovers.is_empty() {
                let unmatched_slice = expected_order
                    .iter()
                    .skip(plan.unmatched_start)
                    .copied()
                    .collect::<BTreeSet<_>>();
                assert_eq!(
                    unmatched_slice, expected_leftovers,
                    "seed {seed} direction {:?}: unmatched block rows mismatch",
                    direction
                );
            }
        }
    }
}
