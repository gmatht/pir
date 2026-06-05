//! Viewport helpers and cell-formatting functions used by the TUI.
//!
//! This module provides the core viewport-algorithm building blocks
//! (`main_col_window`, `right_nonblank_end`) and display helpers
//! (`format_cell_display`, `normalize_inline_text`, …). It is imported
//! into `crate::ui` via `use crate::ui_core::*`.

use crate::grid::{
    CellAddr, GridBox as Grid, NumberFormat, SheetCursor, TextAlign,
    MARGIN_COLS,
};
use crate::formula::cell_effective_display;
use crate::ops::SheetState;

// ---------------------------------------------------------------------------
// Re-exports from addr.rs (convenience aliases)
// ---------------------------------------------------------------------------

pub use crate::addr::ui_column_fragment as col_header_label;
pub use crate::addr::ui_row_label as sheet_row_label;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Number of blank (non-content) footer rows to show below the last content row.
pub const NAV_BLANK_ROWS: usize = 2;

/// Number of blank (non-content) right-margin columns to show to the right of
/// the last content column.
pub const NAV_BLANK_COLS: usize = 2;

/// Width reserved for the row-label gutter on the left side of the grid
/// (enough for `~N`, ` N`, `_N` with a little padding).
pub const ROW_LABEL_CHARS: usize = 5;

// ---------------------------------------------------------------------------
// Inter-column trailing type
// ---------------------------------------------------------------------------

/// Describes what separator/gap should be drawn after a column in the grid
/// viewport.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterColumnTrailing {
    /// This is the last visible column in the row — no separator.
    EndOfVisibleRow,
    /// A single ASCII space between adjacent main columns.
    AsciiSpace,
    /// A vertical pipe (`│`) followed by a space, used at the boundary
    /// between the left margin and the main region (or main/right-margin).
    PipeAndSpace,
}

// ---------------------------------------------------------------------------
// Viewport helpers
// ---------------------------------------------------------------------------

/// Return the range `(lo, hi)` of **main** (0-based main-relative) columns
/// that form the “stable main column window” for the current cursor.
///
/// The range always starts at 0 (the first main column) so that columns
/// between the left-margin anchor and the cursor are never skipped.
/// `hi` extends at least to the cursor's main column.
pub fn main_col_window(state: &SheetState, cursor: SheetCursor) -> (u32, u32) {
    let g = &state.grid;
    let lm = MARGIN_COLS;
    let mc = g.main_cols();
    if mc == 0 {
        return (0, 0);
    }
    let mc_u32 = mc as u32;

    let cursor_main_col = if cursor.col < lm {
        0u32
    } else if cursor.col < lm + mc {
        (cursor.col - lm) as u32
    } else {
        mc_u32.saturating_sub(1)
    };

    // Always start from the first main column so that no main columns
    // are skipped between the left-margin anchor and the cursor.
    let lo = 0u32;
    // Extend at least to the cursor, plus a few extra so the viewport
    // shows context ahead.
    let hi = (cursor_main_col + 4).min(mc_u32.saturating_sub(1));
    (lo, hi)
}

/// Return the **right-margin-relative** index of the last non-blank right
/// margin column, or `None` if every right-margin column is empty.
///
/// This only considers cells on **main rows** (the margin columns that sit
/// alongside the main region). Header/footer right-margin cells are **not**
/// counted here.
pub fn right_nonblank_end(state: &SheetState) -> Option<usize> {
    let g = &state.grid;
    let lm = MARGIN_COLS;
    let mc = g.main_cols();
    let right_start = lm + mc;
    // Walk right margin columns from the outermost inward.
    for i in (0..MARGIN_COLS).rev() {
        let global_col = right_start + i;
        if g.logical_col_has_content(global_col) {
            return Some(i);
        }
    }
    None
}

