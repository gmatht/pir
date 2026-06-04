//! Shared cell-address parsing (Excel columns, global column suffixes, single-cell refs).

use crate::grid::{CellAddr, ColumnAddr, HEADER_ROWS};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct LogicalRow(pub usize);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct GlobalCol(pub usize);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct MainRows(pub usize);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct MainCols(pub usize);

#[inline(always)]
/// Parse Excel-style column name `A`..`ZZZ` → 0-based main column index.
pub fn parse_excel_column(name: &str) -> Option<u32> {
    let mut n: u32 = 0;
    for b in name.bytes() {
        if !b.is_ascii_uppercase() {
            return None;
        }
        n = n.checked_mul(26)?.checked_add((b - b'A') as u32 + 1)?;
    }
    Some(n - 1)
}

/// 0-based main column index → Excel column letters.
#[inline(always)]
pub fn excel_column_name(main_col_index: usize) -> String {
    let mut n = main_col_index + 1;
    let mut s = String::new();
    while n > 0 {
        n -= 1;
        s.push((b'A' + (n % 26) as u8) as char);
        n /= 26;
    }
    s.chars().rev().collect()
}

#[inline(always)]
/// Margin label (`A` nearest the main grid/right edge, up to `ZZ`).
pub fn mirror_margin_column_name(margin_col_index: usize, left_side: bool) -> String {
    // Map the margin_col_index (0..MARGIN_COLS-1) into a letter sequence.
    // If left_side is true, mirror the index (so 0 -> last, as in previous
    // behavior for small margins).
    let max = crate::grid::MARGIN_COLS;
    let idx = margin_col_index.min(max.saturating_sub(1));
    let mapped = if left_side {
        max.saturating_sub(1).saturating_sub(idx)
    } else {
        idx
    };
    // Use excel-style column naming for the mapped index (0 -> A, 25 -> Z,
    // 26 -> AA, ...). Reuse excel_column_name which is 0-based.
    excel_column_name(mapped)
}

/// UI-style column fragment for display and formulas.
pub fn ui_column_fragment(global_col: usize, main_cols: usize) -> String {
    let m = crate::grid::MARGIN_COLS;
    if global_col < m {
        format!("[{}", mirror_margin_column_name(global_col, true))
    } else if global_col < m + main_cols {
        excel_column_name(global_col - m)
    } else {
        format!(
            "]{}",
            mirror_margin_column_name(global_col - m - main_cols, false)
        )
    }
}

/// UI-style row label for the left gutter (`~N`, `1`, `_N`).
pub fn ui_row_label(logical_row: usize, main_rows: usize) -> String {
    let hr = crate::grid::HEADER_ROWS;
    if logical_row < hr {
        format!("~{}", hr - logical_row)
    } else if logical_row < hr + main_rows {
        format!("{}", logical_row - hr + 1)
    } else {
        let fr = logical_row - hr - main_rows;
        format!("_{}", fr + 1)
    }
}

/// Convert a logical sheet cursor (`row`, global `col`) to a concrete cell address.
pub fn sheet_cursor_to_addr(
    logical_row: LogicalRow,
    global_col: GlobalCol,
    main_rows: MainRows,
    main_cols: MainCols,
) -> CellAddr {
    use crate::grid::ColumnAddr;
    let logical_row = logical_row.0;
    let global_col = global_col.0;
    let main_rows = main_rows.0;
    let main_cols = main_cols.0;
    let hr = crate::grid::HEADER_ROWS;
    if logical_row < hr {
        CellAddr::Header {
            row: logical_row as u32,
            col: ColumnAddr::from_global(global_col, main_cols),
        }
    } else if logical_row < hr + main_rows {
        let main_row = logical_row - hr;
        if global_col < crate::grid::MARGIN_COLS {
            CellAddr::Left {
                col: global_col,
                row: main_row as u32,
            }
        } else if global_col < crate::grid::MARGIN_COLS + main_cols {
            CellAddr::Main {
                row: main_row as u32,
                col: (global_col - crate::grid::MARGIN_COLS) as u32,
            }
        } else {
            CellAddr::Right {
                col: global_col - crate::grid::MARGIN_COLS - main_cols,
                row: main_row as u32,
            }
        }
    } else {
        CellAddr::Footer {
            row: (logical_row - hr - main_rows) as u32,
            col: ColumnAddr::from_global(global_col, main_cols),
        }
    }
}

