//! `=...` cell formulas: parse, evaluate, display.

use crate::addr::{
    corner_locks_for_bbox, excel_column_name, formula_cell_ref_text, parse_cell_ref_at,
    parse_main_range_formula_at, A1RefLocks,
};
use crate::grid::{CellAddr, ColumnAddr, GridBox as Grid, MainRange, HEADER_ROWS, MARGIN_COLS};
use crate::ops::{AggFunc, WorkbookState};
use std::cell::RefCell;

mod functions;
pub mod number;

pub use number::Number;
pub(crate) use number::exact_decimal_generic_scientific;

thread_local! {
    static EVAL_WORKBOOK: RefCell<Option<WorkbookState>> = const { RefCell::new(None) };
}
thread_local! {
    // Global per-thread stack of (sheet_id, CellAddr) pairs used when
    // evaluating sheet-qualified references. Using a shared stack (instead
    // of creating a fresh Vec for every SheetRef) lets us detect cycles that
    // span multiple sheets (A!X -> B!Y -> A!X) and avoid infinite recursion
    // / stack overflow.
    static SHEET_VISITING: RefCell<Vec<(u32, CellAddr)>> = const { RefCell::new(Vec::new()) };
}

// Lightweight per-thread recursion depth counter to guard against evaluation
// paths that recurse without pushing to the visiting stack (and so would not
// otherwise be bounded by MAX_VISIT_DEPTH). We use an RAII guard to ensure
// the counter is decremented on function exit.
thread_local! {
    static EVAL_RECURSION_DEPTH: std::cell::Cell<usize> = std::cell::Cell::new(0);
}

#[derive(Clone, Debug)]
pub struct FormulaCopyContext {
    pub source_sheet_id: u32,
    pub target_sheet_id: u32,
    pub row_map: Vec<u32>,
    pub main_cols: usize,
}

pub struct EvalContextGuard;

impl Drop for EvalContextGuard {
    fn drop(&mut self) {
        EVAL_WORKBOOK.with(|wb| *wb.borrow_mut() = None);
    }
}

pub fn set_eval_context(workbook: &WorkbookState) -> EvalContextGuard {
    EVAL_WORKBOOK.with(|wb| *wb.borrow_mut() = Some(workbook.clone()));
    EvalContextGuard
}

fn workbook_lookup(sheet_id: u32) -> Option<Grid> {
    EVAL_WORKBOOK.with(|wb| {
        wb.borrow()
            .as_ref()
            .and_then(|w| w.sheets.iter().find(|s| s.id == sheet_id))
            .map(|s| s.state.grid.clone())
    })
}

fn workbook_lookup_sheet_ref(sheet: &str) -> Option<(u32, Grid)> {
    EVAL_WORKBOOK.with(|wb| {
        let wb = wb.borrow();
        let wb = wb.as_ref()?;
        if let Some(rest) = sheet.strip_prefix("Sheet") {
            if let Ok(id) = rest.parse::<u32>() {
                let rec = wb.sheets.iter().find(|s| s.id == id)?;
                return Some((rec.id, rec.state.grid.clone()));
            }
        }
        let rec = wb.sheets.iter().find(|s| s.title == sheet)?;
        Some((rec.id, rec.state.grid.clone()))
    })
}
const DEFAULT_BUDGET: usize = 10_000;

// Protect against runaway recursion (deep acyclic graphs or missed cycle
// detection) which can overflow the thread stack. This limits the number of
// active cell visits on the evaluation stack; when exceeded we return a
// transient LIMIT error so the caller can handle it rather than crashing.
// Lowered from 1000 to 128 to reduce the chance of exhausting the OS stack
// before our logical guard fires.
const MAX_VISIT_DEPTH: usize = 128;

/// Evaluation step budget for one aggregate range scan (many cells).
pub const EVAL_BUDGET_AGG: usize = 1_000_000;

/// Result of evaluating a cell (formula or plain).
#[derive(Clone, Debug, PartialEq)]
pub enum EvalResult {
    Number(Number),
    /// Logical spreadsheet value (`TRUE` / `FALSE`): distinct runtime type from numbers.
    Bool(bool),
    Text(String),
    Array(Vec<Vec<EvalResult>>),
    /// Display as `#msg` in the UI.
    Error(&'static str),
}

/// Exact `0` / `1` used when coercing booleans into numeric contexts (SUM, `+`, etc.).
#[inline]
pub(super) fn number_from_bool(b: bool) -> Number {
    if b {
        Number::one()
    } else {
        Number::exact_zero()
    }
}

fn eval_plain_cell_raw(trimmed: &str) -> EvalResult {
    if let Some(n) = parse_number_literal(trimmed) {
        EvalResult::Number(n)
    } else if trimmed.eq_ignore_ascii_case("true") {
        EvalResult::Bool(true)
    } else if trimmed.eq_ignore_ascii_case("false") {
        EvalResult::Bool(false)
    } else {
        EvalResult::Text(trimmed.to_string())
    }
}

impl EvalResult {
    pub(crate) fn scalar_coerce(self) -> EvalResult {
        match self {
            EvalResult::Array(rows) => rows
                .into_iter()
                .next()
                .and_then(|row| row.into_iter().next())
                .unwrap_or(EvalResult::Error("CALC"))
                .scalar_coerce(),
            other => other,
        }
    }

    fn top_left(&self) -> Option<&EvalResult> {
        match self {
            EvalResult::Array(rows) => rows.first().and_then(|row| row.first()),
            _ => Some(self),
        }
    }

