//! Extrapolation utilities: detect simple sequences and generate preview/commit
//! operations. This module intentionally provides a small, well-documented API so
//! the UI can call into it for drag-preview and commit.

use crate::grid::{CellAddr, ColumnAddr, GridBox, MainRange};
use crate::formula::{translate_formula_text_by_offset, is_formula};

/// Direction for a 1-D extrapolation (used by the UI when inferring values).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FillDirection {
    Right,
    Down,
}

/// Infer a single fill value from a seed sequence. Matches the UI precedence:
/// 1) formula translation (translate_formula_text_by_offset)
/// 2) numeric linear extrapolation
/// 3) named-sequence (weekdays/months)
/// 4) suffix increment (preserve zero-padding width)
/// 5) fallback to the last seed value
#[inline(always)]
pub fn infer_fill_value(
    seed: &[String],
    offset_from_last: i32,
    direction: FillDirection,
    main_cols: usize,
) -> Option<String> {
    let last = seed.last()?.clone();
    if is_formula(&last) {
        let (row_delta, col_delta) = match direction {
            FillDirection::Right => (0, offset_from_last),
            FillDirection::Down => (offset_from_last, 0),
        };
         if let Some(translated) = translate_formula_text_by_offset(&last, row_delta, col_delta, main_cols) {
            return Some(translated);
        }
    }
    if let Some(v) = infer_numeric_fill(seed, offset_from_last) {
        return Some(v);
    }
    if let Some(v) = infer_named_sequence_fill(seed, offset_from_last) {
        return Some(v);
    }
    if let Some(v) = infer_suffix_fill(seed, offset_from_last) {
        return Some(v);
    }
    Some(last)
}

/// Basic preview cell: target address and the value to show in preview.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewCell {
    pub addr: CellAddr,
    pub value: String,
}

/// Analyze a rectangular main-region selection and generate a preview fill for
/// the target range. This minimal implementation implements two simple rules:
/// - If the source contains a single formula cell, translate it by offsets (relative fill).
/// - Otherwise, repeat the last cell's text to the target region.
///
/// Parameters:
/// - grid: current sheet grid (GridBox) for value access.
/// - source: main-range being dragged from (row/col in main-space).
/// - target: main-range being filled to (row/col in main-space).
///
/// Returns a Vec of PreviewCell for the target cells in arbitrary order.
#[inline(always)]
pub fn generate_preview(
    grid: &GridBox,
    source: &MainRange,
    target: &MainRange,
) -> Vec<PreviewCell> {
    // Collect source cells (row-major) into a vector of Option<String> so we
    // preserve missing vs present semantics from Grid::get.
    let mut src_values: Vec<Option<String>> = Vec::new();
    for r in source.row_start..source.row_end {
        for c in source.col_start..source.col_end {
            let addr = CellAddr::Main { row: r, col: c };
            let v = grid.get(&addr);
            src_values.push(v);
        }
    }

    // Count formula cells (only present values that start with '=') and
    // count non-empty present values (Some(s) where s is not empty).
    let formula_count = src_values
        .iter()
        .filter(|opt| opt.as_ref().map_or(false, |s| s.trim_start().starts_with('=')))
        .count();
    let nonempty_count = src_values.iter().filter(|opt| opt.as_ref().map_or(false, |s| !s.is_empty())).count();

    let mut out: Vec<PreviewCell> = Vec::new();
    if formula_count == 1 && nonempty_count == 1 {
        // Find the formula cell index and its source coords.
        if let Some((fi, formula_text)) = src_values
            .iter()
            .enumerate()
            .find_map(|(idx, opt)| {
                opt.as_ref()
                    .and_then(|s| s.trim_start().starts_with('=').then_some((idx, s.clone())))
            })
        {
            let src_cols = (source.col_end - source.col_start) as usize;
            let src_r = fi / src_cols;
            let src_c = fi % src_cols;

            for r in target.row_start..target.row_end {
                for c in target.col_start..target.col_end {
                    // compute row/col delta in main-space relative to source top-left
                    let row_delta = r as i32 - (source.row_start as i32 + src_r as i32);
                    let col_delta = c as i32 - (source.col_start as i32 + src_c as i32);
                    let translated = translate_formula_text_by_offset(&formula_text, row_delta, col_delta, grid.main_cols())
                        .unwrap_or_else(|| formula_text.clone());
                    out.push(PreviewCell {
                        addr: CellAddr::Main { row: r, col: c },
                        value: translated,
                    });
                }
            }
            return out;
        }
    }
    // Fallback: attempt to infer a fill (numeric, named-sequence, suffix)
    // using the non-empty present source values as the seed. If inference
    // doesn't produce a value for a target cell, fall back to repeating the
    // last non-empty source value.
    let seeds: Vec<String> = src_values
        .iter()
        .filter_map(|opt| opt.as_ref().and_then(|s| (!s.is_empty()).then_some(s.clone())))
        .collect();
    if seeds.is_empty() {
        return out; // nothing to fill
    }

    // Find the last non-empty present source index so we can compute offsets
    // relative to it when extrapolating (supports backward fills).
    let last_present_idx_opt = src_values
        .iter()
        .rposition(|opt| opt.as_ref().map_or(false, |s| !s.is_empty()));
    let last_present_idx = match last_present_idx_opt {
        Some(i) => i,
        None => return out,
    };
    let src_cols = (source.col_end - source.col_start) as usize;
    let last_row = source.row_start + (last_present_idx / src_cols) as u32;
    let last_col = source.col_start + (last_present_idx % src_cols) as u32;

    // Decide whether this is a vertical or horizontal fill. For simplicity
    // prefer vertical when the target rows differ from the source rows.
    let vertical = (target.row_start != source.row_start) || (target.row_end != source.row_end);

    for r in target.row_start..target.row_end {
        for c in target.col_start..target.col_end {
            let offset = if vertical {
                r as i32 - last_row as i32
            } else {
                c as i32 - last_col as i32
            };
            let inferred = infer_fill_value(&seeds, offset, if vertical { FillDirection::Down } else { FillDirection::Right }, grid.main_cols());
            let value = inferred.unwrap_or_else(|| seeds.last().cloned().unwrap_or_default());
            out.push(PreviewCell { addr: CellAddr::Main { row: r, col: c }, value });
        }
    }
    out
}