/// Convert a concrete cell address to a logical sheet cursor (`row`, global `col`).
pub fn addr_to_sheet_cursor(
    addr: &CellAddr,
    main_rows: MainRows,
    main_cols: MainCols,
) -> (LogicalRow, GlobalCol) {
    let main_rows = main_rows.0;
    let main_cols = main_cols.0;
    let row_col = match addr {
        CellAddr::Header { row, col } => {
            (LogicalRow(*row as usize), GlobalCol(col.to_global(main_cols)))
        }
        CellAddr::Footer { row, col } => (
            LogicalRow(crate::grid::HEADER_ROWS + main_rows + *row as usize),
            GlobalCol(col.to_global(main_cols)),
        ),
        CellAddr::Main { row, col } => (
            LogicalRow(crate::grid::HEADER_ROWS + *row as usize),
            GlobalCol(crate::grid::MARGIN_COLS + *col as usize),
        ),
        CellAddr::Left { col, row } => (
            LogicalRow(crate::grid::HEADER_ROWS + *row as usize),
            GlobalCol(*col as usize),
        ),
        CellAddr::Right { col, row } => (
            LogicalRow(crate::grid::HEADER_ROWS + *row as usize),
            GlobalCol(crate::grid::MARGIN_COLS + main_cols + *col as usize),
        ),
    };
    row_col
}

#[inline(always)]
/// Parse a column fragment at the start of a cell ref.
pub fn parse_ui_column_fragment(s: &str, main_cols: usize) -> Option<(u32, usize)> {
    if let Some(rest) = s.strip_prefix('[') {
        let col_len = rest.chars().take_while(|c| c.is_ascii_uppercase()).count();
        if col_len == 0 {
            return None;
        }
        let col = parse_mirror_margin_column_name(&rest[..col_len], true)?;
        return Some((col as u32, 1 + col_len));
    }
    if let Some(rest) = s.strip_prefix(']') {
        let col_len = rest.chars().take_while(|c| c.is_ascii_uppercase()).count();
        if col_len == 0 {
            return None;
        }
        let col = parse_mirror_margin_column_name(&rest[..col_len], false)?;
        return Some((
            crate::grid::MARGIN_COLS as u32 + main_cols as u32 + col as u32,
            1 + col_len,
        ));
    }
    let col_len = s.chars().take_while(|c| c.is_ascii_uppercase()).count();
    if col_len == 0 {
        return None;
    }
    let col = parse_excel_column(&s[..col_len])?;
    Some((crate::grid::MARGIN_COLS as u32 + col, col_len))
}

/// Back-compat alias for the UI-style column fragment.
pub fn ui_column_name(global_col: usize, main_cols: usize) -> String {
    ui_column_fragment(global_col, main_cols)
}

#[inline(always)]
/// Parse a sheet id prefix like `$12` at the start of `s`.
pub fn parse_sheet_id_prefix_at(s: &str) -> Option<(u32, usize)> {
    let bytes = s.as_bytes();
    if bytes.first().copied()? != b'$' {
        return None;
    }
    let mut i = 1usize;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 1 {
        return None;
    }
    let sheet_id = std::str::from_utf8(&bytes[1..i]).ok()?.parse().ok()?;
    Some((sheet_id, i))
}

#[inline(always)]
/// Parse a sheet-qualified cell ref like `$2:A1` at the start of `s`.
pub fn parse_sheet_qualified_cell_ref_at(
    s: &str,
    main_cols: usize,
) -> Option<(u32, CellAddr, usize)> {
    let (sheet_id, prefix_len) = parse_sheet_id_prefix_at(s)?;
    let rest = s.get(prefix_len..)?;
    let rest = rest.strip_prefix(':')?;
    let (addr, _, addr_len) = parse_cell_ref_at(rest, main_cols)?;
    Some((sheet_id, addr, prefix_len + 1 + addr_len))
}