    fn as_text(&self) -> Option<&str> {
        match self {
            EvalResult::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }
}

pub(crate) fn parse_number_literal(s: &str) -> Option<Number> {
    number::parse_number_literal(s)
}

pub(crate) fn parse_numeric_or_date_literal(s: &str) -> Option<Number> {
    #[cfg(test)]
    {
        if s.contains("2001/01/01") || s.contains("2001-01-01") || s.contains('/') {
            crate::debug_log::log(&format!("DEBUG wrapper parse_numeric_or_date_literal called: {:?}", s));
        }
    }
    let res = functions::parse_numeric_or_date_literal(s);
    #[cfg(test)]
    {
        if s.contains("2001/01/01") || s.contains("2001-01-01") || s.contains('/') {
            crate::debug_log::log(&format!("DEBUG wrapper parse_numeric_or_date_literal result: {:?}", res.as_ref().map(|n| n.to_f64())));
        }
    }
    res
}

fn resolve_name(name: &str, bindings: &[(String, EvalResult)]) -> Option<EvalResult> {
    if let Some((_, value)) = bindings.iter().rev().find(|(n, _)| n == name) {
        return Some(value.clone());
    }
    if name.eq_ignore_ascii_case("true") {
        return Some(EvalResult::Bool(true));
    }
    if name.eq_ignore_ascii_case("false") {
        return Some(EvalResult::Bool(false));
    }

    match name {
        "π" => Some(EvalResult::Number(Number::from_f64_unchecked(std::f64::consts::PI))),
        "e" => Some(EvalResult::Number(Number::from_f64_unchecked(std::f64::consts::E))),
        "i" => Some(EvalResult::Number(Number::from_complex_unchecked(0.0, 1.0))),
        "c" => Some(EvalResult::Number(Number::from_i64(299_792_458))),
        _ => None,
    }
}

pub(crate) fn split_labeled_formula(raw: &str) -> Option<(&str, &str)> {
    let t = raw.trim();
    let expr = t.strip_prefix('=')?;
    let (expr, label) = expr.rsplit_once(" -- ")?;
    let expr = expr.trim();
    let label = label.trim();
    if expr.is_empty() || label.is_empty() {
        return None;
    }
    Some((expr, label))
}

fn render_addr(addr: &CellAddr, locks: &A1RefLocks) -> String {
    formula_cell_ref_text(addr, 0, *locks)
}

fn render_ast(ast: &Ast) -> String {
    match ast {
        Ast::Number(n) => n.to_formula_string(),
        Ast::Text(s) => format!("\"{}\"", s.replace('"', "\"\"")),
        Ast::Name(name) => name.clone(),
        Ast::Ref { addr, locks } => render_addr(addr, locks),
        Ast::SheetRef {
            sheet_id,
            addr,
            locks,
        } => format!("#{sheet_id}!{}", render_addr(addr, locks)),
        Ast::Range {
            range,
            locks_tl,
            locks_br,
        } => format!(
            "{}:{}",
            render_addr(
                &CellAddr::Main {
                    row: range.row_start,
                    col: range.col_start,
                },
                locks_tl,
            ),
            render_addr(
                &CellAddr::Main {
                    row: range.row_end.saturating_sub(1),
                    col: range.col_end.saturating_sub(1),
                },
                locks_br,
            ),
        ),
        Ast::Neg(a) => format!("(-{})", render_ast(a)),
        Ast::Add(a, b) => format!("({}+{})", render_ast(a), render_ast(b)),
        Ast::Sub(a, b) => format!("({}-{})", render_ast(a), render_ast(b)),
        Ast::Mul(a, b) => format!("({}*{})", render_ast(a), render_ast(b)),
        Ast::Div(a, b) => format!("({}/{})", render_ast(a), render_ast(b)),
        Ast::Pow(a, b) => format!("({}^{})", render_ast(a), render_ast(b)),
        Ast::Concat(a, b) => format!("({}&{})", render_ast(a), render_ast(b)),
        Ast::Call { name, args } => format!(
            "{}({})",
            name,
            args.iter().map(render_ast).collect::<Vec<_>>().join(",")
        ),
    }
}

fn translate_addr(addr: &CellAddr, locks: &A1RefLocks, ctx: &FormulaCopyContext) -> Option<CellAddr> {
    match addr {
        CellAddr::Header { .. } | CellAddr::Footer { .. } => Some(addr.clone()),
        CellAddr::Main { row, col } => Some(CellAddr::Main {
            row: if locks.row_absolute {
                *row
            } else {
                *ctx.row_map.get(*row as usize)?
            },
            col: *col,
        }),
        CellAddr::Left { col, row } => Some(CellAddr::Left {
            col: *col,
            row: if locks.row_absolute {
                *row
            } else {
                *ctx.row_map.get(*row as usize)?
            },
        }),
        CellAddr::Right { col, row } => Some(CellAddr::Right {
            col: *col,
            row: if locks.row_absolute {
                *row
            } else {
                *ctx.row_map.get(*row as usize)?
            },
        }),
    }
}

fn translate_range(
    range: &MainRange,
    locks_tl: &A1RefLocks,
    locks_br: &A1RefLocks,
    ctx: &FormulaCopyContext,
) -> Option<MainRange> {
    let tl = translate_addr(
        &CellAddr::Main {
            row: range.row_start,
            col: range.col_start,
        },
        locks_tl,
        ctx,
    )?;
    let br = translate_addr(
        &CellAddr::Main {
            row: range.row_end.saturating_sub(1),
            col: range.col_end.saturating_sub(1),
        },
        locks_br,
        ctx,
    )?;
    let (r1, c1, r2, c2) = match (tl, br) {
        (
            CellAddr::Main {
                row: r1,
                col: c1,
            },
            CellAddr::Main {
                row: r2,
                col: c2,
            },
        ) => (r1, c1, r2, c2),
        _ => return None,
    };
    let row_start = r1.min(r2);
    let row_end = r1.max(r2).saturating_add(1);
    let col_start = c1.min(c2);
    let col_end = c1.max(c2).saturating_add(1);
    Some(MainRange {
        row_start,
        row_end,
        col_start,
        col_end,
    })
}

/// Shift A1 cell references in an interop `=…` string by `(row_delta, col_delta)` (main-cell units).
/// Used for generic export so the pasted file’s top-left is Excel A1. On parse failure, returns `s`.
pub fn rebase_interop_formula_row_col(s: &str, row_delta: i32, col_delta: i32, main_cols: usize) -> String {
    if row_delta == 0 && col_delta == 0 {
        return s.to_string();
    }
    translate_formula_text_by_offset(s, row_delta, col_delta, main_cols).unwrap_or_else(|| s.to_string())
}

pub fn translate_formula_text_by_offset(
    raw: &str,
    row_delta: i32,
    col_delta: i32,
    main_cols: usize,
) -> Option<String> {
    if row_delta == 0 && col_delta == 0 {
        return Some(raw.trim().to_string());
    }
    let t = raw.trim();
    let (expr_to_parse, label_opt) = if let Some((e, l)) = split_labeled_formula(t) {
        (format!("={}", e.trim()), Some(l.to_string()))
    } else {
        (t.to_string(), None)
    };
    if !expr_to_parse.starts_with('=') {
        return None;
    }
    let mut parser = Parser {
        s: &expr_to_parse[1..],
        i: 0,
        main_cols: 0,
    };
    let ast = parser.parse_expr().ok()?;
    parser.skip_ws();
    if parser.i != parser.s.len() {
        return None;
    }
    let translated = translate_ast_by_offset(&ast, row_delta, col_delta, main_cols)?;
    let mut out = format!("={}", render_ast(&translated));
    if let Some(ref label) = label_opt {
        out.push_str(" -- ");
        out.push_str(label);
    }
    Some(out)
}

/// Like Excel row insert: relative main‑band row refs at or below `gap_start` shift by `delta_rows`
/// (positive = rows pushed down). Absolute `$` row locks are preserved.
/// `translate_qualified_same_sheet_refs`: When `Some(id)`, row refs inside `id!…` qualifiers are
/// updated; when `None`, only unqualified refs are shifted (foreign sheet qualifiers unchanged).
pub fn translate_formula_text_insert_main_rows(
    raw: &str,
    main_cols: usize,
    gap_start: u32,
    delta_rows: i32,
    translate_qualified_same_sheet_refs: Option<u32>,
) -> Option<String> {
    if delta_rows == 0 {
        return Some(raw.trim().to_string());
    }
    let t = raw.trim();
    let (expr_to_parse, label_opt) = if let Some((e, l)) = split_labeled_formula(t) {
        (format!("={}", e.trim()), Some(l.to_string()))
    } else {
        (t.to_string(), None)
    };
    if !expr_to_parse.starts_with('=') {
        return None;
    }
    let mut parser = Parser {
        s: &expr_to_parse[1..],
        i: 0,
        main_cols,
    };
    let ast = parser.parse_expr().ok()?;
    parser.skip_ws();
    if parser.i != parser.s.len() {
        return None;
    }
    let translated =
        translate_ast_insert_main_rows(&ast, gap_start, delta_rows, translate_qualified_same_sheet_refs)?;
    let mut out = format!("={}", render_ast(&translated));
    if let Some(ref label) = label_opt {
        out.push_str(" -- ");
        out.push_str(label);
    }
    Some(out)
}

/// Like Excel column insert: relative main‑band column refs at or right of `gap_start` shift by
/// `delta_cols` (positive = columns pushed right). Absolute `$` column locks are preserved.
/// See [`translate_formula_text_insert_main_rows`] for `translate_qualified_same_sheet_refs`.
pub fn translate_formula_text_insert_main_cols(
    raw: &str,
    main_cols: usize,
    gap_start: u32,
    delta_cols: i32,
    translate_qualified_same_sheet_refs: Option<u32>,
) -> Option<String> {
    if delta_cols == 0 {
        return Some(raw.trim().to_string());
    }
    let t = raw.trim();
    let (expr_to_parse, label_opt) = if let Some((e, l)) = split_labeled_formula(t) {
        (format!("={}", e.trim()), Some(l.to_string()))
    } else {
        (t.to_string(), None)
    };
    if !expr_to_parse.starts_with('=') {
        return None;
    }
    let mut parser = Parser {
        s: &expr_to_parse[1..],
        i: 0,
        main_cols,
    };
    let ast = parser.parse_expr().ok()?;
    parser.skip_ws();
    if parser.i != parser.s.len() {
        return None;
    }
    let translated = translate_ast_insert_main_cols(
        &ast,
        gap_start,
        delta_cols,
        main_cols,
        translate_qualified_same_sheet_refs,
    )?;
    let mut out = format!("={}", render_ast(&translated));
    if let Some(ref label) = label_opt {
        out.push_str(" -- ");
        out.push_str(label);
    }
    Some(out)
}

/// After creating an insert gap for main rows starting at `gap_start` with height `gap_len`,
/// update every stored `=…` cell on the grid (Excel-style).
pub fn repair_all_formulas_after_main_row_insert(
    grid: &mut Grid,
    main_cols: usize,
    gap_start: u32,
    gap_len: u32,
    translate_qualified_same_sheet_refs: Option<u32>,
) {
    if gap_len == 0 {
        return;
    }
    let delta = gap_len as i32;
    let updates: Vec<(CellAddr, String)> = grid
        .iter_nonempty()
        .filter_map(|(addr, raw)| {
            if !raw.trim_start().starts_with('=') {
                return None;
            }
            let new_s = translate_formula_text_insert_main_rows(
                &raw,
                main_cols,
                gap_start,
                delta,
                translate_qualified_same_sheet_refs,
            )?;
            (new_s != raw).then_some((addr, new_s))
        })
        .collect();
    for (addr, s) in updates {
        grid.set(&addr, s);
    }
}

/// After creating an insert gap for main columns starting at `gap_start` with width `gap_len`,
/// update every stored `=…` cell on the grid (Excel-style).
pub fn repair_all_formulas_after_main_col_insert(
    grid: &mut Grid,
    main_cols: usize,
    gap_start: u32,
    gap_len: u32,
    translate_qualified_same_sheet_refs: Option<u32>,
) {
    if gap_len == 0 {
        return;
    }
    let delta = gap_len as i32;
    let updates: Vec<(CellAddr, String)> = grid
        .iter_nonempty()
        .filter_map(|(addr, raw)| {
            if !raw.trim_start().starts_with('=') {
                return None;
            }
            let new_s = translate_formula_text_insert_main_cols(
                &raw,
                main_cols,
                gap_start,
                delta,
                translate_qualified_same_sheet_refs,
            )?;
            (new_s != raw).then_some((addr, new_s))
        })
        .collect();
    for (addr, s) in updates {
        grid.set(&addr, s);
    }
}

fn shift_if_unlocked_row(row: u32, locks: &A1RefLocks, gap_start: u32, delta: i32) -> Option<u32> {
    if locks.row_absolute || row < gap_start {
        return Some(row);
    }
    let r = row as i64 + delta as i64;
    if r < 0 || r > u32::MAX as i64 {
        None
    } else {
        Some(r as u32)
    }
}

fn shift_if_unlocked_col(col: u32, locks: &A1RefLocks, gap_start: u32, delta: i32) -> Option<u32> {
    if locks.col_absolute || col < gap_start {
        return Some(col);
    }
    let c = col as i64 + delta as i64;
    if c < 0 || c > u32::MAX as i64 {
        None
    } else {
        Some(c as u32)
    }
}

fn translate_cell_addr_insert_main_rows(
    addr: &CellAddr,
    locks: &A1RefLocks,
    gap_start: u32,
    delta: i32,
) -> Option<CellAddr> {
    Some(match addr {
        CellAddr::Main { row, col } => CellAddr::Main {
            row: shift_if_unlocked_row(*row, locks, gap_start, delta)?,
            col: *col,
        },
        CellAddr::Left { col, row } => CellAddr::Left {
            col: *col,
            row: shift_if_unlocked_row(*row, locks, gap_start, delta)?,
        },
        CellAddr::Right { col, row } => CellAddr::Right {
            col: *col,
            row: shift_if_unlocked_row(*row, locks, gap_start, delta)?,
        },
        CellAddr::Header { .. } | CellAddr::Footer { .. } => addr.clone(),
    })
}

fn translate_cell_addr_insert_main_cols(
    addr: &CellAddr,
    locks: &A1RefLocks,
    gap_start: u32,
    delta: i32,
    main_cols: usize,
) -> Option<CellAddr> {
    Some(match addr {
        CellAddr::Main { row, col } => CellAddr::Main {
            row: *row,
            col: shift_if_unlocked_col(*col, locks, gap_start, delta)?,
        },
        CellAddr::Header { row, col } => {
            let gc = col.to_global(main_cols);
            if gc >= MARGIN_COLS && gc < MARGIN_COLS + main_cols {
                let mc = (gc - MARGIN_COLS) as u32;
                let nmc = shift_if_unlocked_col(mc, locks, gap_start, delta)?;
                CellAddr::Header {
                    row: *row,
                    col: ColumnAddr::from_global(MARGIN_COLS + nmc as usize, main_cols),
                }
            } else {
                addr.clone()
            }
        }
        CellAddr::Footer { row, col } => {
            let gc = col.to_global(main_cols);
            if gc >= MARGIN_COLS && gc < MARGIN_COLS + main_cols {
                let mc = (gc - MARGIN_COLS) as u32;
                let nmc = shift_if_unlocked_col(mc, locks, gap_start, delta)?;
                CellAddr::Footer {
                    row: *row,
                    col: ColumnAddr::from_global(MARGIN_COLS + nmc as usize, main_cols),
                }
            } else {
                addr.clone()
            }
        }
        CellAddr::Left { .. } | CellAddr::Right { .. } => addr.clone(),
    })
}

fn translate_range_insert_main_rows(
    range: &MainRange,
    locks_tl: A1RefLocks,
    locks_br: A1RefLocks,
    gap_start: u32,
    delta: i32,
) -> Option<(MainRange, A1RefLocks, A1RefLocks)> {
    let tl = translate_cell_addr_insert_main_rows(
        &CellAddr::Main {
            row: range.row_start,
            col: range.col_start,
        },
        &locks_tl,
        gap_start,
        delta,
    )?;
    let br = translate_cell_addr_insert_main_rows(
        &CellAddr::Main {
            row: range.row_end.saturating_sub(1),
            col: range.col_end.saturating_sub(1),
        },
        &locks_br,
        gap_start,
        delta,
    )?;
    let (r1, c1, r2, c2) = match (tl, br) {
        (
            CellAddr::Main {
                row: r1,
                col: c1,
            },
            CellAddr::Main {
                row: r2,
                col: c2,
            },
        ) => (r1, c1, r2, c2),
        _ => return None,
    };
    let row_start = r1.min(r2);
    let row_end = r1.max(r2).saturating_add(1);
    let col_start = c1.min(c2);
    let col_end = c1.max(c2).saturating_add(1);
    let (out_tl, out_br) = corner_locks_for_bbox(r1, c1, locks_tl, r2, c2, locks_br);
    Some((
        MainRange {
            row_start,
            row_end,
            col_start,
            col_end,
        },
        out_tl,
        out_br,
    ))
}

fn translate_range_insert_main_cols(
    range: &MainRange,
    locks_tl: A1RefLocks,
    locks_br: A1RefLocks,
    gap_start: u32,
    delta: i32,
    main_cols: usize,
) -> Option<(MainRange, A1RefLocks, A1RefLocks)> {
    let tl = translate_cell_addr_insert_main_cols(
        &CellAddr::Main {
            row: range.row_start,
            col: range.col_start,
        },
        &locks_tl,
        gap_start,
        delta,
        main_cols,
    )?;
    let br = translate_cell_addr_insert_main_cols(
        &CellAddr::Main {
            row: range.row_end.saturating_sub(1),
            col: range.col_end.saturating_sub(1),
        },
        &locks_br,
        gap_start,
        delta,
        main_cols,
    )?;
    let (r1, c1, r2, c2) = match (tl, br) {
        (
            CellAddr::Main {
                row: r1,
                col: c1,
            },
            CellAddr::Main {
                row: r2,
                col: c2,
            },
        ) => (r1, c1, r2, c2),
        _ => return None,
    };
    let row_start = r1.min(r2);
    let row_end = r1.max(r2).saturating_add(1);
    let col_start = c1.min(c2);
    let col_end = c1.max(c2).saturating_add(1);
    let (out_tl, out_br) = corner_locks_for_bbox(r1, c1, locks_tl, r2, c2, locks_br);
    Some((
        MainRange {
            row_start,
            row_end,
            col_start,
            col_end,
        },
        out_tl,
        out_br,
    ))
}

fn translate_ast_insert_main_rows(
    ast: &Ast,
    gap_start: u32,
    delta: i32,
    translate_qualified_same_sheet_refs: Option<u32>,
) -> Option<Ast> {
    Some(match ast {
        Ast::Number(n) => Ast::Number(n.clone()),
        Ast::Text(s) => Ast::Text(s.clone()),
        Ast::Name(name) => Ast::Name(name.clone()),
        Ast::Ref { addr, locks } => Ast::Ref {
            addr: translate_cell_addr_insert_main_rows(addr, locks, gap_start, delta)?,
            locks: *locks,
        },
        Ast::SheetRef {
            sheet_id,
            addr,
            locks,
        } => {
            let new_addr =
                if translate_qualified_same_sheet_refs.map_or(false, |id| id == *sheet_id) {
                    translate_cell_addr_insert_main_rows(addr, locks, gap_start, delta)?
                } else {
                    addr.clone()
                };
            Ast::SheetRef {
                sheet_id: *sheet_id,
                addr: new_addr,
                locks: *locks,
            }
        }
        Ast::Range {
            range,
            locks_tl,
            locks_br,
        } => {
            let (rng, otl, obr) =
                translate_range_insert_main_rows(range, *locks_tl, *locks_br, gap_start, delta)?;
            Ast::Range {
                range: rng,
                locks_tl: otl,
                locks_br: obr,
            }
        }
        Ast::Neg(a) => Ast::Neg(Box::new(translate_ast_insert_main_rows(
            a,
            gap_start,
            delta,
            translate_qualified_same_sheet_refs,
        )?)),
        Ast::Add(a, b) => Ast::Add(
            Box::new(translate_ast_insert_main_rows(
                a,
                gap_start,
                delta,
                translate_qualified_same_sheet_refs,
            )?),
            Box::new(translate_ast_insert_main_rows(
                b,
                gap_start,
                delta,
                translate_qualified_same_sheet_refs,
            )?),
        ),
        Ast::Sub(a, b) => Ast::Sub(
            Box::new(translate_ast_insert_main_rows(
                a,
                gap_start,
                delta,
                translate_qualified_same_sheet_refs,
            )?),
            Box::new(translate_ast_insert_main_rows(
                b,
                gap_start,
                delta,
                translate_qualified_same_sheet_refs,
            )?),
        ),
        Ast::Mul(a, b) => Ast::Mul(
            Box::new(translate_ast_insert_main_rows(
                a,
                gap_start,
                delta,
                translate_qualified_same_sheet_refs,
            )?),
            Box::new(translate_ast_insert_main_rows(
                b,
                gap_start,
                delta,
                translate_qualified_same_sheet_refs,
            )?),
        ),
        Ast::Div(a, b) => Ast::Div(
            Box::new(translate_ast_insert_main_rows(
                a,
                gap_start,
                delta,
                translate_qualified_same_sheet_refs,
            )?),
            Box::new(translate_ast_insert_main_rows(
                b,
                gap_start,
                delta,
                translate_qualified_same_sheet_refs,
            )?),
        ),
        Ast::Pow(a, b) => Ast::Pow(
            Box::new(translate_ast_insert_main_rows(
                a,
                gap_start,
                delta,
                translate_qualified_same_sheet_refs,
            )?),
            Box::new(translate_ast_insert_main_rows(
                b,
                gap_start,
                delta,
                translate_qualified_same_sheet_refs,
            )?),
        ),
        Ast::Concat(a, b) => Ast::Concat(
            Box::new(translate_ast_insert_main_rows(
                a,
                gap_start,
                delta,
                translate_qualified_same_sheet_refs,
            )?),
            Box::new(translate_ast_insert_main_rows(
                b,
                gap_start,
                delta,
                translate_qualified_same_sheet_refs,
            )?),
        ),
        Ast::Call { name, args } => Ast::Call {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| {
                    translate_ast_insert_main_rows(
                        a,
                        gap_start,
                        delta,
                        translate_qualified_same_sheet_refs,
                    )
                })
                .collect::<Option<Vec<_>>>()?,
        },
    })
}

fn translate_ast_insert_main_cols(
    ast: &Ast,
    gap_start: u32,
    delta: i32,
    main_cols: usize,
    translate_qualified_same_sheet_refs: Option<u32>,
) -> Option<Ast> {
    Some(match ast {
        Ast::Number(n) => Ast::Number(n.clone()),
        Ast::Text(s) => Ast::Text(s.clone()),
        Ast::Name(name) => Ast::Name(name.clone()),
        Ast::Ref { addr, locks } => Ast::Ref {
            addr: translate_cell_addr_insert_main_cols(addr, locks, gap_start, delta, main_cols)?,
            locks: *locks,
        },
        Ast::SheetRef {
            sheet_id,
            addr,
            locks,
        } => {
            let new_addr =
                if translate_qualified_same_sheet_refs.map_or(false, |id| id == *sheet_id) {
                    translate_cell_addr_insert_main_cols(addr, locks, gap_start, delta, main_cols)?
                } else {
                    addr.clone()
                };
            Ast::SheetRef {
                sheet_id: *sheet_id,
                addr: new_addr,
                locks: *locks,
            }
        }
        Ast::Range {
            range,
            locks_tl,
            locks_br,
        } => {
            let (rng, otl, obr) =
                translate_range_insert_main_cols(range, *locks_tl, *locks_br, gap_start, delta, main_cols)?;
            Ast::Range {
                range: rng,
                locks_tl: otl,
                locks_br: obr,
            }
        }
        Ast::Neg(a) => Ast::Neg(Box::new(translate_ast_insert_main_cols(
            a,
            gap_start,
            delta,
            main_cols,
            translate_qualified_same_sheet_refs,
        )?)),
        Ast::Add(a, b) => Ast::Add(
            Box::new(translate_ast_insert_main_cols(
                a,
                gap_start,
                delta,
                main_cols,
                translate_qualified_same_sheet_refs,
            )?),
            Box::new(translate_ast_insert_main_cols(
                b,
                gap_start,
                delta,
                main_cols,
                translate_qualified_same_sheet_refs,
            )?),
        ),
        Ast::Sub(a, b) => Ast::Sub(
            Box::new(translate_ast_insert_main_cols(
                a,
                gap_start,
                delta,
                main_cols,
                translate_qualified_same_sheet_refs,
            )?),
            Box::new(translate_ast_insert_main_cols(
                b,
                gap_start,
                delta,
                main_cols,
                translate_qualified_same_sheet_refs,
            )?),
        ),
        Ast::Mul(a, b) => Ast::Mul(
            Box::new(translate_ast_insert_main_cols(
                a,
                gap_start,
                delta,
                main_cols,
                translate_qualified_same_sheet_refs,
            )?),
            Box::new(translate_ast_insert_main_cols(
                b,
                gap_start,
                delta,
                main_cols,
                translate_qualified_same_sheet_refs,
            )?),
        ),
        Ast::Div(a, b) => Ast::Div(
            Box::new(translate_ast_insert_main_cols(
                a,
                gap_start,
                delta,
                main_cols,
                translate_qualified_same_sheet_refs,
            )?),
            Box::new(translate_ast_insert_main_cols(
                b,
                gap_start,
                delta,
                main_cols,
                translate_qualified_same_sheet_refs,
            )?),
        ),
        Ast::Pow(a, b) => Ast::Pow(
            Box::new(translate_ast_insert_main_cols(
                a,
                gap_start,
                delta,
                main_cols,
                translate_qualified_same_sheet_refs,
            )?),
            Box::new(translate_ast_insert_main_cols(
                b,
                gap_start,
                delta,
                main_cols,
                translate_qualified_same_sheet_refs,
            )?),
        ),
        Ast::Concat(a, b) => Ast::Concat(
            Box::new(translate_ast_insert_main_cols(
                a,
                gap_start,
                delta,
                main_cols,
                translate_qualified_same_sheet_refs,
            )?),
            Box::new(translate_ast_insert_main_cols(
                b,
                gap_start,
                delta,
                main_cols,
                translate_qualified_same_sheet_refs,
            )?),
        ),
        Ast::Call { name, args } => Ast::Call {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| {
                    translate_ast_insert_main_cols(
                        a,
                        gap_start,
                        delta,
                        main_cols,
                        translate_qualified_same_sheet_refs,
                    )
                })
                .collect::<Option<Vec<_>>>()?,
        },
    })
}