// The following helpers mirror the original UI inference routines. They are
// kept private to this module but exposed via `infer_fill_value` above so the
// UI can call a single centralized function.
#[inline(always)]
fn infer_numeric_fill(seed: &[String], offset_from_last: i32) -> Option<String> {
    if !seed.iter().all(|v| v.trim().parse::<f64>().is_ok()) {
        return None;
    }
    let last = seed.last()?.trim().parse::<f64>().ok()?;
    let prev = if seed.len() >= 2 {
        seed[seed.len() - 2].trim().parse::<f64>().ok()?
    } else {
        last
    };
    let step = last - prev;
    Some(format!("{}", last + step * offset_from_last as f64))
}

#[inline(always)]
fn infer_named_sequence_fill(seed: &[String], offset_from_last: i32) -> Option<String> {
    // Accept both 3-letter abbreviations and full names for weekdays/months.
    const WEEKDAYS_ABBR: [&str; 7] = ["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"];
    const WEEKDAYS_FULL: [&str; 7] = [
        "MONDAY",
        "TUESDAY",
        "WEDNESDAY",
        "THURSDAY",
        "FRIDAY",
        "SATURDAY",
        "SUNDAY",
    ];
    const MONTHS_ABBR: [&str; 12] = [
        "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
    ];
    const MONTHS_FULL: [&str; 12] = [
        "JANUARY",
        "FEBRUARY",
        "MARCH",
        "APRIL",
        "MAY",
        "JUNE",
        "JULY",
        "AUGUST",
        "SEPTEMBER",
        "OCTOBER",
        "NOVEMBER",
        "DECEMBER",
    ];

    // Normalize to uppercase letters-only (remove punctuation) for matching.
    let normalized_alpha: Vec<String> = seed
        .iter()
        .map(|v| v.trim().chars().filter(|c| c.is_alphabetic()).collect::<String>().to_ascii_uppercase())
        .collect();
    let last_norm = normalized_alpha.last()?.as_str();
    let original_last = seed.last()?.trim();

    // Split trailing non-alpha suffix (eg. punctuation) so we can re-append it
    // to the inferred token and preserve punctuation like "Feb." -> "Mar.".
#[inline(always)]
    fn split_trailing_non_alpha(s: &str) -> (&str, &str) {
        let mut last_alpha_end: Option<usize> = None;
        for (i, ch) in s.char_indices() {
            if ch.is_alphabetic() {
                last_alpha_end = Some(i + ch.len_utf8());
            }
        }
        match last_alpha_end {
            None => ("", s),
            Some(pos) => (&s[..pos], &s[pos..]),
        }
    }

    let (original_core, original_suffix) = split_trailing_non_alpha(original_last);

    // Helper to apply the original case pattern to the canonical token.
#[inline(always)]
    fn apply_case_pattern(original: &str, token: &str) -> String {
        if original.is_empty() {
            return token.to_string();
        }
        // If original is all-uppercase (or has no letters), return token as-is.
        if original.chars().all(|c| !c.is_alphabetic() || c.is_uppercase()) {
            return token.to_string();
        }
        // All-lower
        if original.chars().all(|c| !c.is_alphabetic() || c.is_lowercase()) {
            return token.to_ascii_lowercase();
        }
        // Title-case: first uppercase, rest lowercase
        let mut chars: Vec<char> = original.chars().collect();
        if !chars.is_empty()
            && chars[0].is_uppercase()
            && chars.iter().skip(1).all(|c| !c.is_alphabetic() || c.is_lowercase())
        {
            let mut out = String::new();
            let mut iter = token.chars();
            if let Some(first) = iter.next() {
                out.extend(first.to_uppercase());
            }
            for ch in iter {
                out.extend(ch.to_lowercase());
            }
            return out;
        }
        // Fallback: per-character mapping (zip). If original is shorter, use
        // original's casing for the corresponding prefix, then lowercase the
        // remainder.
        let mut out = String::new();
        for (o, t) in original.chars().zip(token.chars()) {
            if o.is_lowercase() {
                out.extend(t.to_lowercase());
            } else {
                out.extend(t.to_uppercase());
            }
        }
        // If token has more chars than original, append the rest in lowercase.
        if token.chars().count() > original.chars().count() {
            for t in token.chars().skip(original.chars().count()) {
                out.extend(t.to_lowercase());
            }
        }
        out
    }

    // Classification helpers for tokens (full vs abbr for months/weekdays).
#[inline(always)]
    fn is_weekday_full(s: &str) -> bool {
        WEEKDAYS_FULL.contains(&s)
    }
#[inline(always)]
    fn is_weekday_abbr(s: &str) -> bool {
        let first3: String = s.chars().take(3).collect();
        WEEKDAYS_ABBR.contains(&first3.as_str())
    }
#[inline(always)]
    fn is_month_full(s: &str) -> bool {
        MONTHS_FULL.contains(&s)
    }
#[inline(always)]
    fn is_month_abbr(s: &str) -> bool {
        let first3: String = s.chars().take(3).collect();
        MONTHS_ABBR.contains(&first3.as_str())
    }

    enum SeqKind {
        WeekdayAbbr,
        WeekdayFull,
        MonthAbbr,
        MonthFull,
    }

    let mut kind: Option<SeqKind> = None;
    // Prefer full names over abbreviations when both match (e.g. "January"
    // starts with "JAN" but is the full month name). Check full forms first.
    // All tokens classified as weekday full?
    if normalized_alpha.iter().all(|v| is_weekday_full(v)) {
        kind = Some(SeqKind::WeekdayFull);
    }
    // All tokens classified as weekday abbr?
    if kind.is_none() && normalized_alpha.iter().all(|v| is_weekday_abbr(v)) {
        kind = Some(SeqKind::WeekdayAbbr);
    }
    // All tokens classified as month full?
    if kind.is_none() && normalized_alpha.iter().all(|v| is_month_full(v)) {
        kind = Some(SeqKind::MonthFull);
    }
    // All tokens classified as month abbr?
    if kind.is_none() && normalized_alpha.iter().all(|v| is_month_abbr(v)) {
        kind = Some(SeqKind::MonthAbbr);
    }
    // If mixed, fall back to using the last token's classification if possible.
    if kind.is_none() {
        // Prefer full forms when a token could be both (e.g. "JANUARY")
        if is_weekday_full(last_norm) {
            kind = Some(SeqKind::WeekdayFull);
        } else if is_weekday_abbr(last_norm) {
            kind = Some(SeqKind::WeekdayAbbr);
        } else if is_month_full(last_norm) {
            kind = Some(SeqKind::MonthFull);
        } else if is_month_abbr(last_norm) {
            kind = Some(SeqKind::MonthAbbr);
        }
    }

    match kind {
        Some(SeqKind::WeekdayAbbr) => {
            // find index by matching first3 of last_norm to abbr list
            let last3: String = last_norm.chars().take(3).collect();
            let idx = WEEKDAYS_ABBR.iter().position(|&v| v == last3)?;
            let tok = WEEKDAYS_ABBR[(idx as i32 + offset_from_last).rem_euclid(WEEKDAYS_ABBR.len() as i32) as usize];
            let core = apply_case_pattern(original_core, tok);
            return Some(format!("{}{}", core, original_suffix));
        }
        Some(SeqKind::WeekdayFull) => {
            let idx = WEEKDAYS_FULL.iter().position(|&v| v == last_norm)?;
            let tok = WEEKDAYS_FULL[(idx as i32 + offset_from_last).rem_euclid(WEEKDAYS_FULL.len() as i32) as usize];
            let core = apply_case_pattern(original_core, tok);
            return Some(format!("{}{}", core, original_suffix));
        }
        Some(SeqKind::MonthAbbr) => {
            let last3: String = last_norm.chars().take(3).collect();
            let idx = MONTHS_ABBR.iter().position(|&v| v == last3)?;
            let tok = MONTHS_ABBR[(idx as i32 + offset_from_last).rem_euclid(MONTHS_ABBR.len() as i32) as usize];
            let core = apply_case_pattern(original_core, tok);
            return Some(format!("{}{}", core, original_suffix));
        }
        Some(SeqKind::MonthFull) => {
            let idx = MONTHS_FULL.iter().position(|&v| v == last_norm)?;
            let tok = MONTHS_FULL[(idx as i32 + offset_from_last).rem_euclid(MONTHS_FULL.len() as i32) as usize];
            let core = apply_case_pattern(original_core, tok);
            return Some(format!("{}{}", core, original_suffix));
        }
        None => None,
    }
}