pub(crate) fn parse_mirror_margin_column_name(name: &str, left_side: bool) -> Option<usize> {
    // Accept multi-letter uppercase sequences and parse them like Excel
    // columns, then map according to left_side mirroring.
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_uppercase()) {
        return None;
    }
    let parsed = parse_excel_column(name)? as usize; // 0-based
    if parsed >= crate::grid::MARGIN_COLS {
        return None;
    }
    let mapped = if left_side {
        crate::grid::MARGIN_COLS - 1 - parsed
    } else {
        parsed
    };
    Some(mapped)
}

/// Lock flags from Excel-style `$` in unprefixed A1 references (`$A$1` fixes both axes).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub struct A1RefLocks {
    pub col_absolute: bool,
    pub row_absolute: bool,
}

/// Parse one cell reference at the start of `s` (no leading whitespace).
/// Returns `(address, lock flags for main-style A1 translation, byte length consumed)`.
///
/// `$` locking applies only to plain `A1`/`$A1`/`A$1`/`$A$1` forms (no `[` / `]` / `~` / `_`).
#[inline(always)]
pub fn parse_cell_ref_at(s: &str, main_cols: usize) -> Option<(CellAddr, A1RefLocks, usize)> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return None;
    }

    let mut i: usize = 0;
    let prefix = match bytes[0] {
        b'[' => {
            i = 1;
            Some(true)
        }
        b']' => {
            i = 1;
            Some(false)
        }
        _ => None,
    };

    let mut locks = A1RefLocks::default();

    // Optional `$` before column letters (only plain `A1` style, not `[`/`]`).
    if prefix.is_none()
        && bytes.get(i) == Some(&b'$')
        && bytes
            .get(i + 1)
            .is_some_and(|b| b.is_ascii_uppercase())
    {
        locks.col_absolute = true;
        i += 1;
    }

    let col_byte_len = bytes
        .get(i..)?
        .iter()
        .take_while(|b| b.is_ascii_uppercase())
        .count();
    if col_byte_len == 0 {
        return None;
    }
    let col_name = s.get(i..i + col_byte_len)?;
    i += col_byte_len;

    let after_col = s.get(i..)?;

    // Header/footer: `A~1` / `A_1`
    if let Some(marker) = after_col
        .as_bytes()
        .first()
        .copied()
        .filter(|b| *b == b'~' || *b == b'_')
    {
        let row_digits = after_col[1..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .count();
        if row_digits == 0 {
            return None;
        }
        let row_num: usize = after_col[1..1 + row_digits].parse().ok()?;
        let row = if marker == b'~' {
            if row_num == 0 || row_num > crate::grid::HEADER_ROWS {
                return None;
            }
            (crate::grid::HEADER_ROWS - row_num) as u32
        } else {
            if row_num == 0 || row_num > crate::grid::FOOTER_ROWS {
                return None;
            }
            (row_num - 1) as u32
        };
        let col = match prefix {
            Some(true) => ColumnAddr::Left(parse_mirror_margin_column_name(col_name, true)?),
            Some(false) => parse_mirror_margin_column_name(col_name, false)
                .map(ColumnAddr::Right)
                .or_else(|| Some(ColumnAddr::Main(parse_excel_column(col_name)?)))?,
            None => ColumnAddr::Main(parse_excel_column(col_name)?),
        };
        let addr = if marker == b'~' {
            CellAddr::Header { row, col }
        } else {
            CellAddr::Footer { row, col }
        };
        return Some((addr, A1RefLocks::default(), i + 1 + row_digits));
    }

    // Optional `$` before row digits (main-style only).
    if prefix.is_none()
        && after_col
            .as_bytes()
            .first()
            .is_some_and(|b| *b == b'$')
        && after_col
            .as_bytes()
            .get(1)
            .is_some_and(|b| b.is_ascii_digit())
    {
        locks.row_absolute = true;
        i += 1;
    }

    let tail = s.get(i..)?;
    let row_digits = tail
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .count();
    if row_digits == 0 {
        return None;
    }
    let row_num: u32 = tail[..row_digits].parse().ok()?;
    if row_num == 0 {
        return None;
    }
    let addr = match prefix {
        Some(true) => CellAddr::Left {
            col: parse_mirror_margin_column_name(col_name, true)?,
            row: row_num - 1,
        },
        Some(false) => CellAddr::Right {
            col: parse_mirror_margin_column_name(col_name, false)?,
            row: row_num - 1,
        },
        None => CellAddr::Main {
            row: row_num - 1,
            col: parse_excel_column(col_name)?,
        },
    };
    Some((addr, locks, i + row_digits))
}