fn translate_cell_addr_by_offset(
    addr: &CellAddr,
    row_delta: i32,
    col_delta: i32,
    locks: &A1RefLocks,
    main_cols: usize,
) -> Option<CellAddr> {
    let shift_u32 = |v: u32, delta: i32| -> Option<u32> {
        if delta >= 0 {
            v.checked_add(delta as u32)
        } else {
            v.checked_sub(delta.unsigned_abs())
        }
    };
    match addr {
        CellAddr::Header { row, col } => {
            let gc = col.to_global(main_cols);
            Some(CellAddr::Header {
                row: shift_u32(*row, row_delta)?,
                col: ColumnAddr::from_global(shift_u32(gc as u32, col_delta)? as usize, main_cols),
            })
        }
        CellAddr::Footer { row, col } => {
            let gc = col.to_global(main_cols);
            Some(CellAddr::Footer {
                row: shift_u32(*row, row_delta)?,
                col: ColumnAddr::from_global(shift_u32(gc as u32, col_delta)? as usize, main_cols),
            })
        }
        CellAddr::Main { row, col } => Some(CellAddr::Main {
            row: if locks.row_absolute {
                *row
            } else {
                shift_u32(*row, row_delta)?
            },
            col: if locks.col_absolute {
                *col
            } else {
                shift_u32(*col, col_delta)?
            },
        }),
        CellAddr::Left { col, row } => Some(CellAddr::Left {
            col: shift_u32(*col as u32, col_delta)? as usize,
            row: shift_u32(*row, row_delta)?,
        }),
        CellAddr::Right { col, row } => Some(CellAddr::Right {
            col: shift_u32(*col as u32, col_delta)? as usize,
            row: shift_u32(*row, row_delta)?,
        }),
    }
}

fn translate_range_by_offset(
    range: &MainRange,
    locks_tl: A1RefLocks,
    locks_br: A1RefLocks,
    row_delta: i32,
    col_delta: i32,
    main_cols: usize,
) -> Option<(MainRange, A1RefLocks, A1RefLocks)> {
    let tl = translate_cell_addr_by_offset(
        &CellAddr::Main {
            row: range.row_start,
            col: range.col_start,
        },
        row_delta,
        col_delta,
        &locks_tl,
        main_cols,
    )?;
    let br = translate_cell_addr_by_offset(
        &CellAddr::Main {
            row: range.row_end.saturating_sub(1),
            col: range.col_end.saturating_sub(1),
        },
        row_delta,
        col_delta,
        &locks_br,
        main_cols,
    )?;
    let (r1, c1, r2, c2) = match (tl, br) {
        (
            CellAddr::Main {
                row: r1,
                col: c1,
            },
            CellAddr::Main {
                row: r2,
                col: c2,
            },
        ) => (r1, c1, r2, c2),
        _ => return None,
    };
    let row_start = r1.min(r2);
    let row_end = r1.max(r2).saturating_add(1);
    let col_start = c1.min(c2);
    let col_end = c1.max(c2).saturating_add(1);
    let (out_tl, out_br) = corner_locks_for_bbox(r1, c1, locks_tl, r2, c2, locks_br);
    Some((
        MainRange {
            row_start,
            row_end,
            col_start,
            col_end,
        },
        out_tl,
        out_br,
    ))
}

fn translate_ast_by_offset(ast: &Ast, row_delta: i32, col_delta: i32, main_cols: usize) -> Option<Ast> {
    Some(match ast {
        Ast::Number(n) => Ast::Number(n.clone()),
        Ast::Text(s) => Ast::Text(s.clone()),
        Ast::Name(name) => Ast::Name(name.clone()),
        Ast::Ref { addr, locks } => Ast::Ref {
            addr: translate_cell_addr_by_offset(addr, row_delta, col_delta, locks, main_cols)?,
            locks: *locks,
        },
        Ast::SheetRef {
            sheet_id,
            addr,
            locks,
        } => Ast::SheetRef {
            sheet_id: *sheet_id,
            addr: translate_cell_addr_by_offset(addr, row_delta, col_delta, locks, main_cols)?,
            locks: *locks,
        },
        Ast::Range {
            range,
            locks_tl,
            locks_br,
        } => {
            let (rng, otl, obr) =
                translate_range_by_offset(range, *locks_tl, *locks_br, row_delta, col_delta, main_cols)?;
            Ast::Range {
                range: rng,
                locks_tl: otl,
                locks_br: obr,
            }
        }
        Ast::Neg(a) => Ast::Neg(Box::new(translate_ast_by_offset(a, row_delta, col_delta, main_cols)?)),
        Ast::Add(a, b) => Ast::Add(
            Box::new(translate_ast_by_offset(a, row_delta, col_delta, main_cols)?),
            Box::new(translate_ast_by_offset(b, row_delta, col_delta, main_cols)?),
        ),
        Ast::Sub(a, b) => Ast::Sub(
            Box::new(translate_ast_by_offset(a, row_delta, col_delta, main_cols)?),
            Box::new(translate_ast_by_offset(b, row_delta, col_delta, main_cols)?),
        ),
        Ast::Mul(a, b) => Ast::Mul(
            Box::new(translate_ast_by_offset(a, row_delta, col_delta, main_cols)?),
            Box::new(translate_ast_by_offset(b, row_delta, col_delta, main_cols)?),
        ),
        Ast::Div(a, b) => Ast::Div(
            Box::new(translate_ast_by_offset(a, row_delta, col_delta, main_cols)?),
            Box::new(translate_ast_by_offset(b, row_delta, col_delta, main_cols)?),
        ),
        Ast::Pow(a, b) => Ast::Pow(
            Box::new(translate_ast_by_offset(a, row_delta, col_delta, main_cols)?),
            Box::new(translate_ast_by_offset(b, row_delta, col_delta, main_cols)?),
        ),
        Ast::Concat(a, b) => Ast::Concat(
            Box::new(translate_ast_by_offset(a, row_delta, col_delta, main_cols)?),
            Box::new(translate_ast_by_offset(b, row_delta, col_delta, main_cols)?),
        ),
        Ast::Call { name, args } => Ast::Call {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| translate_ast_by_offset(a, row_delta, col_delta, main_cols))
                .collect::<Option<Vec<_>>>()?,
        },
    })
}

fn translate_ast(ast: &Ast, ctx: &FormulaCopyContext) -> Option<Ast> {
    Some(match ast {
        Ast::Number(n) => Ast::Number(n.clone()),
        Ast::Text(s) => Ast::Text(s.clone()),
        Ast::Name(name) => Ast::Name(name.clone()),
        Ast::Ref { addr, locks } => Ast::Ref {
            addr: translate_addr(addr, locks, ctx)?,
            locks: *locks,
        },
        Ast::SheetRef {
            sheet_id,
            addr,
            locks,
        } => {
            if *sheet_id == ctx.source_sheet_id {
                Ast::Ref {
                    addr: translate_addr(addr, locks, ctx)?,
                    locks: *locks,
                }
            } else {
                Ast::SheetRef {
                    sheet_id: *sheet_id,
                    addr: addr.clone(),
                    locks: *locks,
                }
            }
        }
        Ast::Range {
            range,
            locks_tl,
            locks_br,
        } => Ast::Range {
            range: translate_range(range, locks_tl, locks_br, ctx)?,
            locks_tl: *locks_tl,
            locks_br: *locks_br,
        },
        Ast::Neg(a) => Ast::Neg(Box::new(translate_ast(a, ctx)?)),
        Ast::Add(a, b) => Ast::Add(
            Box::new(translate_ast(a, ctx)?),
            Box::new(translate_ast(b, ctx)?),
        ),
        Ast::Sub(a, b) => Ast::Sub(
            Box::new(translate_ast(a, ctx)?),
            Box::new(translate_ast(b, ctx)?),
        ),
        Ast::Mul(a, b) => Ast::Mul(
            Box::new(translate_ast(a, ctx)?),
            Box::new(translate_ast(b, ctx)?),
        ),
        Ast::Div(a, b) => Ast::Div(
            Box::new(translate_ast(a, ctx)?),
            Box::new(translate_ast(b, ctx)?),
        ),
        Ast::Pow(a, b) => Ast::Pow(
            Box::new(translate_ast(a, ctx)?),
            Box::new(translate_ast(b, ctx)?),
        ),
        Ast::Concat(a, b) => Ast::Concat(
            Box::new(translate_ast(a, ctx)?),
            Box::new(translate_ast(b, ctx)?),
        ),
        Ast::Call { name, args } => Ast::Call {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| translate_ast(a, ctx))
                .collect::<Option<Vec<_>>>()?,
        },
    })
}

pub fn translate_formula_text(raw: &str, ctx: &FormulaCopyContext) -> Option<String> {
    let t = raw.trim();
    let (expr, label) =
        split_labeled_formula(t).map_or((t, None), |(expr, label)| (expr, Some(label)));
    if !expr.starts_with('=') {
        return None;
    }
    let mut parser = Parser {
        s: &expr[1..],
        i: 0,
        main_cols: ctx.main_cols,
    };
    let ast = parser.parse_expr().ok()?;
    parser.skip_ws();
    if parser.i != parser.s.len() {
        return None;
    }
    let translated = translate_ast(&ast, ctx)?;
    let mut out = format!("={}", render_ast(&translated));
    if let Some(label) = label {
        out.push_str(" -- ");
        out.push_str(label);
    }
    Some(out)
}

fn rewrite_header_template(expr: &str, row: u32) -> String {
    let mut out = String::new();
    let bytes = expr.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphabetic() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
            let token = &expr[start..i];
            if i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'(') {
                out.push_str(token);
            } else {
                out.push_str(&token.to_ascii_uppercase());
                out.push_str(&(row + 1).to_string());
            }
        } else {
            out.push(b as char);
            i += 1;
        }
    }
    out
}

