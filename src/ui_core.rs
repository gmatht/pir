//! Viewport helpers and cell-formatting functions used by the TUI.
//!
//! This module provides the core viewport-algorithm building blocks
//! (`main_col_window`, `right_nonblank_end`) and display helpers
//! (`format_cell_display`, `normalize_inline_text`, …). It is imported
//! into `crate::ui` via `use crate::ui_core::*`.

use crate::formula::cell_effective_display;
use crate::grid::{
    CellAddr, GridBox as Grid, NumberFormat, SheetCursor, TextAlign,
    FOOTER_ROWS, HEADER_ROWS, MARGIN_COLS,
};
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
    // Extend at least to the cursor, plus extra so the viewport shows
    // context ahead.  Use a wider window so content columns just past
    // the cursor (e.g. column F when the cursor is at A) are visible
    // without requiring the user to scroll right.
    let hi = (cursor_main_col + 8).min(mc_u32.saturating_sub(1));
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
        let _sign = if digits.starts_with('-') { "-" } else { "" };
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

    if let Some(_dot_pos) = t.find('.') {
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

// ── Viewport functions (shared by ratatui and pancurses backends) ─────────

pub fn visible_row_indices(
    state: &SheetState,
    cursor: SheetCursor,
    dim: usize,
    prev_start: usize,
) -> (Vec<usize>, usize) {
    let g = &state.grid;
    let hr = HEADER_ROWS;
    let mr = g.main_rows();
    let main_order = g.sorted_main_rows();
    let mut header_rows = Vec::new();
    let mut footer_rows = Vec::new();
    for (addr, _) in g.iter_nonempty() {
        match addr {
            CellAddr::Header { row, .. } => header_rows.push(row as usize),
            CellAddr::Footer { row, .. } => footer_rows.push(hr + mr + row as usize),
            _ => {}
        }
    }
    if cursor.row < hr {
        let window = 5usize;
        let lo = cursor.row.saturating_sub(window / 2);
        let hi = cursor.row.min(hr - 1);
        for r in lo..=hi {
            if r < hr {
                header_rows.push(r);
            }
        }
        let so_far = header_rows.len() + main_order.len() + footer_rows.len();
        let can_add = dim.saturating_sub(so_far).min(hr.saturating_sub(hi + 1));
        for r in (hi + 1)..(hi + 1 + can_add) {
            header_rows.push(r);
        }
    } else if cursor.row >= hr + mr {
        let window = 5usize;
        let lo = cursor.row;
        let hi = (cursor.row + window / 2).min(hr + mr + FOOTER_ROWS - 1);
        for r in lo..=hi {
            if r >= hr + mr {
                footer_rows.push(r);
            }
        }
    }
    let content_count = header_rows.len() + main_order.len() + footer_rows.len();
    let blank_needed = dim.saturating_sub(content_count);
    if cursor.row >= hr + mr {
        let lo = cursor.row;
        let hi = (lo + 2).min(hr + mr + FOOTER_ROWS - 1);
        for i in 0..dim {
            let r = lo.saturating_sub(1 + i);
            if r >= hr + mr {
                footer_rows.push(r);
            }
        }
        let so_far = header_rows.len() + main_order.len() + footer_rows.len();
        if so_far < dim {
            let remaining = dim - so_far;
            for i in 0..remaining {
                let r = hi + 1 + i;
                if r < hr + mr + FOOTER_ROWS {
                    footer_rows.push(r);
                }
            }
        }
    } else {
        for i in 0..blank_needed {
            footer_rows.push(hr + mr + i);
        }
    }
    header_rows.sort_unstable();
    header_rows.dedup();
    footer_rows.sort_unstable();
    footer_rows.dedup();

    let mut display_rows: Vec<usize> =
        Vec::with_capacity(header_rows.len() + main_order.len() + footer_rows.len());
    display_rows.extend(header_rows);
    display_rows.extend(main_order.iter().copied().map(|r| hr + r));
    // Include the cursor row if it is a main row that sorted_main_rows
    // omitted (e.g. a blank row just added by grow_main_row_at_bottom).
    if (hr..hr + mr).contains(&cursor.row) && !display_rows.contains(&cursor.row) {
        display_rows.push(cursor.row);
    }
    display_rows.extend(footer_rows);

    let dim = dim.max(1).min(display_rows.len().max(1));
    if display_rows.len() <= dim {
        return (display_rows, 0);
    }

    let cur_display = if cursor.row < hr {
        cursor.row
    } else if cursor.row < hr + mr {
        hr + main_order
            .iter()
            .position(|&r| hr + r == cursor.row)
            .unwrap_or(0)
    } else {
        cursor.row
    };

    let cur_pos = display_rows
        .iter()
        .position(|&r| r == cur_display)
        .unwrap_or(0);
    let max_start = display_rows.len().saturating_sub(dim);
    let mut start = prev_start.min(max_start);
    if cur_pos < start {
        start = cur_pos;
    } else if cur_pos >= start + dim {
        start = cur_pos + 1 - dim;
    }

    (display_rows[start..start + dim].to_vec(), start)
}

pub fn visible_col_indices(
    state: &SheetState,
    cursor: SheetCursor,
    dim: usize,
    prev_start: usize,
) -> (Vec<usize>, usize) {
    let g = &state.grid;
    let lm = MARGIN_COLS;
    let mc = g.main_cols();
    let rm = MARGIN_COLS;
    let total = lm + mc + rm;
    let dim = dim.max(1).min(total.max(1));
    let cur = cursor.col.min(total.saturating_sub(1));
    let cursor_in_left = cursor.col < lm;
    let cursor_in_right = cursor.col >= lm + mc;

    if total <= dim {
        return ((0..total).collect(), 0);
    }

    let (_main_lo, main_hi) = main_col_window(state, cursor);
    let right_start = lm + mc;
    let mut right_band: Vec<usize> = match right_nonblank_end(state) {
        Some(end) => (0..=end).map(|i| right_start + i).collect(),
        None => Vec::new(),
    };
    let blank_right = right_nonblank_end(state)
        .map(|end| end + 1)
        .filter(|&i| i < rm)
        .map(|i| right_start + i)
        .unwrap_or(right_start);
    if cursor_in_right {
        let rcur = cur.saturating_sub(right_start);
        for i in 0..=rcur {
            right_band.push(right_start + i);
        }
        right_band.push(blank_right);
    }
    let left_band: Vec<usize> = if cursor_in_left {
        let start = cursor.col;
        let end = lm.saturating_sub(1);
        let window = lm;
        if end.saturating_sub(start) <= window {
            (start..=end).collect()
        } else {
            let half = window / 2;
            let lo = start.saturating_sub(half);
            let hi = (lo + window).min(end);
            (lo..=hi).collect()
        }
    } else {
        Vec::new()
    };
    let main_span = (main_hi.saturating_sub(0) + 1) as usize;
    let mut stable_band = Vec::with_capacity(
        (if lm > 0 { 1 } else { 0 })
            + left_band.len()
            + main_span
            + right_band.len(),
    );
    if lm > 0 {
        stable_band.push(lm - 1);
    }
    stable_band.extend(left_band.iter().copied());
    stable_band.extend((0..=main_hi).map(|ci| lm + ci as usize));
    // Fill remaining viewport space.  Prefer main columns so that
    // non-blank data columns past the cursor window are never hidden
    // behind right-margin filler columns.
    {
        let total_so_far = stable_band.len();
        if total_so_far < dim {
            let blank_cols_needed = dim - total_so_far;
            let hi = main_hi as usize;
            let extra_main = mc.saturating_sub(hi + 1);
            let main_fill = blank_cols_needed.min(extra_main);
            for ci in (hi + 1)..(hi + 1 + main_fill) {
                stable_band.push(lm + ci);
            }
            let rm_fill = blank_cols_needed.saturating_sub(main_fill).min(rm);
            for i in 0..rm_fill {
                stable_band.push(right_start + i);
            }
        }
    }
    stable_band.extend(right_band.iter().copied());
    stable_band.sort_unstable();
    stable_band.dedup();
    if stable_band.len() <= dim && stable_band.contains(&cur) {
        return (stable_band, 0);
    }

    let mut reserved: Vec<usize> = left_band;
    for ci in 0..=main_hi {
        let gc = lm + ci as usize;
        if !reserved.contains(&gc) {
            reserved.push(gc);
        }
    }
    if lm > 0 && !reserved.contains(&(lm - 1)) {
        reserved.push(lm - 1);
    }
    if !cursor_in_right && rm > 0 && !reserved.iter().any(|&c| c == blank_right) {
        let mut cand = reserved.clone();
        cand.push(blank_right);
        cand.sort_unstable();
        cand.dedup();
        if cand.len() < dim {
            let available = dim.saturating_sub(cand.len()).max(1);
            let filtered_len = (0..total).filter(|c| !cand.iter().any(|p| p == c)).count();
            if filtered_len <= available {
                reserved = cand;
            }
        }
    }
    reserved.sort_unstable();
    reserved.dedup();

    let available = dim.saturating_sub(reserved.len()).max(1);
    let filtered: Vec<usize> = (0..total)
        .filter(|c| !reserved.iter().any(|p| p == c))
        .collect();
    if filtered.is_empty() {
        return (reserved, 0);
    }

    let cur_pos = match filtered.binary_search(&cur) {
        Ok(i) => i,
        Err(i) => i.min(filtered.len().saturating_sub(1)),
    };
    let max_start = filtered.len().saturating_sub(available);
    let mut start = prev_start.min(max_start);
    if cur_pos < start || cur_pos >= start + available {
        start = cur_pos.saturating_sub(available / 2).min(max_start);
    }
    let end = (start + available).min(filtered.len());

    let mut out: Vec<usize> = filtered[start..end].to_vec();

    if end >= filtered.len() && out.last().copied().unwrap_or(0) <= right_start.saturating_sub(1) {
        let right_start_col = right_start;
        for i in 0..MARGIN_COLS {
            let gc = right_start_col + i;
            if reserved.contains(&gc) || out.contains(&gc) {
                continue;
            }
            out.push(gc);
            if out.len() + reserved.len() >= dim * 2 {
                break;
            }
        }
    }

    out.extend(reserved);
    out.sort_unstable();
    (out, start)
}

pub fn visible_cols_render_width(grid: &Grid, cols: &[usize]) -> usize {
    let lm = MARGIN_COLS;
    let mc = grid.main_cols();
    let show_right_divider = cols.contains(&(lm + mc));
    cols.iter()
        .enumerate()
        .map(|(i, &c)| {
            let sep = if i + 1 >= cols.len() {
                0
            } else if (c == lm - 1 && lm > 0 && cols.contains(&lm))
                || (c == lm + mc - 1 && show_right_divider)
            {
                2
            } else {
                1
            };
            grid.col_width(c).max(1) + sep
        })
        .sum()
}

pub fn trim_visible_cols_to_width(grid: &Grid, cols: &mut Vec<usize>, cursor_col: usize, width: usize) {
    // The left-margin boundary column [A should never be removed when the
    // cursor is in the left margin — it is the visual anchor to the main grid.
    let boundary = MARGIN_COLS.saturating_sub(1);
    let protect_boundary = cursor_col < MARGIN_COLS;
    while cols.len() > 1 && visible_cols_render_width(grid, cols) > width {
        let first = cols.first().copied().unwrap_or(cursor_col);
        let last = cols.last().copied().unwrap_or(cursor_col);
        if last > cursor_col {
            if protect_boundary && last == boundary {
                let mut removed = false;
                for j in (0..cols.len().saturating_sub(1)).rev() {
                    if cols[j] > cursor_col {
                        cols.remove(j);
                        removed = true;
                        break;
                    }
                }
                if removed {
                    continue;
                }
                cols.remove(0);
                continue;
            }
            cols.pop();
        } else if first < cursor_col {
            cols.remove(0);
        } else {
            break;
        }
    }
}