#[inline(always)]
pub fn cell_ref_text(addr: &CellAddr, main_cols: usize) -> String {
    // Keep this function quiet in normal operation; debug traces should go
    // to the configured debug log via stderr redirection in main.
    match addr {
        CellAddr::Header { row, col } => {
            let row = HEADER_ROWS - *row as usize;
            match col {
                ColumnAddr::Left(idx) => {
                    format!("[{}~{}", mirror_margin_column_name(*idx, true), row)
                }
                ColumnAddr::Main(idx) => {
                    format!("{}~{}", excel_column_name(*idx as usize), row)
                }
                ColumnAddr::Right(idx) => {
                    format!("]{}~{}", mirror_margin_column_name(*idx, false), row)
                }
            }
        }
        CellAddr::Footer { row, col } => {
            let row = *row as usize + 1;
            match col {
                ColumnAddr::Left(idx) => {
                    format!("[{}_{row}", mirror_margin_column_name(*idx, true))
                }
                ColumnAddr::Main(idx) => {
                    format!("{}_{row}", excel_column_name(*idx as usize))
                }
                ColumnAddr::Right(idx) => {
                    format!("]{}_{row}", mirror_margin_column_name(*idx, false))
                }
            }
        }
        CellAddr::Main { row, col } => format!("{}{}", excel_column_name(*col as usize), row + 1),
        CellAddr::Left { col, row } => format!(
            "[{}{}",
            mirror_margin_column_name(*col as usize, true),
            row + 1
        ),
        CellAddr::Right { col, row } => format!(
            "]{}{}",
            mirror_margin_column_name(*col as usize, false),
            row + 1
        ),
    }
}

/// Like [`cell_ref_text`], but preserves Excel `$` locks for main-region `A1` references.
pub fn formula_cell_ref_text(addr: &CellAddr, main_cols: usize, locks: A1RefLocks) -> String {
    match addr {
        CellAddr::Main { row, col } => {
            let col_s = excel_column_name(*col as usize);
            let row_s = (row + 1).to_string();
            format!(
                "{}{}{}{}",
                if locks.col_absolute { "$" } else { "" },
                col_s,
                if locks.row_absolute { "$" } else { "" },
                row_s
            )
        }
        _ => cell_ref_text(addr, main_cols),
    }
}

pub fn sheet_qualified_cell_ref_text(sheet_id: u32, addr: &CellAddr, main_cols: usize) -> String {
    format!("${sheet_id}:{}", cell_ref_text(addr, main_cols))
}

pub(crate) fn corner_locks_for_bbox(
    ra: u32,
    ca: u32,
    la: A1RefLocks,
    rb: u32,
    cb: u32,
    lb: A1RefLocks,
) -> (A1RefLocks, A1RefLocks) {
    let tl_r = ra.min(rb);
    let tl_c = ca.min(cb);
    let br_r = ra.max(rb);
    let br_c = ca.max(cb);
    let pick = |r: u32, c: u32| -> A1RefLocks {
        if r == ra && c == ca {
            la
        } else if r == rb && c == cb {
            lb
        } else {
            A1RefLocks::default()
        }
    };
    (pick(tl_r, tl_c), pick(br_r, br_c))
}