fn rewrite_row_template(expr: &str, col: usize) -> String {
    let mut out = String::new();
    let mut chars = expr.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == ':' {
            let mut digits = String::new();
            while matches!(chars.peek(), Some(c) if c.is_ascii_digit()) {
                digits.push(chars.next().unwrap());
            }
            if digits.is_empty() {
                out.push(ch);
            } else {
                out.push_str(&excel_column_name(col));
                out.push_str(&digits);
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn control_formula_expr(grid: &Grid, addr: &CellAddr) -> Option<String> {
    let raw_owned = grid.get(addr);
    let raw = raw_owned.as_deref()?;
    if let Some((expr, _label)) = split_labeled_formula(raw) {
        return Some(expr.to_string());
    }
    // ODS generic historically stored only `of:` (=…) in the cell; round-trip lost ` -- LABEL`.
    let raw_trim = raw.trim();
    let after_first_eq = raw_trim.strip_prefix('=')?;
    if after_first_eq.starts_with('=') {
        // `==…` aggregate row markers are not spreadsheet expressions here.
        return None;
    }
    let t = after_first_eq;
    if t.contains(" -- ") {
        return None;
    }
    Some(t.to_string())
}

fn margin_control_label_after_double_equals(t: &str) -> Option<String> {
    crate::ops::margin_key_agg_func(t)?;
    let kw = t.strip_prefix("==").map(str::trim).unwrap_or_default();
    match crate::ops::margin_key_agg_func(t)? {
        AggFunc::Sum => {
            Some(if kw.eq_ignore_ascii_case("SUM") {
                "SUM".into()
            } else {
                "TOTAL".into()
            })
        }
        AggFunc::Mean => Some("MEAN".into()),
        AggFunc::Median => Some("MEDIAN".into()),
        AggFunc::Min => Some("MIN".into()),
        AggFunc::Max => Some("MAX".into()),
        AggFunc::Count => Some("COUNT".into()),
    }
}

fn control_formula_label(grid: &Grid, addr: &CellAddr) -> Option<String> {
    let raw_owned = grid.get(addr);
    let raw = raw_owned.as_deref()?;
    if matches!(
        addr,
        CellAddr::Header { .. }
            | CellAddr::Footer { .. }
            | CellAddr::Left { .. }
            | CellAddr::Right { .. }
    ) {
        let t = raw.trim();
        if t.starts_with("==") {
            if let Some(l) = margin_control_label_after_double_equals(t) {
                return Some(l);
            }
        }
        if t
            .strip_prefix('=')
            .is_some_and(|r| !r.starts_with('=') && r.eq_ignore_ascii_case("TOTAL"))
        {
            return Some("TOTAL".to_string());
        }
    }
    let (_expr, label) = split_labeled_formula(raw)?;
    Some(label.to_string())
}

fn templated_formula(grid: &Grid, addr: &CellAddr) -> Option<String> {
    let CellAddr::Main { row, col } = addr else {
        return None;
    };

    let header_addr = CellAddr::Header {
        row: (HEADER_ROWS - 1) as u32,
        col: ColumnAddr::from_global(MARGIN_COLS + *col as usize, grid.main_cols()),
    };
    // Avoid treating margin-aggregate directives (e.g. `=TOTAL`, `==SUM`) as
    // header templates. If the raw header text is an aggregate key, skip
    // templating here so aggregate semantics remain intact.
    if let Some(raw) = grid.get(&header_addr) {
        if crate::ops::margin_key_agg_func(&raw).is_none() {
            if let Some(expr) = control_formula_expr(grid, &header_addr) {
                return Some(format!("={}", rewrite_header_template(&expr, *row)));
            }
        }
    }

    // Also support header templates placed in the right margin. A right-margin
    // header cell immediately to the right of the main block (]A) maps to the
    // last main column, the next (]B) to the second-last, etc. This lets users
    // put a control formula like `=B` in a right-margin header (e.g. `]A~1`) and
    // have it act as a per-row template for the corresponding main column.
    let mc = grid.main_cols() as u32;
    if mc > 0 {
        // col is the 0-based main-column index; compute the right-margin index
        // that sits adjacent to that main column.
        if *col <= mc.saturating_sub(1) {
            let rmi = mc.saturating_sub(1).saturating_sub(*col);
            let right_header_col = (MARGIN_COLS as u32) + mc + rmi;
            let right_header_addr = CellAddr::Header {
                row: (HEADER_ROWS - 1) as u32,
                col: ColumnAddr::from_global(right_header_col as usize, grid.main_cols()),
            };
            if let Some(raw) = grid.get(&right_header_addr) {
                if crate::ops::margin_key_agg_func(&raw).is_none() {
                    if let Some(expr) = control_formula_expr(grid, &right_header_addr) {
                        return Some(format!("={}", rewrite_header_template(&expr, *row)));
                    }
                }
            }
        }
    }

    let left_addr = CellAddr::Left {
        col: MARGIN_COLS - 1,
        row: *row,
    };
    if let Some(expr) = control_formula_expr(grid, &left_addr) {
        return Some(format!("={}", rewrite_row_template(&expr, *col as usize)));
    }

    None
}

/// Public for export: `=A1*0.1` from header/left ` -- ` template (no ` -- ` in output).
pub fn export_templated_formula(grid: &Grid, addr: &CellAddr) -> Option<String> {
    templated_formula(grid, addr)
}

/// ` -- ` display label for a main column, from the column header's labeled formula (e.g. TAX from `=A*0.1 -- TAX`).
pub fn main_column_label_from_header(grid: &Grid, main_col: usize) -> Option<String> {
    let header_addr = CellAddr::Header {
        row: (HEADER_ROWS - 1) as u32,
        col: ColumnAddr::from_global(MARGIN_COLS + main_col, grid.main_cols()),
    };
    control_formula_label(grid, &header_addr)
}

/// True if the stored cell text is a spreadsheet formula (`=` prefix after trim). `==…`
/// margin aggregate directives are not formulas here.
pub fn is_formula(raw: &str) -> bool {
    let t = raw.trim_start();
    if t.starts_with("==") {
        return false;
    }
    t.starts_with('=')
}

/// Numeric value for aggregation: formulas evaluate to a number if possible; plain text uses exact/approx parse.
pub fn effective_numeric(
    grid: &Grid,
    addr: &CellAddr,
    visiting: &mut Vec<CellAddr>,
    budget: &mut usize,
) -> Option<Number> {
    let raw_owned = grid.get(addr);
    let raw = raw_owned.as_deref().unwrap_or("");
    let template_formula = templated_formula(grid, addr);
    if template_formula.is_none() && !is_formula(raw) {
        return functions::parse_numeric_or_date_literal(raw);
    }
    match eval_cell(grid, addr, visiting, budget) {
        EvalResult::Number(n) if !n.is_nan() => {
            if n.is_zeroish()
                && template_formula
                    .as_deref()
                    .is_some_and(|formula| formula_references_all_empty(grid, formula))
            {
                None
            } else {
                Some(n)
            }
        }
        EvalResult::Text(s) => functions::parse_numeric_or_date_literal(&s),
        EvalResult::Bool(_) => None,
        _ => None,
    }
}

/// Spreadsheet SUM-style numeric contribution: [`effective_numeric`] plus boolean values as exact `0`/`1`.
pub fn summable_numeric(
    grid: &Grid,
    addr: &CellAddr,
    visiting: &mut Vec<CellAddr>,
    budget: &mut usize,
) -> Option<Number> {
    if let Some(n) = effective_numeric(grid, addr, visiting, budget) {
        return Some(n);
    }
    let raw_owned = grid.get(addr);
    let raw = raw_owned.as_deref().unwrap_or("");
    let t = raw.trim();
    let template_formula = templated_formula(grid, addr);
    // Plain keywords (stored without `=`)
    if template_formula.is_none() && !is_formula(raw) {
        if t.eq_ignore_ascii_case("true") {
            return Some(Number::one());
        }
        if t.eq_ignore_ascii_case("false") {
            return Some(Number::exact_zero());
        }
        return None;
    }
    match eval_cell(grid, addr, visiting, budget).scalar_coerce() {
        EvalResult::Bool(b) => Some(number_from_bool(b)),
        _ => None,
    }
}

/// Evaluate a cell (handles `=...`); used for display and dependencies.
pub fn eval_cell(
    grid: &Grid,
    addr: &CellAddr,
    visiting: &mut Vec<CellAddr>,
    budget: &mut usize,
) -> EvalResult {
    eval_cell_inner(grid, addr, visiting, budget, true)
}

fn eval_cell_with_sheet(
    grid: &Grid,
    sheet_id: u32,
    addr: &CellAddr,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    // Guard overall evaluation recursion depth to avoid blowing the OS stack
    // from unexpected recursive paths. Use the same logical limit as the
    // visiting-stack guard.
    let _depth_guard = {
        let mut entered = false;
        EVAL_RECURSION_DEPTH.with(|d| {
            let cur = d.get();
            if cur >= MAX_VISIT_DEPTH {
                entered = false;
            } else {
                d.set(cur + 1);
                entered = true;
            }
        });
        // RAII guard that decrements on drop.
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                EVAL_RECURSION_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
            }
        }
        if !entered {
            return EvalResult::Error("LIMIT");
        }
        Guard
    };
    *budget = budget.saturating_sub(1);
    if *budget == 0 {
        return EvalResult::Error("LIMIT");
    }
    // Guard against too-deep evaluation stacks causing stack overflow.
    if SHEET_VISITING.with(|stack| stack.borrow().len() >= MAX_VISIT_DEPTH) {
        return EvalResult::Error("LIMIT");
    }
    let raw_owned = grid.get(addr);
    let raw = raw_owned.as_deref().unwrap_or("");
    let t = raw.trim();
    if t.is_empty() {
        return EvalResult::Number(Number::exact_zero());
    }
    if t.starts_with("==") {
        return EvalResult::Text(String::new());
    }
    if !t.starts_with('=') {
        return eval_plain_cell_raw(t);
    }

    // Check for circular reference across sheet-qualified visits using the
    // shared per-thread SHEET_VISITING stack.
    if SHEET_VISITING.with(|stack| stack.borrow().iter().any(|a| a.0 == sheet_id && &a.1 == addr)) {
        return EvalResult::Error("CIRC");
    }

    // Push this sheet/cell onto the global stack, evaluate, then pop. We do
    // not hold the RefMut across the recursive evaluation; we borrow_mut()
    // only to mutate and then drop so nested borrows are allowed.
    SHEET_VISITING.with(|stack| {
        stack.borrow_mut().push((sheet_id, addr.clone()));
    });
    let r = eval_expr_str(
        &t[1..],
        grid,
        &mut Vec::new(),
        bindings,
        budget,
        allow_templates,
    );
    SHEET_VISITING.with(|stack| {
        stack.borrow_mut().pop();
    });
    r
}

fn eval_cell_inner(
    grid: &Grid,
    addr: &CellAddr,
    visiting: &mut Vec<CellAddr>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    // Guard overall evaluation recursion depth to avoid blowing the OS stack
    // from unexpected recursive paths. Use the same logical limit as the
    // visiting-stack guard. This mirrors the check in eval_cell_with_sheet so
    // non-sheet-qualified evaluation paths are also protected.
    let _depth_guard = {
        let mut entered = false;
        EVAL_RECURSION_DEPTH.with(|d| {
            let cur = d.get();
            if cur >= MAX_VISIT_DEPTH {
                entered = false;
            } else {
                d.set(cur + 1);
                entered = true;
            }
        });
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                EVAL_RECURSION_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
            }
        }
        if !entered {
            return EvalResult::Error("LIMIT");
        }
        Guard
    };
    *budget = budget.saturating_sub(1);
    if *budget == 0 {
        return EvalResult::Error("LIMIT");
    }

    // Also guard the local visiting stack depth.
    if visiting.len() >= MAX_VISIT_DEPTH {
        return EvalResult::Error("LIMIT");
    }

    // If this grid is part of the current eval workbook, push a
    // sheet-qualified entry onto the shared SHEET_VISITING stack so
    // cross-sheet cycles are visible early (avoids deep recursion
    // before the sheet-visit detection fires). We only push when
    // the current grid can be mapped to a sheet id in the active
    // workbook. Use an RAII guard to ensure the stack is popped on
    // every exit path.
    struct SheetGuard {
        pushed: bool,
    }
    impl Drop for SheetGuard {
        fn drop(&mut self) {
            if self.pushed {
                SHEET_VISITING.with(|stack| {
                    stack.borrow_mut().pop();
                });
            }
        }
    }

    let mut _sheet_guard = SheetGuard { pushed: false };
    // Try to find a matching sheet id for this grid; if found, and the
    // exact (sheet_id, addr) pair is already on the stack, treat as
    // a circular reference. Otherwise push the pair so nested
    // sheet-qualified evaluations will see it.
    if let Some(sheet_id) = {
        // Lookup by scanning the workbook for a sheet whose stored
        // GridBox id matches this grid's id. This is a cheap linear
        // scan and only done when set_eval_context was used.
        EVAL_WORKBOOK.with(|wb| {
            wb.borrow()
                .as_ref()
                .and_then(|w| w.sheets.iter().find(|s| s.state.grid.id() == grid.id()).map(|s| s.id))
        })
    } {
        if SHEET_VISITING.with(|stack| stack.borrow().iter().any(|a| a.0 == sheet_id && &a.1 == addr)) {
            return EvalResult::Error("CIRC");
        }
        SHEET_VISITING.with(|stack| {
            stack.borrow_mut().push((sheet_id, addr.clone()));
        });
        _sheet_guard.pushed = true;
    }

    if allow_templates {
        if let Some(formula) = templated_formula(grid, addr) {
            // Ensure we mark this cell as being visited so cycles via templated
            // formulas are detected the same way as normal formulas.
            if visiting.iter().any(|a| a == addr) {
                return EvalResult::Error("CIRC");
            }
            visiting.push(addr.clone());
            let r = eval_expr_str(
                &formula[1..],
                grid,
                visiting,
                &mut Vec::new(),
                budget,
                allow_templates,
            );
            visiting.pop();
            return r;
        }
    }

    let raw_owned = grid.get(addr);
    let raw = raw_owned.as_deref().unwrap_or("");
    let t = raw.trim();
    if t.is_empty() {
        return EvalResult::Number(Number::exact_zero());
    }
    if t.starts_with("==") {
        return EvalResult::Text(String::new());
    }
    if !t.starts_with('=') {
        return eval_plain_cell_raw(t);
    }

    if let Some(expr) = control_formula_expr(grid, addr) {
        // Same cycle-detection handling as for normal `=` formulas: push the
        // current cell before evaluating the expression so a reference back
        // to this cell will be detected.
        if visiting.iter().any(|a| a == addr) {
            return EvalResult::Error("CIRC");
        }
        visiting.push(addr.clone());
        let r = eval_expr_str(&expr, grid, visiting, &mut Vec::new(), budget, allow_templates);
        visiting.pop();
        return r;
    }

    if visiting.iter().any(|a| a == addr) {
        return EvalResult::Error("CIRC");
    }

    visiting.push(addr.clone());
    let r = eval_expr_str(&t[1..], grid, visiting, &mut Vec::new(), budget, allow_templates);
    visiting.pop();
    r
}

fn eval_expr_str(
    expr: &str,
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    let mut p = Parser {
        s: expr.trim(),
        i: 0,
        main_cols: grid.main_cols(),
    };
    let ast = match p.parse_expr() {
        Ok(a) => a,
        Err(()) => return EvalResult::Error("PARSE"),
    };
    p.skip_ws();
    if p.i != p.s.len() {
        return EvalResult::Error("PARSE");
    }
    eval_ast(&ast, grid, visiting, bindings, budget, allow_templates)
}

#[derive(Clone, Debug)]
enum Ast {
    Number(Number),
    Text(String),
    Name(String),
    Ref {
        addr: CellAddr,
        locks: A1RefLocks,
    },
    SheetRef {
        sheet_id: u32,
        addr: CellAddr,
        locks: A1RefLocks,
    },
    /// Main grid only (`A1:B2`).
    Range {
        range: MainRange,
        locks_tl: A1RefLocks,
        locks_br: A1RefLocks,
    },
    Neg(Box<Ast>),
    Add(Box<Ast>, Box<Ast>),
    Sub(Box<Ast>, Box<Ast>),
    Mul(Box<Ast>, Box<Ast>),
    Div(Box<Ast>, Box<Ast>),
    Pow(Box<Ast>, Box<Ast>),
    Concat(Box<Ast>, Box<Ast>),
    Call {
        name: String,
        args: Vec<Ast>,
    },
}