#[inline(always)]
fn infer_suffix_fill(seed: &[String], offset_from_last: i32) -> Option<String> {
    let last = seed.last()?.trim();
    let (prefix, digits) = split_trailing_digits(last)?;
    if seed
        .iter()
        .any(|v| split_trailing_digits(v.trim()).is_none_or(|(p, _)| p != prefix))
    {
        return None;
    }
    let width = digits.len();
    let last_num = digits.parse::<i64>().ok()?;
    let prev_num = if seed.len() >= 2 {
        let (_, prev_digits) = split_trailing_digits(seed[seed.len() - 2].trim())?;
        prev_digits.parse::<i64>().ok()?
    } else {
        last_num
    };
    let next = last_num + (last_num - prev_num) * offset_from_last as i64;
    Some(format!("{}{:0width$}", prefix, next, width = width))
}

#[inline(always)]
fn split_trailing_digits(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    let mut i = bytes.len();
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i == bytes.len() {
        return None;
    }
    Some((&s[..i], &s[i..]))
}

/// Construct an Op::FillRange or Op::RelFillRange equivalent commit for the
/// given preview cells. The caller (UI) should wrap this into a WorkbookOp::SheetOp
/// and commit via the existing IO/ops flow.
#[inline(always)]
pub fn commit_from_preview(cells: Vec<PreviewCell>) -> crate::ops::Op {
    let mapped: Vec<(CellAddr, String)> = cells.into_iter().map(|p| (p.addr, p.value)).collect();
    crate::ops::Op::FillRange { cells: mapped }
}

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::grid::{Grid, GridBox, CellAddr, MainRange};

        #[test]