/// Determine what kind of trailing separator to draw after the column at
/// viewport position `vp_index` (with global column index `global_col`).
pub fn inter_column_trailing_after_data_cell(
    vp_index: usize,
    global_col: usize,
    col_ixs: &[usize],
    lm: usize,
    mc: usize,
    show_right_divider: bool,
) -> InterColumnTrailing {
    // Last visible column → no trailing separator.
    if vp_index + 1 >= col_ixs.len() {
        return InterColumnTrailing::EndOfVisibleRow;
    }

    let next = col_ixs[vp_index + 1];

    // Boundary between left margin and main region: show pipe.
    if global_col == lm.saturating_sub(1) && lm > 0 && next == lm {
        return InterColumnTrailing::PipeAndSpace;
    }

    // Boundary between main and right margin region: show pipe if the
    // divider is requested.
    if global_col == lm + mc - 1 && show_right_divider && next == lm + mc {
        return InterColumnTrailing::PipeAndSpace;
    }

    // Default: single space between adjacent columns.
    InterColumnTrailing::AsciiSpace
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

/// Apply cell-level formatting (number format, alignment heuristics) to the
/// given display `text` and return the formatted string.
pub fn format_cell_display(grid: &Grid, addr: &CellAddr, text: String) -> String {
    let fmt = grid.format_for_addr(addr);
    match fmt.number {
        Some(NumberFormat::Fixed { decimals }) => {
            match text.trim().parse::<f64>() {
                Ok(v) if v.is_finite() => {
                    format!("{:.decimals$}", v, decimals = decimals)
                }
                Ok(_) => {
                    // Overflow to infinity — try scientific notation.
                    if let Some(sci) = exponential_numeric_display(text.trim(), 20) {
                        sci
                    } else {
                        text
                    }
                }
                Err(_) => {
                    // Not numeric — could be a formula result like "1/7".
                    text
                }
            }
        }
        Some(NumberFormat::Currency { decimals }) => {
            match text.trim().parse::<f64>() {
                Ok(v) if v.is_finite() => {
                    let sign = if v < 0.0 { "-" } else { "" };
                    format!("{}{:.decimals$}", sign, v.abs(), decimals = decimals)
                }
                Ok(_) => text,
                Err(_) => text,
            }
        }
        Some(NumberFormat::Rational) | Some(NumberFormat::DecimalGeneric) | None => text,
    }
}

/// Normalise whitespace for inline (single-line) display: collapse runs of
/// whitespace into a single space, trim leading/trailing whitespace, and
/// replace newlines/carriage-returns with spaces.
pub fn normalize_inline_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_was_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !prev_was_space {
                out.push(' ');
                prev_was_space = true;
            }
        } else {
            out.push(ch);
            prev_was_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// If the stored value is a plain literal (not a formula), return its
/// display width in monospace columns based on the raw string.  For formula
/// cells this returns `None` so the caller falls back to
/// [`normalize_inline_text`] on the evaluated result.
pub fn measured_width_text_for_stored_literal(raw: &str) -> Option<String> {
    if raw.starts_with('=') {
        return None;
    }
    let normalised = normalize_inline_text(raw);
    if normalised.is_empty() {
        None
    } else {
        Some(normalised)
    }
}

/// Determine the effective alignment for a cell based on its formatting and
/// formatted display text.
pub fn effective_cell_align(grid: &Grid, addr: &CellAddr, formatted: &str) -> Option<TextAlign> {
    let fmt = grid.format_for_addr(addr);
    match fmt.align {
        Some(TextAlign::Default) | None => {
            if formatted.trim().parse::<f64>().is_ok() {
                Some(TextAlign::Right)
            } else if formatted.starts_with('=') {
                Some(TextAlign::Left)
            } else {
                None
            }
        }
        Some(align) => Some(align),
    }
}

/// Check whether truncating `text` to `width` monospace columns would hide a
/// decimal point (i.e. the text contains a `.` beyond the truncation point).
pub fn would_ellipsis_hide_decimal_point(text: &str, width: usize) -> bool {
    use unicode_width::UnicodeWidthChar;
    use unicode_width::UnicodeWidthStr;
    if width == 0 {
        return false;
    }
    let t = text.trim();
    let w = t.width();
    if w <= width {
        return false;
    }
    let mut col = 0usize;
    for ch in t.chars() {
        if ch == '.' && col >= width {
            return true;
        }
        col += UnicodeWidthChar::width(ch).unwrap_or(1);
    }
    false
}

/// Try to render a numeric string in exponential (scientific) notation so it
/// fits within `width` monospace columns.  Returns `None` if the input is not
/// numeric or cannot be shortened usefully.
pub fn exponential_numeric_display(text: &str, width: usize) -> Option<String> {
    use unicode_width::UnicodeWidthChar;
    use unicode_width::UnicodeWidthStr;

    let t = text.trim();
    if t.is_empty() || width < 4 {
        return None;
    }

    // Try f64 parsing first.
    if let Ok(v) = t.parse::<f64>() {
        if v.is_finite() {
            let sci = format!("{:e}", v);
            if sci.width() <= width {
                return Some(sci);
            }
            // Try to compact the mantissa.
            let mut parts = sci.splitn(2, 'e');
            let mantissa = parts.next()?;
            let exponent = parts.next()?;
            let exp_str = format!("e{}", exponent);
            let exp_w = exp_str.width();
            let max_mantissa_w = width.saturating_sub(exp_w).max(1);
            let mantissa_compact = if mantissa.width() > max_mantissa_w {
                let target = max_mantissa_w;
                let mut out = String::with_capacity(target);
                let mut col = 0usize;
                for ch in mantissa.chars() {
                    let w = UnicodeWidthChar::width(ch).unwrap_or(1);
                    if col + w > target { break; }
                    out.push(ch);
                    col += w;
                }
                out
            } else {
                mantissa.to_string()
            };
            let result = format!("{}{}", mantissa_compact, exp_str);
            if result.width() <= width {
                return Some(result);
            }
        }
    }

    // For numbers that overflow f64 (too large), manually create scientific
    // notation from the decimal string representation.
    let digits: String = t.chars().filter(|c| *c == '-' || c.is_ascii_digit()).collect();
    if digits.is_empty() || digits == "-" {
        return None;
    }
    // Count trailing zeros to guess the magnitude.
    let trimmed = digits.trim_start_matches('-');
    let non_zero = trimmed.trim_end_matches('0');
    let trailing_zeros = trimmed.len().saturating_sub(non_zero.len());
    let significant: &str = if non_zero.is_empty() { "0" } else { non_zero };

    if trailing_zeros > 0 && !significant.is_empty() && significant != "0" {
        let sign = if digits.starts_with('-') { "-" } else { "" };
        // Format as: first digit + . + remaining significant digits + e + exponent
        let mut mantissa = String::with_capacity(significant.len() + 2);
        mantissa.push(significant.chars().next()?);
        if significant.len() > 1 {
            mantissa.push('.');
            mantissa.push_str(&significant[1..]);
        }
        let exponent = significant.len() - 1 + trailing_zeros;
        let result = format!("{mantissa}e{exponent}");
        if result.width() <= width {
            return Some(result);
        }
        // Try shortening mantissa.
        if mantissa.len() > 2 {
            mantissa.truncate(2);
            let result = format!("{mantissa}e{exponent}");
            if result.width() <= width {
                return Some(result);
            }
        }
    }

    None
}

/// Like [`exponential_numeric_display`] but accepts an optional rational
/// value hint to guide formatting.
pub fn exponential_numeric_display_with_hint(
    text: &str,
    width: usize,
    hint: Option<f64>,
) -> Option<String> {
    // If direct text parsing fails, try the hint value.
    if text.parse::<f64>().ok().filter(|v| v.is_finite()).is_none() {
        if let Some(hv) = hint.filter(|v| v.is_finite()) {
            return exponential_numeric_display(&hv.to_string(), width);
        }
    }
    exponential_numeric_display(text, width)
}

/// Try to shrink a numeric display string so it fits within `width` monospace
/// columns by rounding decimal places or switching to a shorter format.
pub fn shrink_numeric_display(text: &str, width: usize) -> Option<String> {
    use unicode_width::UnicodeWidthStr;

    let t = text.trim();
    if t.is_empty() || width < 2 {
        return None;
    }
    if t.width() <= width {
        return Some(t.to_string());
    }

    // Complex number: "a+bi" — try shrinking real part.
    if let Some(plus_idx) = t.rfind('+').or_else(|| t.find('-').filter(|&i| i > 0)) {
        if t.ends_with('i') {
            let real_part = &t[..plus_idx];
            let imag_part = &t[plus_idx..];
            let real_shrunk =
                shrink_numeric_display(real_part, width.saturating_sub(imag_part.width()))?;
            let result = format!("{}{}", real_shrunk, imag_part);
            if result.width() <= width {
                return Some(result);
            }
        }
    }

    if let Some(sci) = exponential_numeric_display(t, width) {
        return Some(sci);
    }

    if let Some(dot_pos) = t.find('.') {
        let trimmed = t.trim_end_matches('0');
        if trimmed != t && trimmed.width() <= width {
            return Some(trimmed.to_string());
        }
    }

    None
}

/// Truncate `text` to fit within `width` monospace columns, appending `…`
/// when truncation occurs.
pub fn truncate_with_ellipsis(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    use unicode_width::UnicodeWidthStr;
    if width == 0 {
        return String::new();
    }
    if text.width() <= width {
        return text.to_string();
    }
    let target = width.saturating_sub(1).max(0);
    let mut out = String::with_capacity(width);
    let mut col = 0usize;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(1);
        if col + w > target {
            break;
        }
        out.push(ch);
        col += w;
    }
    out.push('…');
    out
}