struct Parser<'a> {
    s: &'a str,
    i: usize,
    main_cols: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if b.is_ascii_whitespace() {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    fn parse_expr(&mut self) -> Result<Ast, ()> {
        self.parse_concat()
    }

    /// Excel-style: `&` is looser than `+` / `-` (concat binds after add/sub).
    fn parse_concat(&mut self) -> Result<Ast, ()> {
        let mut left = self.parse_add_sub()?;
        loop {
            self.skip_ws();
            if self.peek() == Some(b'&') {
                self.i += 1;
                let right = self.parse_add_sub()?;
                left = Ast::Concat(Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_add_sub(&mut self) -> Result<Ast, ()> {
        let mut left = self.parse_mul_div()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'+') => {
                    self.i += 1;
                    let right = self.parse_mul_div()?;
                    left = Ast::Add(Box::new(left), Box::new(right));
                }
                Some(b'-') => {
                    self.i += 1;
                    let right = self.parse_mul_div()?;
                    left = Ast::Sub(Box::new(left), Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_mul_div(&mut self) -> Result<Ast, ()> {
        let mut left = self.parse_unary()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'*') => {
                    self.i += 1;
                    let right = self.parse_unary()?;
                    left = Ast::Mul(Box::new(left), Box::new(right));
                }
                Some(b'/') => {
                    self.i += 1;
                    let right = self.parse_unary()?;
                    left = Ast::Div(Box::new(left), Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Ast, ()> {
        self.skip_ws();
        if self.peek() == Some(b'-') {
            self.i += 1;
            let inner = self.parse_unary()?;
            return Ok(Ast::Neg(Box::new(inner)));
        }
        self.parse_power()
    }

    fn parse_power(&mut self) -> Result<Ast, ()> {
        let left = self.parse_primary()?;
        self.skip_ws();
        if self.peek() == Some(b'^') {
            self.i += 1;
            let right = self.parse_unary()?;
            return Ok(Ast::Pow(Box::new(left), Box::new(right)));
        }
        Ok(left)
    }

    fn parse_primary(&mut self) -> Result<Ast, ()> {
        self.skip_ws();
        let b = self.peek().ok_or(())?;

        if b == b'"' {
            self.i += 1;
            let mut out = String::new();
            while let Some(ch) = self.peek() {
                self.i += 1;
                if ch == b'"' {
                    if self.peek() == Some(b'"') {
                        out.push('"');
                        self.i += 1;
                    } else {
                        return Ok(Ast::Text(out));
                    }
                } else {
                    out.push(ch as char);
                }
            }
            return Err(());
        }

        if b == b'(' {
            self.i += 1;
            let e = self.parse_expr()?;
            self.skip_ws();
            if self.peek() != Some(b')') {
                return Err(());
            }
            self.i += 1;
            return Ok(e);
        }

        // Number
        if b.is_ascii_digit()
            || (b == b'.'
                && self
                    .s
                    .get(self.i + 1..)
                    .and_then(|r| r.as_bytes().first())
                    .map_or(false, |x| x.is_ascii_digit()))
        {
            return Ok(Ast::Number(self.parse_number()?));
        }

        let rest = &self.s[self.i..];

        if rest.starts_with('#') {
            if let Some((sheet_id, addr, locks, len)) = parse_sheet_qualified_ref(rest) {
                self.i += len;
                return Ok(Ast::SheetRef {
                    sheet_id,
                    addr,
                    locks,
                });
            }
            return Err(());
        }

        if let Some(ch) = self.s[self.i..].chars().next() {
            if ch == 'π' {
                self.i += ch.len_utf8();
                return Ok(Ast::Name(ch.to_string()));
            }
        }

        if rest.starts_with('$') {
            if let Some((sheet_id, addr, locks, len)) =
                parse_sheet_qualified_cell_ref_at_for_workbook(rest)
            {
                self.i += len;
                return Ok(Ast::SheetRef {
                    sheet_id,
                    addr,
                    locks,
                });
            }
            let bytes = rest.as_bytes();
            let mut j = 1usize;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j > 1 && j < bytes.len() && bytes[j] == b':' {
                let sheet = &rest[1..j];
                let after = &rest[j + 1..];
                if let Some((sheet_id, grid)) = workbook_lookup_sheet_ref(sheet) {
                    let (addr, locks, len) = parse_cell_ref_at(after, grid.main_cols()).ok_or(())?;
                    self.i += j + 1 + len;
                    return Ok(Ast::SheetRef {
                        sheet_id,
                        addr,
                        locks,
                    });
                }
            }
            if let Some((addr, locks, len)) = parse_cell_ref_at(rest, self.main_cols) {
                self.i += len;
                return Ok(Ast::Ref { addr, locks });
            }
            return Err(());
        }

        if rest.starts_with('[') || rest.starts_with(']') {
            let (addr, locks, len) = parse_cell_ref_at(rest, 0).ok_or(())?;
            self.i += len;
            return Ok(Ast::Ref { addr, locks });
        }

        // Letter: A1:B2, sum( … ), A1, or bare name.
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = self.i;
            while self
                .peek()
                .map(|x| x.is_ascii_alphanumeric() || x == b'_')
                .unwrap_or(false)
            {
                self.i += 1;
            }
            let token = &self.s[start..self.i];
            let rest = &self.s[start..];
            if let Some((range, locks_tl, locks_br, len)) = parse_main_range_formula_at(rest) {
                self.i = start + len;
                return Ok(Ast::Range {
                    range,
                    locks_tl,
                    locks_br,
                });
            }
            if self.peek() == Some(b'(') {
                let name = token.to_string();
                self.i += 1;
                let inner_end = self.scan_balanced_from(self.i)?;
                let inner = &self.s[self.i..inner_end];
                self.i = inner_end + 1;
                let args = if inner.trim().is_empty() {
                    Vec::new()
                } else {
                    split_top_level_args(inner)?
                };
                let mut arg_asts = Vec::with_capacity(args.len());
                for a in args {
                    let mut sub = Parser {
                        s: a.trim(),
                        i: 0,
                        main_cols: self.main_cols,
                    };
                    arg_asts.push(sub.parse_expr()?);
                    sub.skip_ws();
                    if sub.i != sub.s.len() {
                        return Err(());
                    }
                }
                return Ok(Ast::Call {
                    name,
                    args: arg_asts,
                });
            }
            if let Some((addr, locks, len)) = parse_cell_ref_at(rest, self.main_cols) {
                self.i = start + len;
                return Ok(Ast::Ref { addr, locks });
            }
            return Ok(Ast::Name(token.to_string()));
        }

        Err(())
    }

    fn parse_number(&mut self) -> Result<Number, ()> {
        let start = self.i;
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() || b == b'.' {
                self.i += 1;
            } else {
                break;
            }
        }
        number::parse_number_literal(&self.s[start..self.i]).ok_or(())
    }

    /// Find index of closing `)` matching an already-open `(` at depth starting at `from`.
    fn scan_balanced_from(&self, from: usize) -> Result<usize, ()> {
        let bytes = self.s.as_bytes();
        let mut depth = 1usize;
        let mut i = from;
        let mut in_string = false;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => {
                    if in_string && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                        i += 1;
                    } else {
                        in_string = !in_string;
                    }
                }
                b'(' if !in_string => depth += 1,
                b')' if !in_string => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(i);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        Err(())
    }
}

fn parse_sheet_qualified_ref(s: &str) -> Option<(u32, CellAddr, A1RefLocks, usize)> {
    let bytes = s.as_bytes();
    if bytes.first().copied()? != b'#' {
        return None;
    }
    let mut i = 1usize;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 1 || i >= bytes.len() || bytes[i] != b'!' {
        return None;
    }
    let sheet_id = std::str::from_utf8(&bytes[1..i]).ok()?.parse().ok()?;
    let (addr, locks, len) = parse_cell_ref_at(&s[i + 1..], 0)?;
    Some((sheet_id, addr, locks, i + 1 + len))
}

fn parse_sheet_qualified_cell_ref_at_for_workbook(s: &str) -> Option<(u32, CellAddr, A1RefLocks, usize)> {
    let bytes = s.as_bytes();
    let mut i = 1usize;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 1 || i >= bytes.len() || bytes[i] != b':' {
        return None;
    }
    let sheet_id = std::str::from_utf8(&bytes[1..i]).ok()?.parse().ok()?;
    let grid = workbook_lookup(sheet_id)?;
    let (addr, locks, len) = parse_cell_ref_at(&s[i + 1..], grid.main_cols())?;
    Some((sheet_id, addr, locks, i + 1 + len))
}

fn split_top_level_args(s: &str) -> Result<Vec<&str>, ()> {
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut out = Vec::new();
    let mut in_string = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' => {
                if in_string {
                    in_string = false;
                } else {
                    in_string = true;
                }
            }
            '(' if !in_string => depth += 1,
            ')' if !in_string => depth -= 1,
            // European / ODF list separator as well as U.S. comma: both delimit function args.
            ',' | ';' if depth == 0 && !in_string => {
                out.push(s[start..i].trim());
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(s[start..].trim());
    if depth != 0 {
        return Err(());
    }
    Ok(out)
}

fn eval_ast(
    ast: &Ast,
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    // Guard overall evaluation recursion depth to avoid blowing the OS stack
    // from unexpected recursive AST structures (deeply nested expressions).
    // Mirrors the checks in eval_cell_inner / eval_cell_with_sheet.
    let _depth_guard = {
        let mut entered = false;
        EVAL_RECURSION_DEPTH.with(|d| {
            let cur = d.get();
            if cur >= MAX_VISIT_DEPTH {
                entered = false;
            } else {
                d.set(cur + 1);
                entered = true;
            }
        });
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                EVAL_RECURSION_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
            }
        }
        if !entered {
            return EvalResult::Error("LIMIT");
        }
        Guard
    };
    match ast {
        Ast::Number(n) => EvalResult::Number(n.clone()),
        Ast::Text(s) => EvalResult::Text(s.clone()),
        Ast::Name(name) => resolve_name(name, bindings).unwrap_or(EvalResult::Error("NAME")),
        Ast::Ref { addr, locks: _ } => {
            // Early detect same-sheet cycles before recursing to avoid
            // consuming another stack frame in obvious circular cases.
            if visiting.iter().any(|a| a == addr) {
                // Special-case: when the reference is to the same cell that
                // we're currently evaluating (direct self-reference via a
                // templated formula), treat it as a read of the stored/raw
                // cell text rather than recursively re-evaluating the cell.
                // This lets left-/header-templates like `=:1*0.1 -- TAX`
                // compute using the cell's raw value (e.g. "10") without
                // triggering a CIRC false-positive. If the stored value is
                // itself a formula, consider it a circular reference.
                if visiting.last().map_or(false, |last| last == addr) {
                    let raw_owned = grid.get(addr);
                    let raw = raw_owned.as_deref().unwrap_or("");
                    if raw.trim().is_empty() {
                        return EvalResult::Number(Number::exact_zero());
                    }
                    // If the stored cell is a formula, treat as circular.
                    if raw.trim().starts_with('=') {
                        return EvalResult::Error("CIRC");
                    }
                    return eval_plain_cell_raw(raw.trim());
                }
                return EvalResult::Error("CIRC");
            }
            eval_cell_inner(grid, addr, visiting, budget, allow_templates)
        }
        Ast::SheetRef {
            sheet_id,
            addr,
            locks: _,
        } => {
            // Early detect sheet-qualified cycles using the global visiting
            // stack so we don't recurse into eval_cell_with_sheet and risk
            // overflowing the thread stack in pathological cases.
            if SHEET_VISITING.with(|stack| stack.borrow().iter().any(|a| a.0 == *sheet_id && &a.1 == addr)) {
                return EvalResult::Error("CIRC");
            }
            let Some(sheet_grid) = workbook_lookup(*sheet_id) else {
                return EvalResult::Error("SHEET");
            };
            let mut sheet_bindings: Vec<(String, EvalResult)> = Vec::new();
            eval_cell_with_sheet(&sheet_grid, *sheet_id, addr, &mut sheet_bindings, budget, allow_templates)
        }
        Ast::Range { .. } => EvalResult::Error("RANGE"),
        Ast::Neg(a) => match eval_ast(a, grid, visiting, bindings, budget, allow_templates)
            .scalar_coerce()
        {
            EvalResult::Number(n) => EvalResult::Number(n.neg()),
            EvalResult::Bool(b) => EvalResult::Number(number_from_bool(b).neg()),
            e => e,
        },
        Ast::Add(a, b) => eval_binary_op(
            a,
            b,
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
            BinaryOp::Add,
        ),
        Ast::Sub(a, b) => eval_binary_op(
            a,
            b,
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
            BinaryOp::Sub,
        ),
        Ast::Mul(a, b) => eval_binary_op(
            a,
            b,
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
            BinaryOp::Mul,
        ),
        Ast::Div(a, b) => eval_binary_op(
            a,
            b,
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
            BinaryOp::Div,
        ),
        Ast::Pow(a, b) => eval_binary_op(
            a,
            b,
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
            BinaryOp::Pow,
        ),
        Ast::Concat(a, b) => {
            let ea = eval_ast(
                a,
                grid,
                visiting,
                bindings,
                budget,
                allow_templates,
            )
            .scalar_coerce();
            let eb = eval_ast(
                b,
                grid,
                visiting,
                bindings,
                budget,
                allow_templates,
            )
            .scalar_coerce();
            match (ea, eb) {
                (EvalResult::Error(e), _) | (_, EvalResult::Error(e)) => EvalResult::Error(e),
                (a, b) => {
                    let mut s = eval_result_to_string(&a);
                    s.push_str(&eval_result_to_string(&b));
                    EvalResult::Text(s)
                }
            }
        },
        Ast::Call { name, args } => functions::eval_builtin(
            name,
            args,
            grid,
            visiting,
            bindings,
            budget,
            allow_templates,
        ),
    }
}

fn truthy(e: EvalResult) -> bool {
    match e.scalar_coerce() {
        EvalResult::Bool(b) => b,
        EvalResult::Number(n) => !n.is_nan() && !n.is_zeroish(),
        EvalResult::Text(s) => functions::parse_numeric_or_date_literal(&s)
            .map(|v| !v.is_nan() && !v.is_zeroish())
            .unwrap_or(false),
        EvalResult::Error(_) => false,
        EvalResult::Array(_) => false,
    }
}

enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Pow,
}

fn eval_binary_op(
    a: &Ast,
    b: &Ast,
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
    op: BinaryOp,
) -> EvalResult {
    let ea = eval_ast(
        a,
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )
    .scalar_coerce();
    let eb = eval_ast(
        b,
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )
    .scalar_coerce();
    let na = match coerce_cell_number(ea) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let nb = match coerce_cell_number(eb) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let out = match op {
        BinaryOp::Add => na.add(nb),
        BinaryOp::Sub => na.sub(nb),
        BinaryOp::Mul => na.mul(nb),
        BinaryOp::Div => na.div(nb),
        BinaryOp::Pow => na.pow(nb),
    };
    EvalResult::Number(out)
}

pub(super) fn coerce_cell_number(e: EvalResult) -> Result<Number, EvalResult> {
    match e {
        EvalResult::Number(n) => Ok(n),
        EvalResult::Bool(b) => Ok(number_from_bool(b)),
        EvalResult::Text(s) => {
            if let Some(n) = functions::parse_numeric_or_date_literal(&s) {
                Ok(n)
            } else {
                Err(EvalResult::Error("VALUE"))
            }
        }
        EvalResult::Error(e) => Err(EvalResult::Error(e)),
        EvalResult::Array(_) => Err(EvalResult::Error("CALC")),
    }
}

/// Used by builtins that need float semantics (`POWER`, trigonometry, etc.).
pub(super) fn eval_binary_float(
    a: &Ast,
    b: &Ast,
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
    f: fn(f64, f64) -> f64,
) -> EvalResult {
    let ea = eval_ast(
        a,
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )
    .scalar_coerce();
    let eb = eval_ast(
        b,
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )
    .scalar_coerce();
    let na = match coerce_cell_number(ea) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let nb = match coerce_cell_number(eb) {
        Ok(n) => n,
        Err(e) => return e,
    };
    EvalResult::Number(Number::from_f64_unchecked(f(na.to_f64(), nb.to_f64())))
}

/// Real-first binary helper with complex fallback for operations that leave the real domain.
fn eval_binary_float_with_complex_fallback(
    a: &Ast,
    b: &Ast,
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    bindings: &mut Vec<(String, EvalResult)>,
    budget: &mut usize,
    allow_templates: bool,
    real: fn(f64, f64) -> f64,
    complex: fn(num_complex::Complex64, num_complex::Complex64) -> num_complex::Complex64,
) -> EvalResult {
    let ea = eval_ast(
        a,
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )
    .scalar_coerce();
    let eb = eval_ast(
        b,
        grid,
        visiting,
        bindings,
        budget,
        allow_templates,
    )
    .scalar_coerce();
    let na = match coerce_cell_number(ea) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let nb = match coerce_cell_number(eb) {
        Ok(n) => n,
        Err(e) => return e,
    };
    EvalResult::Number(na.apply_binary_f64_with_complex_fallback(nb, real, complex))
}

fn eval_sum(
    arg: &Ast,
    grid: &Grid,
    visiting: &mut Vec<CellAddr>,
    budget: &mut usize,
    allow_templates: bool,
) -> EvalResult {
    match arg {
        Ast::Range {
            range: r,
            locks_tl: _,
            locks_br: _,
        } => EvalResult::Number(sum_main_range(grid, r, visiting, budget)),
        Ast::Ref { addr, locks: _ } => {
            let n = summable_numeric(grid, addr, visiting, budget);
            EvalResult::Number(n.unwrap_or_else(Number::exact_zero))
        }
        Ast::Call { .. }
        | Ast::Neg(_)
        | Ast::Add(_, _)
        | Ast::Sub(_, _)
        | Ast::Mul(_, _)
        | Ast::Div(_, _)
        | Ast::Pow(_, _)
        | Ast::Concat(_, _)
        | Ast::Text(_)
        | Ast::Name(_)
        | Ast::SheetRef { .. } => match eval_ast(
            arg,
            grid,
            visiting,
            &mut Vec::new(),
            budget,
            allow_templates,
        )
        .scalar_coerce()
        {
            e => match coerce_cell_number(e) {
                Ok(n) => EvalResult::Number(n),
                Err(err) => err,
            },
        },
        Ast::Number(n) => EvalResult::Number(n.clone()),
    }
}

fn sum_main_range(
    grid: &Grid,
    range: &MainRange,
    visiting: &mut Vec<CellAddr>,
    budget: &mut usize,
) -> Number {
    if range.is_empty() {
        return Number::exact_zero();
    }
    let mut s = Number::exact_zero();
    for r in range.row_start..range.row_end {
        for c in range.col_start..range.col_end {
            let addr = CellAddr::Main { row: r, col: c };
            let n = summable_numeric(grid, &addr, visiting, budget)
                .unwrap_or_else(Number::exact_zero);
            s = s.add(n);
        }
    }
    s
}

pub fn refresh_spills(grid: &mut Grid) {
    if !grid.spills_refresh_dirty() {
        return;
    }
    let mut prev_followers: Vec<(CellAddr, String)> = grid.spill_followers().into_iter().collect();
    let mut prev_errors: Vec<(CellAddr, &'static str)> = grid.spill_errors().into_iter().collect();
    for _ in 0..8 {
        grid.clear_spills();
        let mut anchors: Vec<(CellAddr, String)> = grid.iter_nonempty().collect();
        anchors.sort_by_key(|(addr, _)| match addr {
            CellAddr::Main { row, col } => (*row, *col),
            _ => (u32::MAX, u32::MAX),
        });
        for (addr, raw) in anchors {
            if !is_formula(&raw) {
                continue;
            }
            let mut visiting = Vec::new();
            let mut budget = DEFAULT_BUDGET;
            if let EvalResult::Array(rows) = eval_cell(grid, &addr, &mut visiting, &mut budget) {
                let CellAddr::Main { row: ar, col: ac } = addr else {
                    continue;
                };
                for (r, row) in rows.iter().enumerate() {
                    for (c, cell) in row.iter().enumerate() {
                        if r == 0 && c == 0 {
                            continue;
                        }
                        grid.set_spill_value(
                            CellAddr::Main {
                                row: ar + r as u32,
                                col: ac + c as u32,
                            },
                            eval_result_to_string(cell),
                        );
                    }
                }
            }
        }
        if grid.spill_followers() == prev_followers && grid.spill_errors() == prev_errors {
            break;
        }
        prev_followers = grid.spill_followers();
        prev_errors = grid.spill_errors();
    }
    grid.note_spills_refreshed();
}

fn format_number(n: &Number) -> String {
    n.format_eval_display(format_significant_10)
}

/// Display for UI/export when a column uses [`crate::grid::NumberFormat::Rational`]: exact rationals
/// as literals; approximate values use the same float rendering as evaluation.
pub(crate) fn format_number_cell_display(n: &Number) -> String {
    format_number(n)
}

fn eval_result_to_string(result: &EvalResult) -> String {
    match result {
        EvalResult::Number(n) => {
            if n.is_nan() {
                "#NUM!".to_string()
            } else {
                format_number(n)
            }
        }
        EvalResult::Bool(true) => "TRUE".to_string(),
        EvalResult::Bool(false) => "FALSE".to_string(),
        EvalResult::Text(s) => s.clone(),
        EvalResult::Error(e) => format!("#{e}"),
        EvalResult::Array(rows) => rows
            .first()
            .and_then(|row| row.first())
            .map(eval_result_to_string)
            .unwrap_or_else(|| "#CALC".to_string()),
    }
}

fn formula_references_all_empty(grid: &Grid, formula: &str) -> bool {
    let t = formula.trim();
    let Some(expr) = t.strip_prefix('=') else {
        return false;
    };
    let expr = split_labeled_formula(t).map_or(expr, |(expr, _)| expr);
    let mut p = Parser {
        s: expr.trim(),
        i: 0,
        main_cols: grid.main_cols(),
    };
    let Ok(ast) = p.parse_expr() else {
        return false;
    };
    p.skip_ws();
    if p.i != p.s.len() {
        return false;
    }

    let mut saw_ref = false;
    ast_references_all_empty(&ast, grid, &mut saw_ref) && saw_ref
}

fn ast_references_all_empty(ast: &Ast, grid: &Grid, saw_ref: &mut bool) -> bool {
    match ast {
        Ast::Number(_) | Ast::Text(_) | Ast::Name(_) => true,
        Ast::Ref { addr, .. } => {
            *saw_ref = true;
            cell_reference_is_empty(grid, addr)
        }
        Ast::SheetRef { sheet_id, addr, .. } => {
            let Some(sheet_grid) = workbook_lookup(*sheet_id) else {
                return false;
            };
            *saw_ref = true;
            cell_reference_is_empty(&sheet_grid, addr)
        }
        Ast::Range { range, .. } => {
            let mut all_empty = true;
            for row in range.row_start..range.row_end {
                for col in range.col_start..range.col_end {
                    *saw_ref = true;
                    let addr = CellAddr::Main { row, col };
                    all_empty &= cell_reference_is_empty(grid, &addr);
                }
            }
            all_empty
        }
        Ast::Neg(a) => ast_references_all_empty(a, grid, saw_ref),
        Ast::Add(a, b)
        | Ast::Sub(a, b)
        | Ast::Mul(a, b)
        | Ast::Div(a, b)
        | Ast::Pow(a, b)
        | Ast::Concat(a, b) => {
            ast_references_all_empty(a, grid, saw_ref) && ast_references_all_empty(b, grid, saw_ref)
        }
        Ast::Call { args, .. } => args
            .iter()
            .all(|arg| ast_references_all_empty(arg, grid, saw_ref)),
    }
}

fn cell_reference_is_empty(grid: &Grid, addr: &CellAddr) -> bool {
    grid.get(addr)
        .as_deref()
        .map_or(true, |value| value.trim().is_empty())
}

/// Display string for a cell: evaluated formula result, or raw text.
pub fn cell_effective_display(grid: &Grid, addr: &CellAddr) -> String {
    if let Some(label) = control_formula_label(grid, addr) {
        return label;
    }
    if let Some(err) = grid.spill_error(addr) {
        return format!("#{err}");
    }
    if let Some(v) = grid
        .spill_followers()
        .into_iter()
        .find(|(a, _)| a == addr)
        .map(|(_, v)| v)
    {
        return v;
    }
    let raw_owned = grid.get(addr);
    let raw = raw_owned.as_deref().unwrap_or("");
    let template_formula = templated_formula(grid, addr);
    if template_formula.is_none() && !is_formula(raw) {
        // Normalize plain numeric literals for display. Preserve date-like
        // textual forms (e.g. "2001/01/01") so the UI shows the original
        // human-entered text instead of the internal numeric serial. Date
        // literals are still recognized by the evaluator; this only affects
        // how plain text cells are rendered.
        if let Some(n) = parse_number_literal(raw) {
            return format_number(&n);
        }
        return raw.to_string();
    }
    let mut visiting = Vec::new();
    let mut budget = DEFAULT_BUDGET;
    match eval_cell(grid, addr, &mut visiting, &mut budget) {
        EvalResult::Number(n) => {
            if n.is_nan() {
                "#NUM!".to_string()
            } else if n.is_zeroish()
                && template_formula
                    .as_deref()
                    .is_some_and(|formula| formula_references_all_empty(grid, formula))
            {
                String::new()
            } else {
                format_number(&n)
            }
        }
        EvalResult::Bool(true) => "TRUE".to_string(),
        EvalResult::Bool(false) => "FALSE".to_string(),
        EvalResult::Text(s) => s,
        EvalResult::Error(e) => format!("#{e}"),
        EvalResult::Array(rows) => rows
            .first()
            .and_then(|row| row.first())
            .map(eval_result_to_string)
            .unwrap_or_else(|| "#CALC".to_string()),
    }
}

fn format_significant_10(n: f64) -> String {
    if !n.is_finite() {
        return n.to_string();
    }
    if n == 0.0 {
        return "0".into();
    }
    let abs = n.abs();
    if (1e-4..1e10).contains(&abs) {
        let exp = abs.log10().floor() as i32;
        let decimals = (9 - exp).max(0) as usize;
        let s = format!("{n:.decimals$}");
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    } else {
        format!("{n:.9e}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nf(n: &Number) -> f64 {
        n.to_f64()
    }

    /// Classic float hazard: 0.1+0.2 stays exact as a rational, not 0.30000000000000004.
    #[test]
    fn decimal_fractions_sum_exactly() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 1));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=0.1+0.2".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Number(n) => {
                assert!((nf(&n) - 0.3).abs() < 1e-15);
                assert_eq!(format!("{}", n), "0.3");
            }
            e => panic!("expected number {:?}", e),
        }
    }

    #[test]
    fn abs_preserves_exact_rational_display() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 2));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=ABS(-5)".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "=ABS(-3.25)".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert_eq!(format!("{}", n), "5"),
            e => panic!("expected Abs int {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 1 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert_eq!(format!("{}", n), "3.25"),
            e => panic!("expected Abs dec {:?}", e),
        }
    }

    #[test]
    fn round_mod_keeps_exact_where_applicable() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 3));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=ROUND(0.126,2)".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "=MOD(17,7)".into());
        g.set(&CellAddr::Main { row: 0, col: 2 }, "=ROUND(33145.764,-3)".into());

        let cases = [(0usize, "0.13"), (1, "3"), (2, "33000")];
        for &(col, want) in &cases {
            let mut v = Vec::new();
            let mut b = DEFAULT_BUDGET;
            match eval_cell(&g, &CellAddr::Main { row: 0, col: col as u32 }, &mut v, &mut b) {
                EvalResult::Number(n) => assert_eq!(format!("{}", n), want),
                e => panic!("col {}: {:?}", col, e),
            }
        }
    }

    #[test]
    fn margin_stored_eq_total_displays_as_total_label() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 1));
        let addr = CellAddr::Header {
            row: (HEADER_ROWS - 1) as u32,
            col: ColumnAddr::from_global(MARGIN_COLS + 1, g.main_cols()),
        };
        g.set(&addr, "=ToTaL".into());
        assert_eq!(cell_effective_display(&g, &addr), "TOTAL");
    }

    #[test]
    fn margin_stored_double_eq_total_displays_as_total_label() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 1));
        let addr = CellAddr::Header {
            row: (HEADER_ROWS - 1) as u32,
            col: ColumnAddr::from_global(MARGIN_COLS + 1, g.main_cols()),
        };
        g.set(&addr, "==ToTaL".into());
        assert_eq!(cell_effective_display(&g, &addr), "TOTAL");
    }

    #[test]
    fn double_equals_aggregate_not_is_formula() {
        assert!(!is_formula("==TOTAL"));
        assert!(!is_formula("  ==MIN"));
        assert!(is_formula("=TOTAL"));
        assert!(is_formula("=MIN(A1)"));
    }

    #[test]
    fn formula_add() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(2, 2));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=1+2*3".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 7.0).abs() < 1e-9),
            e => panic!("expected number {:?}", e),
        }
    }

    #[test]
    fn formula_amp_concat_coerces_numbers_to_text() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 1));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=1&2".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "12"),
            e => panic!("expected text {:?}", e),
        }
    }

    #[test]
    fn formula_amp_concat_text_literal_and_number() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 1));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=\"a\"&2".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "a2"),
            e => panic!("expected text {:?}", e),
        }
    }

    #[test]
    fn formula_amp_binds_looser_than_add() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 1));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=1+2&3".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "33"),
            e => panic!("expected text {:?}", e),
        }
    }

    #[test]
    fn formula_pow() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 1));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=2^3".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "=4^0.5".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 8.0).abs() < 1e-9),
            e => panic!("expected 8 {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 1 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 2.0).abs() < 1e-9),
            e => panic!("expected 2 {:?}", e),
        }
    }

    #[test]
    fn complex_fallback_for_sqrt_and_pow_display() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 4));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=SQRT(-1)".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "=(-1)^0.5".into());
        g.set(&CellAddr::Main { row: 0, col: 2 }, "=SQRT(-1)+2".into());
        g.set(&CellAddr::Main { row: 0, col: 3 }, "=SQRT(-1)*SQRT(-1)".into());

        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 0, col: 0 }),
            "0+1i"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 0, col: 1 }),
            "0+1i"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 0, col: 2 }),
            "2+1i"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 0, col: 3 }),
            "-1+0i"
        );
    }

    #[test]
    fn numeric_display_trims_fractional_trailing_zeroes() {
        assert_eq!(format_significant_10(0.4040000), "0.404");
        assert_eq!(format_significant_10(100.0), "100");
    }

    #[test]
    fn power_is_right_associative() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 1));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=2^3^2".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 512.0).abs() < 1e-9),
            e => panic!("expected 512 {:?}", e),
        }
    }

    #[test]
    fn unary_minus_binds_weaker_than_power() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 1));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=-2^2".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) + 4.0).abs() < 1e-9),
            e => panic!("expected -4 {:?}", e),
        }
    }

    #[test]
    fn sum_range_with_formula_cells() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(2, 2));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "2".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "=A1+3".into());
        g.set(&CellAddr::Main { row: 1, col: 0 }, "=sum(A1:B1)".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 1, col: 0 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 7.0).abs() < 1e-9),
            e => panic!("expected 7 {:?}", e),
        }
    }

    #[test]
    fn quoted_text_literal_parses() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 1));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=\"hi\"".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "hi"),
            e => panic!("expected text {:?}", e),
        }
    }

    #[test]
    fn quoted_text_escape_parses() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 1));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=\"a\"\"b\"".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "a\"b"),
            e => panic!("expected escaped text {:?}", e),
        }
    }

    #[test]
    fn let_binds_names() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 1));
        g.set(
            &CellAddr::Main { row: 0, col: 0 },
            "=LET(x, 2, y, x + 3, x + y)".into(),
        );
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 7.0).abs() < 1e-9),
            e => panic!("expected 7 {:?}", e),
        }
    }

    #[test]
    fn let_supports_shadowing() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 1));
        g.set(
            &CellAddr::Main { row: 0, col: 0 },
            "=LET(x, 1, x, x + 2, x)".into(),
        );
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 3.0).abs() < 1e-9),
            e => panic!("expected 3 {:?}", e),
        }
    }

    #[test]
    fn translate_formula_text_rewrites_row_refs() {
        let ctx = FormulaCopyContext {
            source_sheet_id: 1,
            target_sheet_id: 2,
            row_map: vec![1, 0],
            main_cols: 1,
        };
        let out = translate_formula_text("=A1+B2", &ctx).expect("translated formula");
        assert_eq!(out, "=(A2+B1)");
    }

    #[test]
    fn translate_formula_text_by_offset_shifts_relative_only() {
        assert_eq!(
            translate_formula_text_by_offset("=$A$1+B2", 1, 1, 0).unwrap(),
            "=($A$1+C3)"
        );
        assert_eq!(translate_formula_text_by_offset("=A1", 1, 0, 0).unwrap(), "=A2");
    }

    #[test]
    fn translate_formula_insert_main_rows_bumps_refs_on_or_below_gap_start() {
        assert_eq!(
            translate_formula_text_insert_main_rows("=A10+A11+A12", 2, 10, 2, None).unwrap(),
            "=((A10+A13)+A14)"
        );
    }

    #[test]
    fn translate_formula_insert_main_cols_bumps_refs_on_or_right_of_gap_start() {
        assert_eq!(
            translate_formula_text_insert_main_cols("=A1+C1+E1", 5, 2, 1, None).unwrap(),
            "=((A1+D1)+F1)"
        );
    }

    #[test]
    fn translate_formula_text_keeps_absolute_ref_under_row_map() {
        let ctx = FormulaCopyContext {
            source_sheet_id: 1,
            target_sheet_id: 2,
            row_map: vec![1, 0],
            main_cols: 1,
        };
        assert_eq!(
            translate_formula_text("=$A$1+B2", &ctx).expect("translate"),
            "=($A$1+B1)"
        );
    }

    #[test]
    fn right_margin_header_template_applies_to_adjacent_main_col() {
        // Create a grid with multiple main columns so right-margin headers exist.
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(2, 3));
        let mc = g.main_cols();
        // Right-margin column immediately to the right of main block (]A)
        let header_addr_a = CellAddr::Header {
            row: (HEADER_ROWS - 1) as u32,
            col: ColumnAddr::from_global(MARGIN_COLS + mc, g.main_cols()),
        };
        g.set(&header_addr_a, "=B".into());

        // The ]A header maps to the last main column (mc-1). For main row 0,
        // the templated formula should become "=B1".
        let main_addr = CellAddr::Main {
            row: 0,
            col: (mc - 1) as u32,
        };
        assert_eq!(templated_formula(&g, &main_addr), Some("=B1".into()));

        // ]B (next right-margin) maps to second-last main column.
        let header_addr_b = CellAddr::Header {
            row: (HEADER_ROWS - 1) as u32,
            col: ColumnAddr::from_global(MARGIN_COLS + mc + 1, g.main_cols()),
        };
        g.set(&header_addr_b, "=C".into());
        let main_addr_b = CellAddr::Main {
            row: 0,
            col: (mc - 2) as u32,
        };
        assert_eq!(templated_formula(&g, &main_addr_b), Some("=C1".into()));
    }

    #[test]
    fn math_constants_evaluate() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 4));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=sin(π)".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "=e".into());
        g.set(&CellAddr::Main { row: 0, col: 2 }, "=c".into());
        g.set(&CellAddr::Main { row: 0, col: 3 }, "=e^i".into());

        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!(nf(&n).abs() < 1e-12),
            e => panic!("expected 0 {:?}", e),
        }

        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 1 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - std::f64::consts::E).abs() < 1e-12),
            e => panic!("expected e {:?}", e),
        }

        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 2 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 299_792_458.0).abs() < 1e-12),
            e => panic!("expected c {:?}", e),
        }

        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 3 }, &mut v, &mut b) {
            EvalResult::Number(Number::Complex(c)) => {
                assert!((c.re - 1.0f64.cos()).abs() < 1e-12);
                assert!((c.im - 1.0f64.sin()).abs() < 1e-12);
            }
            e => panic!("expected complex e^i {:?}", e),
        }
    }

    #[test]
    fn let_can_shadow_pi_constant() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 1));
        g.set(
            &CellAddr::Main { row: 0, col: 0 },
            "=LET(π, 2, π + 1)".into(),
        );

        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 3.0).abs() < 1e-9),
            e => panic!("expected 3 {:?}", e),
        }
    }

    #[test]
    fn bare_name_is_parse_error_outside_let() {
        let mut p = Parser {
            s: "x",
            i: 0,
            main_cols: 0,
        };
        assert!(p.parse_expr().is_ok());
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 1));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=x".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Error(e) => assert_eq!(e, "NAME"),
            e => panic!("expected NAME {:?}", e),
        }
    }

    #[test]
    fn countif_quoted_text_criteria() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 3));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "a".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "b".into());
        g.set(
            &CellAddr::Main { row: 0, col: 2 },
            "=COUNTIF(A1:C1,\"a\")".into(),
        );
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 2 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 1.0).abs() < 1e-9),
            e => panic!("expected 1 {:?}", e),
        }
    }

    #[test]
    fn xlookup_exact_match() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 5));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "b".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "a".into());
        g.set(&CellAddr::Main { row: 0, col: 2 }, "20".into());
        g.set(&CellAddr::Main { row: 0, col: 3 }, "30".into());
        g.set(
            &CellAddr::Main { row: 0, col: 4 },
            "=XLOOKUP(\"a\", A1:B1, C1:D1)".into(),
        );
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 4 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 30.0).abs() < 1e-9),
            e => panic!("expected 30 {:?}", e),
        }
    }

    #[test]
    fn xlookup_if_not_found() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 4));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "a".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "10".into());
        g.set(&CellAddr::Main { row: 0, col: 2 }, "30".into());
        g.set(
            &CellAddr::Main { row: 0, col: 3 },
            "=XLOOKUP(\"z\", A1:A1, B1:B1, \"missing\")".into(),
        );
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 3 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "missing"),
            e => panic!("expected missing {:?}", e),
        }
    }

    #[test]
    fn sequence_spills() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 1));
        g.set(
            &CellAddr::Main { row: 0, col: 0 },
            "=SEQUENCE(2,2,1,1)".into(),
        );
        refresh_spills(&mut g);
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 0, col: 0 }),
            "1"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 0, col: 1 }),
            "2"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 1, col: 0 }),
            "3"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 1, col: 1 }),
            "4"
        );
    }

    #[test]
    fn unique_deduplicates() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 4));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "a".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "a".into());
        g.set(&CellAddr::Main { row: 0, col: 2 }, "b".into());
        g.set(&CellAddr::Main { row: 0, col: 3 }, "=UNIQUE(A1:C1)".into());
        refresh_spills(&mut g);
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 0, col: 3 }),
            "a"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 1, col: 3 }),
            "b"
        );
    }

    #[test]
    fn filter_applies_mask() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 4));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "a".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "b".into());
        g.set(&CellAddr::Main { row: 0, col: 2 }, "1".into());
        g.set(
            &CellAddr::Main { row: 0, col: 3 },
            "=FILTER(A1:B1, C1:D1)".into(),
        );
        refresh_spills(&mut g);
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 0, col: 3 }),
            "a"
        );
    }

    #[test]
    fn iferror_and_ifna() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 4));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "1".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "2".into());
        g.set(
            &CellAddr::Main { row: 0, col: 2 },
            "=IFERROR(VLOOKUP(9,A1:B1,2),\"x\")".into(),
        );
        g.set(
            &CellAddr::Main { row: 0, col: 3 },
            "=IFNA(VLOOKUP(9,A1:B1,2),\"y\")".into(),
        );
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 2 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "x"),
            e => panic!("expected x {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 3 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "y"),
            e => panic!("expected y {:?}", e),
        }
    }

    #[test]
    fn index_and_match_work() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(2, 4));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "a".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "b".into());
        g.set(&CellAddr::Main { row: 1, col: 0 }, "c".into());
        g.set(&CellAddr::Main { row: 1, col: 1 }, "d".into());
        g.set(
            &CellAddr::Main { row: 0, col: 2 },
            "=MATCH(\"c\",A1:A2,0)".into(),
        );
        g.set(
            &CellAddr::Main { row: 0, col: 3 },
            "=INDEX(A1:B2,2,2)".into(),
        );
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 2 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 2.0).abs() < 1e-9),
            e => panic!("expected 2 {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 3 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "d"),
            e => panic!("expected d {:?}", e),
        }
    }

    #[test]
    fn countifs_sumifs_averageifs_work() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(2, 4));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "a".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "1".into());
        g.set(&CellAddr::Main { row: 1, col: 0 }, "a".into());
        g.set(&CellAddr::Main { row: 1, col: 1 }, "3".into());
        g.set(
            &CellAddr::Main { row: 0, col: 2 },
            "=COUNTIFS(A1:A2,\"a\")".into(),
        );
        g.set(
            &CellAddr::Main { row: 0, col: 3 },
            "=SUMIFS(B1:B2,A1:A2,\"a\")".into(),
        );
        g.set(
            &CellAddr::Main { row: 1, col: 2 },
            "=AVERAGEIFS(B1:B2,A1:A2,\"a\")".into(),
        );
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 2 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 2.0).abs() < 1e-9),
            e => panic!("expected 2 {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 3 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 4.0).abs() < 1e-9),
            e => panic!("expected 4 {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 1, col: 2 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 2.0).abs() < 1e-9),
            e => panic!("expected 2 {:?}", e),
        }
    }

    #[test]
    fn sort_take_drop_choose_work() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(4, 9));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "3".into());
        g.set(&CellAddr::Main { row: 1, col: 0 }, "1".into());
        g.set(&CellAddr::Main { row: 2, col: 0 }, "2".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "b".into());
        g.set(&CellAddr::Main { row: 1, col: 1 }, "d".into());
        g.set(&CellAddr::Main { row: 0, col: 3 }, "=SORT(A1:A3)".into());
        g.set(&CellAddr::Main { row: 0, col: 4 }, "=TAKE(A1:A3,2)".into());
        g.set(&CellAddr::Main { row: 0, col: 5 }, "=DROP(A1:A3,1)".into());
        g.set(
            &CellAddr::Main { row: 0, col: 6 },
            "=CHOOSECOLS(A1:B2,2)".into(),
        );
        g.set(
            &CellAddr::Main { row: 0, col: 7 },
            "=CHOOSEROWS(A1:B2,2)".into(),
        );
        refresh_spills(&mut g);
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 0, col: 3 }),
            "1"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 1, col: 3 }),
            "2"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 2, col: 3 }),
            "3"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 0, col: 4 }),
            "3"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 1, col: 4 }),
            "1"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 0, col: 5 }),
            "1"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 1, col: 5 }),
            "2"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 0, col: 6 }),
            "b"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 1, col: 6 }),
            "d"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 0, col: 7 }),
            "1"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 0, col: 8 }),
            "d"
        );
    }

    #[test]
    fn sort_descending_preserves_original_order_for_equal_keys() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(4, 3));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "2".into());
        g.set(&CellAddr::Main { row: 1, col: 0 }, "9".into());
        g.set(&CellAddr::Main { row: 2, col: 0 }, "9".into());
        g.set(
            &CellAddr::Main { row: 0, col: 1 },
            "=SORT(A1:A3,1,-1)".into(),
        );
        refresh_spills(&mut g);
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 0, col: 1 }),
            "9"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 1, col: 1 }),
            "9"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 2, col: 1 }),
            "2"
        );
    }

    #[test]
    fn text_functions_work() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 5));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=LEN(\"abc\")".into());
        g.set(
            &CellAddr::Main { row: 0, col: 1 },
            "=LEFT(\"abcd\",2)".into(),
        );
        g.set(
            &CellAddr::Main { row: 0, col: 2 },
            "=RIGHT(\"abcd\",2)".into(),
        );
        g.set(
            &CellAddr::Main { row: 0, col: 3 },
            "=MID(\"abcd\",2,2)".into(),
        );
        g.set(
            &CellAddr::Main { row: 0, col: 4 },
            "=CONCAT(\"a\",\"b\",\"c\")".into(),
        );
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 3.0).abs() < 1e-9),
            e => panic!("expected 3 {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 1 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "ab"),
            e => panic!("expected ab {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 2 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "cd"),
            e => panic!("expected cd {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 3 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "bc"),
            e => panic!("expected bc {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 4 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "abc"),
            e => panic!("expected abc {:?}", e),
        }
    }

    #[test]
    fn text_casing_and_replace_work() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 7));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=UPPER(\"Abc\")".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "=LOWER(\"AbC\")".into());
        g.set(
            &CellAddr::Main { row: 0, col: 2 },
            "=PROPER(\"hello world\")".into(),
        );
        g.set(
            &CellAddr::Main { row: 0, col: 3 },
            "=SUBSTITUTE(\"a-b-a\",\"a\",\"x\")".into(),
        );
        g.set(
            &CellAddr::Main { row: 0, col: 4 },
            "=REPLACE(\"abcdef\",2,3,\"ZZ\")".into(),
        );
        g.set(
            &CellAddr::Main { row: 0, col: 5 },
            "=FIND(\"cd\",\"abcdef\")".into(),
        );
        g.set(
            &CellAddr::Main { row: 0, col: 6 },
            "=SEARCH(\"CD\",\"abCdEf\")".into(),
        );
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "ABC"),
            e => panic!("expected ABC {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 1 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "abc"),
            e => panic!("expected abc {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 2 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "Hello World"),
            e => panic!("expected Hello World {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 3 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "x-b-x"),
            e => panic!("expected x-b-x {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 4 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "aZZef"),
            e => panic!("expected aZZef {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 5 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 3.0).abs() < 1e-9),
            e => panic!("expected 3 {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 6 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 3.0).abs() < 1e-9),
            e => panic!("expected 3 {:?}", e),
        }
    }

    #[test]
    fn text_formatting_works() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 1));
        g.set(
            &CellAddr::Main { row: 0, col: 0 },
            "=TEXT(12.345,\"0.00\")".into(),
        );
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "12.35"),
            e => panic!("expected 12.35 {:?}", e),
        }
    }

    #[test]
    fn date_time_functions_work() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 7));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "2024-01-02".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "=A1+1".into());
        g.set(&CellAddr::Main { row: 0, col: 2 }, "=YEAR(A1)".into());
        g.set(&CellAddr::Main { row: 0, col: 3 }, "=MONTH(A1)".into());
        g.set(&CellAddr::Main { row: 0, col: 4 }, "=DAY(A1)".into());
        g.set(&CellAddr::Main { row: 0, col: 5 }, "=HOUR(NOW())".into());
        g.set(&CellAddr::Main { row: 0, col: 6 }, "=MINUTE(NOW())".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 1 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!(nf(&n) > 45000.0),
            e => panic!("expected date arithmetic {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 2 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 2024.0).abs() < 1e-9),
            e => panic!("expected year {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 3 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 1.0).abs() < 1e-9),
            e => panic!("expected month {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 4 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 2.0).abs() < 1e-9),
            e => panic!("expected day {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 5 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((0.0..24.0).contains(&nf(&n))),
            e => panic!("expected hour {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 6 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((0.0..60.0).contains(&nf(&n))),
            e => panic!("expected minute {:?}", e),
        }
    }

    #[test]
    fn rand_is_deterministic_per_seed() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 2));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=RAND()".into());
        g.set(
            &CellAddr::Main { row: 0, col: 1 },
            "=RANDBETWEEN(1,10)".into(),
        );
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        let first = match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Number(n) => nf(&n),
            e => panic!("expected rand {:?}", e),
        };
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        let second = match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Number(n) => nf(&n),
            e => panic!("expected rand {:?}", e),
        };
        assert!((first - second).abs() < 1e-12);
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        let between = match eval_cell(&g, &CellAddr::Main { row: 0, col: 1 }, &mut v, &mut b) {
            EvalResult::Number(n) => nf(&n),
            e => panic!("expected randbetween {:?}", e),
        };
        assert!((1.0..=10.0).contains(&between));
        g.bump_volatile_seed();
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        let changed = match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Number(n) => nf(&n),
            e => panic!("expected rand {:?}", e),
        };
        assert!((first - changed).abs() > 1e-12);
    }

    #[test]
    fn practical_batch_functions_work() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(3, 8));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "a".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "".into());
        g.set(&CellAddr::Main { row: 1, col: 0 }, "1".into());
        g.set(&CellAddr::Main { row: 1, col: 1 }, "text".into());
        g.set(
            &CellAddr::Main { row: 2, col: 0 },
            "=COUNTBLANK(A1:B2)".into(),
        );
        g.set(&CellAddr::Main { row: 0, col: 2 }, "=ISNUMBER(A2)".into());
        g.set(&CellAddr::Main { row: 0, col: 3 }, "=ISTEXT(B2)".into());
        g.set(&CellAddr::Main { row: 0, col: 4 }, "=ISBLANK(B1)".into());
        g.set(
            &CellAddr::Main { row: 0, col: 5 },
            "=ISERROR(XMATCH(\"z\",A1:A2))".into(),
        );
        g.set(
            &CellAddr::Main { row: 0, col: 6 },
            "=SWITCH(2,1,\"one\",2,\"two\",\"default\")".into(),
        );
        g.set(
            &CellAddr::Main { row: 0, col: 7 },
            "=CHOOSE(2,\"x\",\"y\",\"z\")".into(),
        );
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 2, col: 0 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 1.0).abs() < 1e-9),
            e => panic!("expected 1 {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 2 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 1.0).abs() < 1e-9),
            e => panic!("expected 1 {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 3 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 1.0).abs() < 1e-9),
            e => panic!("expected 1 {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 4 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 1.0).abs() < 1e-9),
            e => panic!("expected 1 {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 5 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 1.0).abs() < 1e-9),
            e => panic!("expected 1 {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 6 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "two"),
            e => panic!("expected two {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 7 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "y"),
            e => panic!("expected y {:?}", e),
        }
    }

    #[test]
    fn sortby_spills_sorted_rows() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(3, 6));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "b".into());
        g.set(&CellAddr::Main { row: 1, col: 0 }, "a".into());
        g.set(&CellAddr::Main { row: 2, col: 0 }, "c".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "2".into());
        g.set(&CellAddr::Main { row: 1, col: 1 }, "1".into());
        g.set(&CellAddr::Main { row: 2, col: 1 }, "3".into());
        g.set(
            &CellAddr::Main { row: 0, col: 2 },
            "=SORTBY(A1:B3,B1:B3,1)".into(),
        );
        refresh_spills(&mut g);
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 0, col: 2 }),
            "a"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 1, col: 2 }),
            "b"
        );
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 2, col: 2 }),
            "c"
        );
    }

    #[test]
    fn sumproduct_works() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(2, 3));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "1".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "2".into());
        g.set(&CellAddr::Main { row: 1, col: 0 }, "3".into());
        g.set(&CellAddr::Main { row: 1, col: 1 }, "4".into());
        g.set(
            &CellAddr::Main { row: 0, col: 2 },
            "=SUMPRODUCT(A1:B2)".into(),
        );
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 2 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 10.0).abs() < 1e-9),
            e => panic!("expected 10 {:?}", e),
        }
    }

    #[test]
    fn ifs_works() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 1));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=IFS(0,1,1,2)".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 2.0).abs() < 1e-9),
            e => panic!("expected 2 {:?}", e),
        }
    }

    #[test]
    fn xmatch_works() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 4));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "a".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "b".into());
        g.set(&CellAddr::Main { row: 0, col: 2 }, "c".into());
        g.set(
            &CellAddr::Main { row: 0, col: 3 },
            "=XMATCH(\"b\",A1:C1)".into(),
        );
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 3 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 2.0).abs() < 1e-9),
            e => panic!("expected 2 {:?}", e),
        }
    }

    #[test]
    fn max_min_across_two_comma_separated_ranges() {
        // LibreOffice ODS uses `;` between ranges; we normalize to `,` — same as Excel / Corro
        // TSV `MAX(r1,r2)`.
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(4, 2));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "1".into());
        g.set(&CellAddr::Main { row: 1, col: 0 }, "2".into());
        g.set(&CellAddr::Main { row: 2, col: 0 }, "5".into());
        g.set(&CellAddr::Main { row: 3, col: 0 }, "3".into());
        g.set(
            &CellAddr::Main { row: 0, col: 1 },
            "=MAX(A1:A2,A3:A4)".into(),
        );
        g.set(
            &CellAddr::Main { row: 1, col: 1 },
            "=MIN(A1:A2,A3:A4)".into(),
        );
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        assert!(matches!(
            eval_cell(&g, &CellAddr::Main { row: 0, col: 1 }, &mut v, &mut b),
            EvalResult::Number(n) if (nf(&n) - 5.0).abs() < 1e-9
        ));
        v.clear();
        b = DEFAULT_BUDGET;
        assert!(matches!(
            eval_cell(&g, &CellAddr::Main { row: 1, col: 1 }, &mut v, &mut b),
            EvalResult::Number(n) if (nf(&n) - 1.0).abs() < 1e-9
        ));
    }

    #[test]
    fn boolean_functions_work() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 3));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=AND(1,2,3)".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "=OR(0,0,1)".into());
        g.set(&CellAddr::Main { row: 0, col: 2 }, "=NOT(0)".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 1.0).abs() < 1e-9),
            e => panic!("expected 1 {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 1 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 1.0).abs() < 1e-9),
            e => panic!("expected 1 {:?}", e),
        }
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 2 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 1.0).abs() < 1e-9),
            e => panic!("expected 1 {:?}", e),
        }
    }

    #[test]
    fn true_false_typeof_and_if() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(6, 9));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "=TRUE()".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "=FALSE()".into());
        g.set(&CellAddr::Main { row: 0, col: 2 }, "TrUe".into());
        // A5 stays blank for TYPEOF ref test
        g.set(&CellAddr::Main { row: 0, col: 3 }, "=TYPEOF(A1)".into());
        g.set(&CellAddr::Main { row: 0, col: 4 }, "=TYPEOF(B1)".into());
        g.set(&CellAddr::Main { row: 0, col: 5 }, "=TYPEOF(C1)".into());
        g.set(&CellAddr::Main { row: 0, col: 6 }, "=ISNUMBER(A1)".into());
        g.set(&CellAddr::Main { row: 0, col: 7 }, "=ISTEXT(A1)".into());
        g.set(&CellAddr::Main { row: 0, col: 8 }, "=IF(TRUE(),\"yes\",\"no\")".into());
        g.set(&CellAddr::Main { row: 1, col: 0 }, "=TYPEOF(A5)".into());
        g.set(&CellAddr::Main { row: 1, col: 1 }, "=TYPEOF(0)".into());
        g.set(&CellAddr::Main { row: 1, col: 2 }, "=TYPEOF(PI())".into());
        g.set(&CellAddr::Main { row: 1, col: 3 }, "=TYPEOF(\"x\")".into());
        g.set(&CellAddr::Main { row: 1, col: 4 }, "=TYPEOF(i-i)".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        assert!(
            matches!(&eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b), EvalResult::Bool(true)),
            "TRUE()"
        );
        v.clear();
        b = DEFAULT_BUDGET;
        assert!(matches!(
            &eval_cell(&g, &CellAddr::Main { row: 0, col: 1 }, &mut v, &mut b),
            EvalResult::Bool(false)
        ));
        v.clear();
        b = DEFAULT_BUDGET;
        assert!(matches!(
            &eval_cell(&g, &CellAddr::Main { row: 0, col: 2 }, &mut v, &mut b),
            EvalResult::Bool(true)
        ));
        for col in [3u32, 4, 5] {
            v.clear();
            b = DEFAULT_BUDGET;
            match eval_cell(&g, &CellAddr::Main { row: 0, col }, &mut v, &mut b) {
                EvalResult::Text(s) => assert_eq!(s, "bool", "TYPEOF bool col {}", col),
                e => panic!("expected text TYPEOF col {}: {:?}", col, e),
            }
        }
        v.clear();
        b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 6 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!(nf(&n).abs() < 1e-9, "ISNUMBER(TRUE)"),
            e => panic!("ISNUMBER {:?}", e),
        }
        v.clear();
        b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 7 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!(nf(&n).abs() < 1e-9, "ISTEXT(TRUE)"),
            e => panic!("ISTEXT {:?}", e),
        }
        v.clear();
        b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 8 }, &mut v, &mut b) {
            EvalResult::Text(s) => assert_eq!(s, "yes"),
            e => panic!("IF(TRUE) {:?}", e),
        }
        for (col, want) in [
            (0u32, "blank"),
            (1, "exact"),
            (2, "approx"),
            (3, "text"),
            (4, "complex"),
        ] {
            v.clear();
            b = DEFAULT_BUDGET;
            match eval_cell(&g, &CellAddr::Main { row: 1, col }, &mut v, &mut b) {
                EvalResult::Text(s) => assert_eq!(s, want, "TYPEOF row1 col {}", col),
                e => panic!("TYPEOF {:?}", e),
            }
        }
    }

    #[test]
    fn sum_includes_boolean_cells_count_excludes_them() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(2, 4));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "TRUE".into());
        g.set(&CellAddr::Main { row: 1, col: 0 }, "=SUM(A1)".into());
        g.set(&CellAddr::Main { row: 1, col: 1 }, "=COUNT(A1)".into());
        g.set(&CellAddr::Main { row: 1, col: 2 }, "=COUNT(TRUE())".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 1, col: 0 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 1.0).abs() < 1e-9),
            e => panic!("SUM {:?}", e),
        }
        v.clear();
        b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 1, col: 1 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!(nf(&n).abs() < 1e-9),
            e => panic!("COUNT range {:?}", e),
        }
        v.clear();
        b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 1, col: 2 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!(nf(&n).abs() < 1e-9),
            e => panic!("COUNT TRUE {:?}", e),
        }
    }

    #[test]
    fn sheet_ref_syntax_parses() {
        let mut p = Parser {
            s: "#2!A1",
            i: 0,
            main_cols: 0,
        };
        match p.parse_expr().unwrap() {
            Ast::SheetRef { sheet_id, addr, .. } => {
                assert_eq!(sheet_id, 2);
                assert_eq!(addr, CellAddr::Main { row: 0, col: 0 });
            }
            other => panic!("unexpected ast: {other:?}"),
        }
    }

    #[test]
    fn named_sheet_ref_syntax_parses() {
        let mut wb = WorkbookState::new();
        wb.add_sheet("Sheet2".into(), crate::ops::SheetState::new(1, 1));
        let _guard = set_eval_context(&wb);
        let mut p = Parser {
            s: "$Sheet2:A1",
            i: 0,
            main_cols: 0,
        };
        match p.parse_expr().unwrap() {
            Ast::SheetRef { sheet_id, addr, .. } => {
                assert_eq!(sheet_id, 2);
                assert_eq!(addr, CellAddr::Main { row: 0, col: 0 });
            }
            other => panic!("unexpected ast: {other:?}"),
        }
    }

    #[test]
    fn named_sheet_title_ref_parses() {
        let mut wb = WorkbookState::new();
        wb.add_sheet("Budget".into(), crate::ops::SheetState::new(1, 1));
        let _guard = set_eval_context(&wb);
        let mut p = Parser {
            s: "$Budget:A1",
            i: 0,
            main_cols: 0,
        };
        match p.parse_expr().unwrap() {
            Ast::SheetRef { sheet_id, addr, .. } => {
                assert_eq!(sheet_id, 2);
                assert_eq!(addr, CellAddr::Main { row: 0, col: 0 });
            }
            other => panic!("unexpected ast: {other:?}"),
        }
    }

    #[test]
    fn circular_ref() {
        // Run the evaluation in a thread with a larger stack to avoid
        // platform-dependent stack overflow in deeply recursive paths.
        std::thread::Builder::new()
            .name("circular_ref".into())
            // Give the test thread a larger stack to avoid platform-dependent
            // stack overflows when evaluation recurses deeply on some hosts.
            // Increase from 32 MB to 512 MB to be more robust on CI/platforms
            // where the default/native thread stack may be smaller.
            .stack_size(512 * 1024 * 1024)
            .spawn(|| {
                let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 2));
                g.set(&CellAddr::Main { row: 0, col: 0 }, "=B1".into());
                g.set(&CellAddr::Main { row: 0, col: 1 }, "=A1".into());
                let mut v = Vec::new();
                let mut b = DEFAULT_BUDGET;
                match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
                    EvalResult::Error(e) => assert_eq!(e, "CIRC"),
                    e => panic!("expected CIRC {:?}", e),
                }
            })
            .expect("spawn test thread")
            .join()
            .expect("test thread panicked");
    }

    #[test]
    fn cross_sheet_circular_ref() {
        // Run the evaluation in a thread with a larger stack to avoid
        // platform-dependent stack overflow in deeply recursive paths.
        std::thread::Builder::new()
            .name("cross_sheet_circular_ref".into())
            // Give the test thread a larger stack to avoid platform-dependent
            // stack overflows when evaluation recurses across sheets on some hosts.
            // Increase from 32 MB to 512 MB to be more robust on CI/platforms
            // where the default/native thread stack may be smaller.
            .stack_size(512 * 1024 * 1024)
            .spawn(|| {
                // A!A1 -> B!A1 -> A!A1 should be detected as CIRC using SHEET_VISITING.
                let mut wb = crate::ops::WorkbookState::new();
                // Ensure two sheets with ids 1 and 2
                let mut sheet2 = crate::ops::SheetState::new(1, 1);
                sheet2
                    .grid
                    .set(&CellAddr::Main { row: 0, col: 0 }, "=$1:A1".into());
                // Sheet 1 references sheet 2
                wb.sheets[0]
                    .state
                    .grid
                    .set(&CellAddr::Main { row: 0, col: 0 }, "=#2!A1".into());
                wb.add_sheet("Sheet2".to_string(), sheet2);
                let guard = set_eval_context(&wb);
                let sheet1_grid = workbook_lookup(1).expect("sheet1");
                let mut v = Vec::new();
                let mut b = DEFAULT_BUDGET;
                match eval_cell(&sheet1_grid, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
                    EvalResult::Error(e) => assert_eq!(e, "CIRC"),
                    e => panic!("expected CIRC {:?}", e),
                }
                drop(guard);
            })
            .expect("spawn test thread")
            .join()
            .expect("test thread panicked");
    }

    #[test]
    fn if_func() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(1, 3));
        g.set(&CellAddr::Main { row: 0, col: 0 }, "0".into());
        g.set(&CellAddr::Main { row: 0, col: 2 }, "=IF(A1,1,2)".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 2 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 2.0).abs() < 1e-9),
            e => panic!("expected 2 {:?}", e),
        }
    }

    #[test]
    fn header_template_label_is_display_only() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(2, 2));
        g.set(
            &CellAddr::Header {
                row: (HEADER_ROWS - 1) as u32,
                col: ColumnAddr::from_global(MARGIN_COLS + 1, g.main_cols()),
            },
            "=A*2 -- POW2".into(),
        );
        g.set(&CellAddr::Main { row: 0, col: 0 }, "7".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 0, col: 1 }),
            "14"
        );
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 1 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 14.0).abs() < 1e-9),
            e => panic!("expected 14 {:?}", e),
        }
        assert_eq!(
            cell_effective_display(
                &g,
                &CellAddr::Header {
                    row: (HEADER_ROWS - 1) as u32,
                    col: ColumnAddr::from_global(MARGIN_COLS + 1, g.main_cols()),
                },
            ),
            "POW2"
        );
    }

    #[test]
    fn header_template_zero_from_blank_references_displays_blank() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(2, 2));
        g.set(
            &CellAddr::Header {
                row: (HEADER_ROWS - 1) as u32,
                col: ColumnAddr::from_global(MARGIN_COLS + 1, g.main_cols()),
            },
            "=A*0.1 -- TAX".into(),
        );

        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 0, col: 1 }),
            ""
        );

        g.set(&CellAddr::Main { row: 1, col: 0 }, "0".into());
        assert_eq!(
            cell_effective_display(&g, &CellAddr::Main { row: 1, col: 1 }),
            "0"
        );
    }

    #[test]
    fn left_margin_template_can_label_rows() {
        let mut g = crate::grid::GridBox::from(crate::grid::Grid::new(2, 2));
        g.set(
            &CellAddr::Left {
                col: MARGIN_COLS - 1,
                row: 0,
            },
            "=:1*0.1 -- TAX".into(),
        );
        g.set(&CellAddr::Main { row: 0, col: 0 }, "10".into());
        let mut v = Vec::new();
        let mut b = DEFAULT_BUDGET;
        match eval_cell(&g, &CellAddr::Main { row: 0, col: 0 }, &mut v, &mut b) {
            EvalResult::Number(n) => assert!((nf(&n) - 1.0).abs() < 1e-9),
            e => panic!("expected 1 {:?}", e),
        }
        assert_eq!(
            cell_effective_display(
                &g,
                &CellAddr::Left {
                    col: MARGIN_COLS - 1,
                    row: 0
                }
            ),
            "TAX"
        );
    }
}