#[inline(always)]
/// Parse `A1:B2` at start of `s`; both ends must be main cells with lock metadata for translation.
pub fn parse_main_range_formula_at(
    s: &str,
) -> Option<(crate::grid::MainRange, A1RefLocks, A1RefLocks, usize)> {
    let (a, la, na) = parse_cell_ref_at(s, 0)?;
    let CellAddr::Main {
        row: ra,
        col: ca,
    } = a
    else {
        return None;
    };
    let rest = s.get(na..)?;
    let rest = rest.strip_prefix(':')?;
    let (b, lb, nb) = parse_cell_ref_at(rest, 0)?;
    let CellAddr::Main {
        row: rb,
        col: cb,
    } = b
    else {
        return None;
    };
    let r0 = ra.min(rb);
    let r1 = ra.max(rb);
    let c0 = ca.min(cb);
    let c1 = ca.max(cb);
    let range = crate::grid::MainRange {
        row_start: r0,
        row_end: r1 + 1,
        col_start: c0,
        col_end: c1 + 1,
    };
    let (locks_tl, locks_br) = corner_locks_for_bbox(ra, ca, la, rb, cb, lb);
    Some((range, locks_tl, locks_br, na + 1 + nb))
}

/// Parse `A1:B2` at start of `s`; both ends must be main cells. Returns range + consumed length.
#[inline(always)]
pub fn parse_main_range_at(s: &str) -> Option<(crate::grid::MainRange, usize)> {
    let (range, locks_a, locks_b, na) = parse_main_range_formula_at(s)?;
    let _ = (locks_a, locks_b);
    Some((range, na))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a1_roundtrip() {
        let (a, locks, n) = parse_cell_ref_at("A1", 1).unwrap();
        assert_eq!(n, 2);
        assert_eq!(a, CellAddr::Main { row: 0, col: 0 });
        assert_eq!(locks, A1RefLocks::default());
    }

    #[test]
    fn dollar_absolute_variants_parse() {
        let (_, l, _) = parse_cell_ref_at("$A1", 1).unwrap();
        assert!(l.col_absolute && !l.row_absolute);
        let (_, l, _) = parse_cell_ref_at("A$1", 1).unwrap();
        assert!(!l.col_absolute && l.row_absolute);
        let (_, l, _) = parse_cell_ref_at("$A$1", 1).unwrap();
        assert!(l.col_absolute && l.row_absolute);
        let (_, _, n) = parse_cell_ref_at("$A$1", 1).unwrap();
        assert_eq!(n, 4);
    }

    #[test]
    fn formula_cell_ref_preserves_locks_roundtrip() {
        let addr = CellAddr::Main { row: 0, col: 0 };
        let locks = A1RefLocks {
            col_absolute: true,
            row_absolute: true,
        };
        assert_eq!(formula_cell_ref_text(&addr, 1, locks), "$A$1");
    }

    #[test]
    fn main_range() {
        let (r, n) = parse_main_range_at("B2:A1").unwrap();
        assert_eq!(n, 5);
        assert_eq!(r.row_start, 0);
        assert_eq!(r.row_end, 2);
        assert_eq!(r.col_start, 0);
        assert_eq!(r.col_end, 2);
    }

    #[test]
    fn legacy_special_refs_parse() {
        assert_eq!(parse_cell_ref_at("A~1", 1).unwrap().2, 3);
        assert_eq!(parse_cell_ref_at("A_1", 1).unwrap().2, 3);
        assert_eq!(parse_cell_ref_at("[A1", 1).unwrap().2, 3);
        assert_eq!(parse_cell_ref_at("]A1", 1).unwrap().2, 3);
    }

    #[test]
    fn left_margin_is_mirrored_from_the_main_grid() {
        assert_eq!(mirror_margin_column_name(0, true), "ZZ");
        assert_eq!(
            mirror_margin_column_name(crate::grid::MARGIN_COLS - 1, true),
            "A"
        );
        assert_eq!(
            parse_cell_ref_at("[A1", 1).unwrap().0,
            CellAddr::Left {
                col: crate::grid::MARGIN_COLS - 1,
                row: 0
            }
        );
    }

    #[test]
    fn sheet_qualified_cell_refs_parse() {
        let (sheet_id, addr, len) = parse_sheet_qualified_cell_ref_at("$12:A5", 1).unwrap();
        assert_eq!(sheet_id, 12);
        assert_eq!(addr, CellAddr::Main { row: 4, col: 0 });
        assert_eq!(len, 6);
    }

    #[test]
    fn parses_corners_and_footers() {
        assert_eq!(
            parse_cell_ref_at("A_3", 4).unwrap().0,
            CellAddr::Footer {
                row: 2,
                col: crate::grid::ColumnAddr::Main(0)
            }
        );
        assert_eq!(
            parse_cell_ref_at("[A_3", 4).unwrap().0,
            CellAddr::Footer {
                row: 2,
                col: ColumnAddr::Left(701)
            }
        );
        assert_eq!(
parse_cell_ref_at("]A~3", 4).unwrap().0,
                    CellAddr::Header {
                        row: (HEADER_ROWS - 3) as u32,
                        col: crate::grid::ColumnAddr::from_global((crate::grid::MARGIN_COLS + 4) as usize, 4)
                    }
        );
    }

    #[test]
    fn parses_boundary_header_footer_rows() {
        assert_eq!(
parse_cell_ref_at("A~999999999", 1).unwrap().0,
                    CellAddr::Header {
                        row: 0,
                        col: crate::grid::ColumnAddr::Main(0)
                    }
        );
        assert_eq!(
parse_cell_ref_at("A_999999999", 1).unwrap().0,
                    CellAddr::Footer {
                        row: 999_999_998,
                        col: crate::grid::ColumnAddr::Main(0)
                    }
        );
        assert!(parse_cell_ref_at("A~1000000000", 1).is_none());
        assert!(parse_cell_ref_at("A_1000000000", 1).is_none());
    }

    #[test]
    fn ui_column_fragment_roundtrip() {
        let main_cols = 3usize;
        let cols = [
            crate::grid::MARGIN_COLS - 1,
            crate::grid::MARGIN_COLS,
            crate::grid::MARGIN_COLS + 1,
            crate::grid::MARGIN_COLS + main_cols,
        ];
        for col in cols {
            let frag = ui_column_fragment(col, main_cols);
            let (parsed, n) = parse_ui_column_fragment(&frag, main_cols).unwrap();
            assert_eq!(n, frag.len());
            assert_eq!(parsed as usize, col);
        }
    }

    #[test]
    fn ui_row_label_regions() {
        let main_rows = 2usize;
        assert_eq!(ui_row_label(0, main_rows), format!("~{}", crate::grid::HEADER_ROWS));
        assert_eq!(ui_row_label(crate::grid::HEADER_ROWS, main_rows), "1");
        assert_eq!(ui_row_label(crate::grid::HEADER_ROWS + main_rows, main_rows), "_1");
    }

    #[test]
    fn cursor_addr_roundtrip_across_regions() {
        let main_rows = 3usize;
        let main_cols = 4usize;
        let addrs = [
            CellAddr::Header {
                row: 0,
                col: crate::grid::ColumnAddr::Left(0),
            },
            CellAddr::Left {
                col: crate::grid::MARGIN_COLS - 1,
                row: 1,
            },
            CellAddr::Main { row: 2, col: 3 },
            CellAddr::Right { col: 0, row: 2 },
            CellAddr::Footer {
                row: 0,
                col: crate::grid::ColumnAddr::Right(0),
            },
        ];
        for addr in addrs {
            let (row, col) = addr_to_sheet_cursor(&addr, MainRows(main_rows), MainCols(main_cols));
            let back = sheet_cursor_to_addr(row, col, MainRows(main_rows), MainCols(main_cols));
            assert_eq!(back, addr);
        }
    }
}