/// Pad or truncate `text` so it occupies exactly `width` monospace columns,
/// respecting the given alignment.  Returns the display string (pre-padded /
/// post-padded with spaces).
pub fn align_cell_display(text: String, width: usize, align: Option<TextAlign>) -> String {
    use unicode_width::UnicodeWidthStr;
    if width == 0 {
        return String::new();
    }
    let w = text.width();
    if w >= width {
        return truncate_with_ellipsis(&text, width);
    }
    let pad = width.saturating_sub(w);
    match align {
        Some(TextAlign::Left) | None => {
            format!("{}{}", text, " ".repeat(pad))
        }
        Some(TextAlign::Right) => {
            format!("{}{}", " ".repeat(pad), text)
        }
        Some(TextAlign::Center) => {
            let left = pad / 2;
            let right = pad - left;
            format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
        }
        Some(TextAlign::Default) => {
            format!("{}{}", text, " ".repeat(pad))
        }
    }
}

/// Compute the rendered width (in monospace columns) needed to display the
/// widest non-empty cell in the given global column, including a one-character
/// padding gap.  Returns `None` if the column has no content at all.
///
/// This mirrors `App::rendered_width_for_column` for use outside the ratatui
/// backend (notably the pancurses backend).
pub fn rendered_width_for_column(grid: &Grid, global_col: usize) -> Option<usize> {
    use unicode_width::UnicodeWidthStr;

    let mut maxw = 0usize;
    let mut saw_content = false;
    let main_cols = grid.main_cols();

    for (addr, _) in grid.iter_nonempty() {
        match &addr {
            CellAddr::Header { col, .. } | CellAddr::Footer { col, .. }
                if col.to_global(main_cols) == global_col =>
            {
                let mut measured = None;
                if let Some(raw) = grid.get(&addr) {
                    measured = measured_width_text_for_stored_literal(&raw);
                }
                let val = measured.unwrap_or_else(||
                    normalize_inline_text(&cell_effective_display(grid, &addr)));
                if !val.is_empty() {
                    saw_content = true;
                    maxw = maxw.max(val.width() + 1);
                }
            }
            _ => {}
        }
    }

    for r in 0..grid.main_rows() {
        if global_col < MARGIN_COLS {
            let addr = CellAddr::Left { col: global_col, row: r as u32 };
            let mut measured = None;
            if let Some(raw) = grid.get(&addr) {
                measured = measured_width_text_for_stored_literal(&raw);
            }
            let val = measured.unwrap_or_else(||
                normalize_inline_text(&cell_effective_display(grid, &addr)));
            if !val.is_empty() {
                saw_content = true;
                maxw = maxw.max(val.width() + 1);
            }
        } else if global_col < MARGIN_COLS + main_cols {
            let addr = CellAddr::Main { row: r as u32, col: (global_col - MARGIN_COLS) as u32 };
            let mut measured = None;
            if let Some(raw) = grid.get(&addr) {
                measured = measured_width_text_for_stored_literal(&raw);
            }
            let val = measured.unwrap_or_else(||
                normalize_inline_text(&cell_effective_display(grid, &addr)));
            if !val.is_empty() {
                saw_content = true;
                maxw = maxw.max(val.width() + 1);
            }
        } else {
            let addr = CellAddr::Right { col: global_col - MARGIN_COLS - main_cols, row: r as u32 };
            let mut measured = None;
            if let Some(raw) = grid.get(&addr) {
                measured = measured_width_text_for_stored_literal(&raw);
            }
            let val = measured.unwrap_or_else(||
                normalize_inline_text(&cell_effective_display(grid, &addr)));
            if !val.is_empty() {
                saw_content = true;
                maxw = maxw.max(val.width() + 1);
            }
        }
    }

    saw_content.then_some(maxw.max(4))
}