#[inline(always)]
        fn named_sequence_month_full_forward() {
            let seed = vec!["January".to_string()];
            let out = infer_named_sequence_fill(&seed, 1).expect("should infer");
            assert_eq!(out, "February");
        }

        #[test]
#[inline(always)]
        fn named_sequence_month_abbr_forward() {
            let seed = vec!["Jan".to_string()];
            let out = infer_named_sequence_fill(&seed, 1).expect("should infer");
            assert_eq!(out, "Feb");
        }

        #[test]
#[inline(always)]
        fn named_sequence_month_abbr_uppercase_forward() {
            let seed = vec!["JAN".to_string()];
            let out = infer_named_sequence_fill(&seed, 1).expect("should infer");
            assert_eq!(out, "FEB");
        }

        #[test]
#[inline(always)]
        fn named_sequence_month_abbr_lowercase_forward() {
            let seed = vec!["jan".to_string()];
            let out = infer_named_sequence_fill(&seed, 1).expect("should infer");
            assert_eq!(out, "feb");
        }

        #[test]
#[inline(always)]
        fn named_sequence_month_full_uppercase_forward() {
            let seed = vec!["JUNE".to_string()];
            let out = infer_named_sequence_fill(&seed, 1).expect("should infer");
            assert_eq!(out, "JULY");
        }

        #[test]
#[inline(always)]
        fn named_sequence_month_full_titlecase_forward() {
            let seed = vec!["June".to_string()];
            let out = infer_named_sequence_fill(&seed, 1).expect("should infer");
            assert_eq!(out, "July");
        }

        #[test]
#[inline(always)]
        fn named_sequence_month_full_lowercase_forward() {
            let seed = vec!["june".to_string()];
            let out = infer_named_sequence_fill(&seed, 1).expect("should infer");
            assert_eq!(out, "july");
        }

        #[test]
#[inline(always)]
        fn named_sequence_weekday_full_uppercase_forward() {
            let seed = vec!["THURSDAY".to_string()];
            let out = infer_named_sequence_fill(&seed, 1).expect("should infer");
            assert_eq!(out, "FRIDAY");
        }

        #[test]
#[inline(always)]
        fn named_sequence_weekday_full_titlecase_forward() {
            let seed = vec!["Thursday".to_string()];
            let out = infer_named_sequence_fill(&seed, 1).expect("should infer");
            assert_eq!(out, "Friday");
        }

        #[test]
#[inline(always)]
        fn named_sequence_weekday_full_lowercase_forward() {
            let seed = vec!["thursday".to_string()];
            let out = infer_named_sequence_fill(&seed, 1).expect("should infer");
            assert_eq!(out, "friday");
        }

    #[test]
#[inline(always)]
    fn named_sequence_weekday_full_backward() {
        let seed = vec!["Monday".to_string()];
        let out = infer_named_sequence_fill(&seed, -1).expect("should infer");
        assert_eq!(out, "Sunday");
    }

    #[test]
#[inline(always)]
    fn generate_preview_backwards_weekday_not_working_yet() {
        // This test demonstrates a failing case: dragging "Monday" upwards
        // (target above source) should fill "Sunday" but the existing
        // preview generation didn't compute offsets relative to the last
        // present cell, so it failed. We add the test first to observe the
        // failing behavior, then fix generate_preview to make it pass.
        let mut gb = GridBox::from(Grid::new(4, 1));
        gb.set(&CellAddr::Main { row: 1, col: 0 }, "Monday".into());

        // source is row 1..2, target is row 0..1 (above source)
        let source = MainRange { row_start: 1, row_end: 2, col_start: 0, col_end: 1 };
        let target = MainRange { row_start: 0, row_end: 1, col_start: 0, col_end: 1 };

        let out = generate_preview(&gb, &source, &target);
        assert_eq!(out.len(), 1);
        // Expect Sunday in the cell above Monday
        assert_eq!(out[0].value, "Sunday");
    }

        #[test]
#[inline(always)]
        fn named_sequence_weekday_abbr_backward() {
            let seed = vec!["Mon".to_string()];
            let out = infer_named_sequence_fill(&seed, -1).expect("should infer");
            assert_eq!(out, "Sun");
        }

        #[test]
#[inline(always)]
        fn named_sequence_preserves_suffix() {
            let seed = vec!["Jan.".to_string()];
            let out = infer_named_sequence_fill(&seed, 1).expect("should infer");
            assert_eq!(out, "Feb.");
        }

    #[test]
#[inline(always)]
    fn generate_preview_translates_single_formula() {
        let mut gb = GridBox::from(Grid::new(4, 1));
        gb.set(&CellAddr::Main { row: 0, col: 0 }, "=A1".into());

        let source = MainRange { row_start: 0, row_end: 1, col_start: 0, col_end: 1 };
        let target = MainRange { row_start: 1, row_end: 3, col_start: 0, col_end: 1 };

        let out = generate_preview(&gb, &source, &target);
        assert_eq!(out.len(), 2);
        assert!(out.contains(&PreviewCell { addr: CellAddr::Main { row: 1, col: 0 }, value: "=A2".into() }));
        assert!(out.contains(&PreviewCell { addr: CellAddr::Main { row: 2, col: 0 }, value: "=A3".into() }));
    }

    #[test]
#[inline(always)]
    fn generate_preview_repeats_last_nonempty() {
        let mut gb = GridBox::from(Grid::new(4, 2));
        gb.set(&CellAddr::Main { row: 0, col: 0 }, "one".into());

        let source = MainRange { row_start: 0, row_end: 1, col_start: 0, col_end: 2 };
        let target = MainRange { row_start: 1, row_end: 3, col_start: 0, col_end: 2 };

        let out = generate_preview(&gb, &source, &target);
        // 2 rows * 2 cols
        assert_eq!(out.len(), 4);
        for pc in out {
            assert_eq!(pc.value, "one");
        }
    }
}