/// Adjust column widths for the visible columns so they fit within
/// `data_width` characters.  Each column's width is capped by its rendered
/// content width (computed via [`rendered_width_for_column`]) and by the
/// grid's `max_col_width`.
///
/// When the total desired width exceeds the budget the function distributes
/// space proportionally, giving priority to the column under the cursor.
///
/// This mirrors `App::fit_visible_columns_capped` for use outside the ratatui
/// backend.
pub fn fit_visible_columns_capped(
    grid: &mut Grid,
    col_ixs: &[usize],
    data_width: usize,
    cursor_col: usize,
) {
    if col_ixs.is_empty() {
        return;
    }
    let n = col_ixs.len();
    let gaps = n.saturating_sub(1);
    let budget = data_width.saturating_sub(gaps);

    let mut desired: Vec<(usize, usize)> = Vec::with_capacity(n);
    for &c in col_ixs {
        if let Some(maxw) = rendered_width_for_column(grid, c) {
            let cap = maxw.min(grid.max_col_width());
            desired.push((c, cap));
        } else {
            desired.push((c, 4));
        }
    }

    let total_desired: usize = desired.iter().map(|(_, w)| *w).sum();
    if total_desired <= budget {
        for (c, w) in desired {
            grid.set_col_width(c, Some(w));
        }
        return;
    }

    let pivot_ix = if let Some(p) = col_ixs.iter().position(|&c| c == cursor_col) {
        p
    } else {
        let mut best = 0usize;
        let mut best_dist = usize::MAX;
        for (i, &c) in col_ixs.iter().enumerate() {
            let dist = if c > cursor_col { c - cursor_col } else { cursor_col - c };
            if dist < best_dist {
                best_dist = dist;
                best = i;
            }
        }
        best
    };

    let mut left = pivot_ix;
    let mut right = pivot_ix;
    let mut window_sum = desired[pivot_ix].1;
    let mut prefer_right = true;
    loop {
        let can_right = right + 1 < desired.len();
        let can_left = left > 0;
        if !can_right && !can_left {
            break;
        }
        let mut expanded = false;
        let sides = if prefer_right { [1isize, -1isize] } else { [-1isize, 1isize] };
        for &side in &sides {
            if side > 0 && can_right {
                let cand_w = desired[right + 1].1;
                let win_len = right.saturating_sub(left).saturating_add(1);
                let new_win_len = win_len.saturating_add(1);
                let outside = desired.len().saturating_sub(new_win_len);
                if window_sum.saturating_add(cand_w).saturating_add(outside) <= budget {
                    right += 1;
                    window_sum = window_sum.saturating_add(cand_w);
                    expanded = true;
                    break;
                }
            } else if side < 0 && can_left {
                let cand_w = desired[left - 1].1;
                let win_len = right.saturating_sub(left).saturating_add(1);
                let new_win_len = win_len.saturating_add(1);
                let outside = desired.len().saturating_sub(new_win_len);
                if window_sum.saturating_add(cand_w).saturating_add(outside) <= budget {
                    left -= 1;
                    window_sum = window_sum.saturating_add(cand_w);
                    expanded = true;
                    break;
                }
            }
        }
        if !expanded {
            break;
        }
        prefer_right = !prefer_right;
    }

    let mut allocations: std::collections::HashMap<usize, usize> =
        desired.iter().map(|(c, _)| (*c, 1usize)).collect();
    let mut rem_budget = budget.saturating_sub(desired.len());

    let main_cols = grid.main_cols();
    let mut cols: Vec<(usize, usize, usize, usize)> = Vec::new();
    for i in left..=right {
        let (col, cap) = desired[i];
        let mut looks_like_date = false;

        for (addr, _) in grid.iter_nonempty() {
            match &addr {
                CellAddr::Header { col: hcol, .. } | CellAddr::Footer { col: hcol, .. }
                    if hcol.to_global(grid.main_cols()) == col =>
                {
                    if let Some(raw) = grid.get(&addr) {
                        let t = raw.trim();
                        if !crate::formula::is_formula(t) {
                            if crate::formula::parse_numeric_or_date_literal(t).is_some() {
                                looks_like_date = true;
                                break;
                            }
                        }
                    }
                    let val = normalize_inline_text(&cell_effective_display(grid, &addr));
                    let t = val.trim();
                    let bytes = t.as_bytes();
                    if bytes.len() >= 10 {
                        for i in 0..=bytes.len().saturating_sub(10) {
                            if (bytes[i + 4] == b'-' || bytes[i + 4] == b'/' || bytes[i + 4] == b'\\')
                                && (bytes[i + 7] == b'-' || bytes[i + 7] == b'/' || bytes[i + 7] == b'\\')
                            {
                                if bytes[i..i + 4].iter().all(|b| b.is_ascii_digit())
                                    && bytes[i + 5..i + 7].iter().all(|b| b.is_ascii_digit())
                                    && bytes[i + 8..i + 10].iter().all(|b| b.is_ascii_digit())
                                {
                                    looks_like_date = true;
                                    break;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            if looks_like_date {
                break;
            }
        }

        if !looks_like_date {
            for r in 0..grid.main_rows() {
                let raw_val = if col < MARGIN_COLS {
                    grid.get(&CellAddr::Left { col, row: r as u32 })
                } else if col < MARGIN_COLS + main_cols {
                    grid.get(&CellAddr::Main { row: r as u32, col: (col - MARGIN_COLS) as u32 })
                } else {
                    grid.get(&CellAddr::Right { col: col - MARGIN_COLS - main_cols, row: r as u32 })
                };

                if let Some(raw) = raw_val {
                    let t = raw.trim();
                    if !crate::formula::is_formula(t) {
                        if crate::formula::parse_numeric_or_date_literal(t).is_some() {
                            looks_like_date = true;
                            break;
                        }
                    }
                }

                let cell_addr = if col < MARGIN_COLS {
                    CellAddr::Left { col, row: r as u32 }
                } else if col < MARGIN_COLS + main_cols {
                    CellAddr::Main { row: r as u32, col: (col - MARGIN_COLS) as u32 }
                } else {
                    CellAddr::Right { col: col - MARGIN_COLS - main_cols, row: r as u32 }
                };
                let val = normalize_inline_text(&cell_effective_display(grid, &cell_addr));
                let t = val.trim();
                let bytes = t.as_bytes();
                if bytes.len() >= 10 {
                    for i in 0..=bytes.len().saturating_sub(10) {
                        if (bytes[i + 4] == b'-' || bytes[i + 4] == b'/' || bytes[i + 4] == b'\\')
                            && (bytes[i + 7] == b'-' || bytes[i + 7] == b'/' || bytes[i + 7] == b'\\')
                        {
                            if bytes[i..i + 4].iter().all(|b| b.is_ascii_digit())
                                && bytes[i + 5..i + 7].iter().all(|b| b.is_ascii_digit())
                                && bytes[i + 8..i + 10].iter().all(|b| b.is_ascii_digit())
                            {
                                looks_like_date = true;
                                break;
                            }
                        }
                    }
                }
                if looks_like_date {
                    break;
                }
            }
        }

        let cap_used = if looks_like_date { grid.max_col_width() } else { cap };
        let need = cap_used.saturating_sub(1);
        let weight = if looks_like_date { need.saturating_mul(8).max(1) } else { need.max(1) };
        cols.push((col, cap_used, need, weight));
    }

    let pivot_col = desired[pivot_ix].0;
    if rem_budget > 0 {
        if let Some(pos) = cols.iter().position(|(col, ..)| *col == pivot_col) {
            let need = cols[pos].2;
            if need > 0 {
                let give = rem_budget.min(need);
                if give > 0 {
                    allocations.insert(pivot_col, allocations[&pivot_col].saturating_add(give));
                    rem_budget = rem_budget.saturating_sub(give);
                    cols[pos].2 = need.saturating_sub(give);
                }
            }
        }
    }

    while rem_budget > 0 {
        let total_weight: usize = cols.iter()
            .map(|(_, _, need, weight)| if *need > 0 { *weight } else { 0 })
            .sum();
        if total_weight == 0 {
            break;
        }

        let mut given = 0usize;
        let mut remainders: Vec<(usize, usize)> = Vec::new();
        for (col, _cap, need, weight) in cols.iter_mut() {
            if *need == 0 {
                continue;
            }
            let numerator = rem_budget.saturating_mul(*weight);
            let base = numerator / total_weight;
            let rem = numerator % total_weight;
            let give = base.min(*need);
            if give > 0 {
                let entry = allocations.entry(*col).or_insert(1);
                *entry = entry.saturating_add(give);
                *need = need.saturating_sub(give);
                given = given.saturating_add(give);
            }
            if *need > 0 {
                remainders.push((*col, rem));
            }
        }

        rem_budget = rem_budget.saturating_sub(given);
        if rem_budget == 0 {
            break;
        }

        remainders.sort_by(|a, b| b.1.cmp(&a.1));
        for (col, _rem) in remainders.iter() {
            if rem_budget == 0 {
                break;
            }
            if let Some((_c, _cap, need, _weight)) = cols.iter_mut().find(|(cc, _, _, _)| cc == col) {
                if *need > 0 {
                    let entry = allocations.entry(*col).or_insert(1);
                    *entry = entry.saturating_add(1);
                    *need = need.saturating_sub(1);
                    rem_budget = rem_budget.saturating_sub(1);
                }
            }
        }
    }

    for &c in col_ixs {
        if let Some(&w) = allocations.get(&c) {
            grid.set_col_width(c, Some(w));
        } else {
            grid.set_col_width(c, Some(1));
        }
    }
}

/// Split `text` at a unicode-width boundary, returning the prefix that fits
/// within `width` columns and the remaining suffix.
pub fn take_display_prefix(text: &str, width: usize) -> (String, String) {
    use unicode_width::UnicodeWidthChar;
    if width == 0 {
        return (String::new(), text.to_string());
    }
    let mut col = 0usize;
    let mut split_idx = text.len();
    for (i, ch) in text.char_indices() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(1);
        if col + w > width {
            split_idx = i;
            break;
        }
        col += w;
    }
    let (pre, suf) = text.split_at(split_idx);
    (pre.to_string(), suf.to_string())
}
