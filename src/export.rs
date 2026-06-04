//! TSV and CSV export for the main data region.

use crate::formula;
use crate::grid::{CellAddr, ColumnAddr, GridBox as Grid, FOOTER_ROWS, HEADER_ROWS, MARGIN_COLS};
use std::collections::HashSet;
use std::io::Write;
use zip::write::FileOptions;

/// Whether delimited/ASCII/selection export emits computed display text or stored cell text (`=…`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum ExportContent {
    /// Evaluated + formatted (matches the main grid / TSV golden files).
    #[default]
    Values,
    /// Raw storage: formula text where present, labels as stored.
    Formulas,
    /// Labeled ` -- ` column headers, rows use interop `=…` (comma-separated args; see plan).
    Generic,
}

/// Options for tab/comma (and "export all") delimited text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelimitedExportOptions {
    /// If true, emit a first line of column names (`A` / `[A` / `]B` with margins, etc.).
    pub include_header_row: bool,
    /// If true, include margin/header/footer *regions*; if false, main block only.
    pub include_margins: bool,
    /// If true, prefix each data line with the sheet row label and the delimiter, and when
    /// `include_header_row` the header has an empty first field for that row-address column (if the
    /// width style is margin-style, or a leading delimiter for main-block-only export). If false,
    /// data starts with the first data column. Independent of `include_margins` (main-only export
    /// can still show row `1`/`2` in the first field). Same idea as
    /// `AsciiTableOptions::include_row_label_column`.
    pub include_row_label_column: bool,
    /// Computed display vs stored `=…` text; see [ExportContent].
    pub content: ExportContent,
}

impl Default for DelimitedExportOptions {
#[inline(always)]
    fn default() -> Self {
        Self {
            include_header_row: true,
            include_margins: true,
            include_row_label_column: true,
            content: ExportContent::default(),
        }
    }
}

#[inline(always)]
pub fn export_tsv(grid: &Grid, out: &mut dyn Write) {
    export_tsv_with_options(grid, out, &DelimitedExportOptions::default());
}

#[inline(always)]
pub fn export_tsv_with_options(
    grid: &Grid,
    out: &mut dyn Write,
    options: &DelimitedExportOptions,
) {
    export_delimited(grid, out, '\t', options);
}

#[inline(always)]
pub fn export_csv(grid: &Grid, out: &mut dyn Write) {
    export_csv_with_options(grid, out, &DelimitedExportOptions::default());
}

#[inline(always)]
pub fn export_csv_with_options(
    grid: &Grid,
    out: &mut dyn Write,
    options: &DelimitedExportOptions,
) {
    export_delimited(grid, out, ',', options);
}

/// Pad/truncate to `width` by Unicode scalar values; right-align (pads on the left) with `pad`.
#[inline(always)]
fn ascii_field(s: &str, width: usize, pad: char) -> String {
    if width == 0 {
        return String::new();
    }
    let w = s.chars().count();
    if w > width {
        s.chars().rev().take(width).collect::<String>().chars().rev().collect()
    } else {
        let n = width - w;
        std::iter::repeat(pad)
            .take(n)
            .chain(s.chars())
            .collect()
    }
}

/// Space (U+0020) vs em space (U+2003) for padding between ASCII table pipes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum AsciiInterCellSpace {
    #[default]
    Space,
    EmSpace,
}

/// Rule for the line between the column-label row and the first data row.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum AsciiHeaderDataSeparator {
    /// A full `+---+` line under the column labels (default when a label row is present).
    #[default]
    FullBorder,
    /// No border between label row and data; data follows the label line immediately.
    None,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AsciiTableOptions {
    /// When false, export only the main data block: rows `hr..hr+main_rows`, cols A..Z (main) — no
    /// margin/header/footer cell columns or rows in the text table.
    pub include_margins: bool,
    /// Draw extra rules in the main block: a horizontal line of `=` above and below the main row
    /// range, and `+` at the left and right of the main block on each main data row to meet those
    /// lines (in addition to the normal outer `+---+` table border).
    pub data_frame: bool,
    /// First column: sheet row labels (`1`, `2`, `~1`, …). When false, the table starts with
    /// `| A | B |` (or margin columns) with no left gutter. Distinct from [`Self::include_column_label_row`].
    pub include_row_label_column: bool,
    pub include_column_label_row: bool,
    pub row_dividers: bool,
    pub inter_cell_space: AsciiInterCellSpace,
    pub header_data_separator: AsciiHeaderDataSeparator,
    /// Same meaning as [DelimitedExportOptions::content].
    pub content: ExportContent,
}

impl Default for AsciiTableOptions {
#[inline(always)]
    fn default() -> Self {
        Self {
            include_margins: true,
            data_frame: false,
            include_row_label_column: true,
            include_column_label_row: true,
            row_dividers: false,
            inter_cell_space: AsciiInterCellSpace::Space,
            header_data_separator: AsciiHeaderDataSeparator::FullBorder,
            content: ExportContent::default(),
        }
    }
}

#[inline(always)]
fn ascii_pre(opts: &AsciiTableOptions) -> char {
    match opts.inter_cell_space {
        AsciiInterCellSpace::Space => ' ',
        AsciiInterCellSpace::EmSpace => '\u{2003}',
    }
}

/// Append one cell: `pre` + right-aligned `text` in `w` (pad) + pre + `|`.
#[inline(always)]
fn ascii_push_cell(s: &mut String, pre: char, pad: char, text: &str, w: usize) {
    s.push(pre);
    s.push_str(&ascii_field(text, w, pad));
    s.push(pre);
    s.push('|');
}

/// `+---...---+` — optional row-label run (first block) is `-`. When `use_equals_in_main`, column
/// runs for `c in main_c0..main_c1` use `=`, which matches the `data_frame` inner horizontals.
#[inline(always)]
fn ascii_border_line(
    with_row_gutter: bool,
    col_start: usize,
    col_end: usize,
    row_label_w: usize,
    col_widths: &[usize],
    main_c0: usize,
    main_c1: usize,
    use_equals_in_main: bool,
) -> String {
    let border_dash_len = |w: usize| w.saturating_add(2);
    let mut s = String::new();
    s.push('+');
    if with_row_gutter {
        s.push_str(&"-".repeat(border_dash_len(row_label_w)));
        s.push('+');
    }
    for c in col_start..col_end {
        let w = col_widths[c];
        let dch = if use_equals_in_main && (main_c0..main_c1).contains(&c) {
            '='
        } else {
            '-'
        };
        s.push_str(
            &std::iter::repeat(dch)
                .take(border_dash_len(w))
                .collect::<String>(),
        );
        s.push('+');
    }
    s
}

#[inline(always)]
pub fn export_ascii_table_with_options(
    grid: &Grid,
    out: &mut dyn Write,
    options: &AsciiTableOptions,
) {
    let mc = grid.main_cols();
    let tc = grid.total_cols();
    let m = MARGIN_COLS;
    let hr = HEADER_ROWS;
    let mr = grid.main_rows();

    let (mut row_start, mut row_end) = ascii_row_bounds(grid);
    let (mut col_start, mut col_end) = ascii_col_bounds(grid);

    if !options.include_margins {
        col_start = m;
        col_end = (m + mc).min(grid.total_cols());
        col_end = col_end.max(col_start.saturating_add(1));
        row_start = hr;
        row_end = hr + mr;
    }

    let cell_content = options.content;

    let main_c0 = m.max(col_start);
    let main_c1 = (m + mc).min(col_end);
    let frame_active = options.data_frame && main_c0 < main_c1;

    let first_main_r = (row_start..row_end)
        .find(|&r| (hr..hr + mr).contains(&r));
    let last_main_r = (row_start..row_end)
        .rfind(|&r| (hr..hr + mr).contains(&r));

    let generic_rebase = if cell_content == ExportContent::Generic {
        let (dr, dc) = ascii_generic_rebase(
            col_start,
            col_end,
            row_start,
            row_end,
            first_main_r,
            last_main_r,
            frame_active,
            options,
        );
        Some((dr, dc))
    } else {
        None
    };

    let row_label_w = if options.include_row_label_column {
        (row_start..row_end)
            .map(|r| sheet_row_label(r, grid.main_rows()).chars().count())
            .max()
            .unwrap_or(0)
            .max(4)
    } else {
        0
    };
    let with_row_gutter = options.include_row_label_column;

    let mut col_widths: Vec<usize> = vec![0; tc];
    for c in col_start..col_end {
        let label = ascii_col_header_label(c, mc);
        col_widths[c] = label.chars().count().max(1);
    }

    for r in row_start..row_end {
        for c in col_start..col_end {
            let val = if cell_content == ExportContent::Values {
                rendered_value_at_ascii(grid, r, c)
            } else {
                export_cell_text(grid, r, c, cell_content, generic_rebase, true)
            };
            let content_w = val.chars().count();
            col_widths[c] = col_widths[c].max(content_w);
        }
    }

    // Each cell is rendered as `| {:>w$} |`, so the span between one `|` and the next is
    // always w + 2 characters (space + w-wide field + space before the closing `|`). Top/bottom
    // borders use that same width in `-` so `+` corners line up with `|`.
    let border: String = ascii_border_line(
        with_row_gutter,
        col_start,
        col_end,
        row_label_w,
        &col_widths,
        main_c0,
        main_c1,
        false,
    );
    let frame_line = if frame_active {
        Some(ascii_border_line(
            with_row_gutter,
            col_start,
            col_end,
            row_label_w,
            &col_widths,
            main_c0,
            main_c1,
            true,
        ))
    } else {
        None
    };

    let pre = ascii_pre(options);
    let pad = pre;

    let _ = writeln!(out, "{}", border);

    if options.include_column_label_row {
        let mut header_line = String::new();
        header_line.push('|');
        if with_row_gutter {
            ascii_push_cell(&mut header_line, pre, pad, "", row_label_w);
        }
        for c in col_start..col_end {
            let label = ascii_col_header_label(c, mc);
            let w = col_widths[c];
            ascii_push_cell(&mut header_line, pre, pad, &label, w);
        }
        let _ = writeln!(out, "{}", header_line);
        if matches!(options.header_data_separator, AsciiHeaderDataSeparator::FullBorder) {
            let _ = writeln!(out, "{}", border);
        }
    }

    for r in row_start..row_end {
        if frame_active && Some(r) == first_main_r {
            if let Some(ref fl) = frame_line {
                let _ = writeln!(out, "{}", fl);
            }
        }

        let in_main = (hr..hr + mr).contains(&r);
        let row_label = sheet_row_label(r, grid.main_rows());
        let mut data_line = String::new();
        data_line.push('|');
        if with_row_gutter {
            ascii_push_cell(&mut data_line, pre, pad, &row_label, row_label_w);
        }
        for c in col_start..col_end {
            if frame_active && in_main && c == main_c0 {
                if !options.include_row_label_column && col_start == main_c0 {
                    if data_line.starts_with('|') {
                        data_line.remove(0);
                        data_line.insert(0, '+');
                    }
                } else if data_line
                    .as_bytes()
                    .last()
                    .is_some_and(|&b| b == b'|')
                {
                    data_line.pop();
                    data_line.push('+');
                }
            }
            let val = if cell_content == ExportContent::Values {
                rendered_value_at_ascii(grid, r, c)
            } else {
                export_cell_text(grid, r, c, cell_content, generic_rebase, true)
            };
            let w = col_widths[c];
            ascii_push_cell(&mut data_line, pre, pad, &val, w);
            if frame_active
                && in_main
                && main_c0 < main_c1
                && c + 1 == main_c1
            {
                if data_line
                    .as_bytes()
                    .last()
                    .is_some_and(|&b| b == b'|')
                {
                    data_line.pop();
                    data_line.push('+');
                }
            }
        }

        let _ = writeln!(out, "{}", data_line);
        if options.row_dividers {
            let _ = writeln!(out, "{}", border);
        }
        if frame_active && Some(r) == last_main_r {
            if let Some(ref fl) = frame_line {
                let _ = writeln!(out, "{}", fl);
            }
        }
    }
    let _ = writeln!(out, "{}", border);
}

/// Renders a text table. For backward compatibility, [`export_ascii_table`] fixes `row_dividers`
/// only; use [`export_ascii_table_with_options`] for full control.
#[inline(always)]
pub fn export_ascii_table(grid: &Grid, out: &mut dyn Write, row_dividers: bool) {
    let mut o = AsciiTableOptions::default();
    o.row_dividers = row_dividers;
    export_ascii_table_with_options(grid, out, &o);
}

#[inline(always)]
pub fn export_all(grid: &Grid, out: &mut dyn Write) {
    export_all_with_options(grid, out, &DelimitedExportOptions::default());
}

#[inline(always)]
pub fn export_all_with_options(grid: &Grid, out: &mut dyn Write, options: &DelimitedExportOptions) {
    export_delimited(grid, out, '\t', options);
}

#[inline(always)]
pub fn export_odt_bytes(grid: &Grid) -> Result<Vec<u8>, std::io::Error> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(cursor);
    let opt = FileOptions::default().compression_method(zip::CompressionMethod::Stored);

    zip.start_file("mimetype", opt)?;
    zip.write_all(b"application/vnd.oasis.opendocument.text")?;

    zip.start_file("content.xml", FileOptions::default())?;
    let xml = odt_content_xml(grid);
    zip.write_all(xml.as_bytes())?;

    zip.start_file("META-INF/manifest.xml", FileOptions::default())?;
    zip.write_all(odt_manifest_xml().as_bytes())?;

    let cursor = zip.finish()?;
    Ok(cursor.into_inner())
}

#[inline(always)]
pub fn export_selection(
    grid: &Grid,
    out: &mut dyn Write,
    rows: &[usize],
    cols: &[usize],
    options: &DelimitedExportOptions,
) {
    if rows.is_empty() || cols.is_empty() {
        return;
    }

    let include_header_row = options.include_header_row;
    let content = options.content;
    let generic_rebase = if content == ExportContent::Generic {
        let (dr, dc) = selection_generic_rebase(cols, include_header_row, rows);
        Some((dr, dc))
    } else {
        None
    };

    if include_header_row {
        for (ci, &c) in cols.iter().enumerate() {
            if ci > 0 {
                let _ = write!(out, "\t");
            }
            let label = col_header_label_for_export(grid, c, grid.main_cols(), content);
            let _ = write!(out, "{}", label);
        }
        let _ = writeln!(out);
    }

    for &r in rows {
        for (ci, &c) in cols.iter().enumerate() {
            if ci > 0 {
                let _ = write!(out, "\t");
            }
            let val = export_cell_text(grid, r, c, content, generic_rebase, true);
            let _ = write!(out, "{}", val);
        }
        let _ = writeln!(out);
    }
}

/// TSV/CSV main-only / selection header token: `<`/`>` margins or `A`/`B` (column letters).
/// Generic mode shows `TAX` etc. on the bottom control header row, not in this synthetic label line.
#[inline(always)]
fn col_header_label_for_export(
    _grid: &Grid,
    global_col: usize,
    main_cols: usize,
    _content: ExportContent,
) -> String {
    let m = MARGIN_COLS;
    if global_col < m {
        format!("<{}", m - 1 - global_col)
    } else if global_col < m + main_cols {
        crate::addr::excel_column_name(global_col - m)
    } else {
        format!(">{}", global_col - m - main_cols)
    }
}

#[inline(always)]
fn ascii_col_header_label(global_col: usize, main_cols: usize) -> String {
    crate::addr::ui_column_fragment(global_col, main_cols)
}

/// With margins: [A / B / ]C (same in Generic; labeled-column titles appear on the `~1` control row).
#[inline(always)]
fn delimited_marginal_header_token(
    _grid: &Grid,
    global_col: usize,
    main_cols: usize,
    _content: ExportContent,
) -> String {
    crate::addr::ui_column_fragment(global_col, main_cols)
}

#[inline(always)]
fn col_header_label(global_col: usize, main_cols: usize) -> String {
    let m = MARGIN_COLS;
    if global_col < m {
        format!("<{}", m - 1 - global_col)
    } else if global_col < m + main_cols {
        crate::addr::excel_column_name(global_col - m)
    } else {
        format!(">{}", global_col - m - main_cols)
    }
}

/// ODF `;` → Excel `,` in function call lists (TSV generic column).
#[inline(always)]
pub fn interop_excel_list_separators(s: &str) -> String {
    s.replace(';', ",")
}

/// After rebase, the formula pretty-printer joins call arguments with `,`. ODF/Calc
/// expects `;` between arguments; replace top-level-argument commas (inside parens) without
/// touching commas inside string literals.
#[inline(always)]
fn interop_odf_function_commas_to_semicolons(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0i32;
    let mut in_string = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_string = !in_string;
                out.push(c);
            }
            '(' if !in_string => {
                depth += 1;
                out.push(c);
            }
            ')' if !in_string => {
                depth -= 1;
                out.push(c);
            }
            ',' if !in_string && depth > 0 => out.push(';'),
            _ => out.push(c),
        }
    }
    out
}

#[inline(always)]
fn finish_generic_interop(grid: &Grid, s: String, rebase: Option<(i32, i32)>) -> String {
    let Some((d_row, d_col)) = rebase else {
        return s;
    };
    if s.trim_start().starts_with('=') {
         formula::rebase_interop_formula_row_col(&s, d_row, d_col, grid.main_cols())
    } else {
        s
    }
}

/// Deltas for [`formula::rebase_interop_formula_row_col`]: the exported TSV/CSV/ASCII file’s
/// top-left cell (line 0, field 0) is treated as Excel A1, so all `=…` refs shift by these.
///
/// ODS re-import: apply the **inverse** shift (`-d_row, -d_col`) to `of:` bodies so they match
/// Corro’s grid A1 again (see [`crate::ods::set_ods_cell_tsv_parity`]).
#[inline(always)]
fn delimited_generic_rebase(
    col_start: usize,
    col_end: usize,
    include_header_row: bool,
    include_row_label_column: bool,
    main_rows: &[usize],
) -> (i32, i32) {
    // Row labels (~1, 1, 2, …) are a field *before* the col loop: count them in d_col.
    // Left margin `<…` / `[A` style cols are the first c in (col_start..MARGIN_COLS) in the loop;
    // `position(== MARGIN_COLS)` is exactly that many.
    let d_col = (if include_row_label_column { 1 } else { 0 })
        + (col_start..col_end)
            .position(|c| c == MARGIN_COLS)
            .map(|i| i as i32)
            .unwrap_or(0);
    let base = if include_header_row { 1 } else { 0 };
    let d_row = main_rows
        .iter()
        .position(|&r| r == HEADER_ROWS)
        .map(|j| base + j as i32)
        .unwrap_or(0);
    (d_row, d_col)
}

/// Re-export the same rebase the ODS/TSV `corro-ods-layout` block was built with, so the importer
/// can run [`crate::formula::rebase_interop_formula_row_col`] with negated `(d_row, d_col)`.
#[inline(always)]
pub fn delimited_layout_generic_rebase(
    col_start: usize,
    col_end: usize,
    include_header_row: bool,
    include_row_label_column: bool,
    data_logical_rows: &[usize],
) -> (i32, i32) {
    delimited_generic_rebase(
        col_start,
        col_end,
        include_header_row,
        include_row_label_column,
        data_logical_rows,
    )
}

#[inline(always)]
fn selection_generic_rebase(
    cols: &[usize],
    include_header_row: bool,
    rows: &[usize],
) -> (i32, i32) {
    let d_col = cols
        .iter()
        .position(|&c| c == MARGIN_COLS)
        .map(|i| i as i32)
        .unwrap_or(0);
    let base = if include_header_row { 1 } else { 0 };
    let d_row = rows
        .iter()
        .position(|&r| r == HEADER_ROWS)
        .map(|j| base + j as i32)
        .unwrap_or(0);
    (d_row, d_col)
}

/// Same row/col semantics as delimited, matching [`export_ascii_table_with_options`]'s `writeln!` order.
#[inline(always)]
fn ascii_generic_rebase(
    col_start: usize,
    col_end: usize,
    row_start: usize,
    row_end: usize,
    first_main_r: Option<usize>,
    last_main_r: Option<usize>,
    frame_active: bool,
    options: &AsciiTableOptions,
) -> (i32, i32) {
    let d_col = (if options.include_row_label_column { 1 } else { 0 })
        + (col_start..col_end)
            .position(|c| c == MARGIN_COLS)
            .map(|i| i as i32)
            .unwrap_or(0);
    if !(row_start..row_end).contains(&HEADER_ROWS) {
        return (0, d_col);
    }
    let hr = HEADER_ROWS;
    let mut d_row: i32 = 0;
    d_row = d_row.saturating_add(1);
    if options.include_column_label_row {
        d_row = d_row.saturating_add(1);
        if matches!(
            options.header_data_separator,
            AsciiHeaderDataSeparator::FullBorder
        ) {
            d_row = d_row.saturating_add(1);
        }
    }
    for r in row_start..hr {
        if frame_active && first_main_r == Some(r) {
            d_row = d_row.saturating_add(1);
        }
        d_row = d_row.saturating_add(1);
        if options.row_dividers {
            d_row = d_row.saturating_add(1);
        }
        if frame_active && last_main_r == Some(r) {
            d_row = d_row.saturating_add(1);
        }
    }
    if frame_active && first_main_r == Some(hr) {
        d_row = d_row.saturating_add(1);
    }
    (d_row, d_col)
}

/// Generic interop text for a cell. When `excel_list_arg_comma` is true, function-argument
/// `;` becomes `,` (TSV/CSV/Excel). When false, grid/ODF-style `;` is kept in the result string.
///
/// TSV/CSV use `excel_list_arg_comma: true`; ODS generic uses `false` (ODF `;` lists). Same
/// rebase as default delimited export (see `delimited_default_generic_rebase`).
///
/// The list-separator pass runs **after** [`finish_generic_interop`] (rebase), so
/// `formula::rebase_interop_formula_row_col` always sees the same `;` tokenization as the grid.
#[inline(always)]
pub fn generic_interop_cell_text(
    grid: &Grid,
    logical_row: usize,
    global_col: usize,
    rebase: Option<(i32, i32)>,
    excel_list_arg_comma: bool,
) -> Option<String> {
#[inline(always)]
    fn after_rebase(s: &str, excel: bool) -> String {
        if excel {
            interop_excel_list_separators(s)
        } else {
            interop_odf_function_commas_to_semicolons(s)
        }
    }

    use crate::grid::SheetCursor;
    let cur = SheetCursor {
        row: logical_row,
        col: global_col,
    };
    let addr = cur.to_addr(grid);
    // Bare aggregate *labels* (TOTAL, MAX, …), not stored `=…`, must stay that text in Generic.
    // Otherwise `=SUBTOTAL(4,…)` (etc.) is not the string `MAX` and does not “evaluate to” a label
    // — Values export the numeric result; Generic must not pretend the interop is the word on sheet.
    let raw_stored = grid.text(&addr);
    if !raw_stored.is_empty() && !formula::is_formula(&raw_stored) {
        if crate::ods::subtotal_code_for_label(&raw_stored).is_some() {
            return Some(finish_generic_interop(grid, raw_stored, rebase));
        }
    }

    // Margin / footer aggregate interop (`=SUM(…)`, etc.) must win over
    // [`formula::export_templated_formula`], which also reads the left-margin row key. A row
    // labeled `=TOTAL` would otherwise synthesize `=TOTAL` for every main cell from the row
    // template before we can emit `=SUM(…)` for the amount columns.
    if let Some(agg) = left_margin_row_aggregate_formula(grid, logical_row, global_col) {
        let s1 = finish_generic_interop(grid, agg, rebase);
        return Some(after_rebase(&s1, excel_list_arg_comma));
    }

    if let Some(agg) = right_margin_row_aggregate_formula(grid, logical_row, global_col) {
        let s1 = finish_generic_interop(grid, agg, rebase);
        return Some(after_rebase(&s1, excel_list_arg_comma));
    }

    if let Some(agg) = footer_column_aggregate_formula(grid, logical_row, global_col) {
        let s1 = finish_generic_interop(grid, agg, rebase);
        return Some(after_rebase(&s1, excel_list_arg_comma));
    }

    if let Some(tf) = formula::export_templated_formula(grid, &addr) {
        let s0 = crate::ods::ods_labeled_prefix_strip_to_formula(&tf).unwrap_or(tf);
        let s1 = finish_generic_interop(grid, s0, rebase);
        return Some(after_rebase(&s1, excel_list_arg_comma));
    }

    let v = crate::ods::cell_export_value_string(grid, logical_row, global_col);
    if !v.is_empty() {
        if let Some((_expr, label)) = formula::split_labeled_formula(&v) {
            // Bottom control-strip row (`~1`): Generic shows column labels only (`TAX`), not `=… -- TAX`,
            // matching DESIGN "labeled column headers" (see `generic_tsv_uses_tsv_header_and_interop_formula`).
            if logical_row == HEADER_ROWS - 1 {
                return Some(after_rebase(label, excel_list_arg_comma));
            }
            let s1 = finish_generic_interop(grid, v, rebase);
            return Some(after_rebase(&s1, excel_list_arg_comma));
        }
        if let Some(st) = crate::ods::ods_labeled_prefix_strip_to_formula(&v) {
            let s1 = finish_generic_interop(grid, st, rebase);
            return Some(after_rebase(&s1, excel_list_arg_comma));
        }
    }
    if let Some(raw) = grid.get(&addr) {
        if formula::is_formula(&raw) {
            if let Some(st) = crate::ods::ods_labeled_prefix_strip_to_formula(&raw) {
                let s1 = finish_generic_interop(grid, st, rebase);
                return Some(after_rebase(&s1, excel_list_arg_comma));
            }
        }
    }
    None
}

#[inline(always)]
fn aggregate_formula_name(raw: &str) -> Option<&'static str> {
    use crate::ops::AggFunc;
    match crate::ops::margin_key_agg_func(raw)? {
        AggFunc::Sum => Some("SUM"),
        AggFunc::Mean => Some("AVERAGE"),
        AggFunc::Count => Some("COUNT"),
        AggFunc::Max => Some("MAX"),
        AggFunc::Min => Some("MIN"),
        AggFunc::Median => None,
    }
}

#[inline(always)]
fn right_margin_aggregate_formula_name(grid: &Grid, global_col: usize) -> Option<&'static str> {
    let main_cols = grid.main_cols();
    let mut labels: Vec<(u32, String)> = grid
        .iter_nonempty()
        .filter_map(|(addr, val)| match addr {
            CellAddr::Header { row, col } if col.to_global(main_cols) == global_col => Some((row, val)),
            _ => None,
        })
        .collect();
    labels.sort_unstable_by_key(|(row, _)| *row);
    labels
        .into_iter()
        .find_map(|(_, val)| aggregate_formula_name(&val))
}

#[inline(always)]
fn main_row_aggregate_formula_name(grid: &Grid, main_row: usize) -> Option<&'static str> {
    let label = grid.text(&CellAddr::Left {
        col: MARGIN_COLS - 1,
        row: main_row as u32,
    });
    aggregate_formula_name(&label)
}

#[inline(always)]
fn raw_main_row_runs_before_aggregate(grid: &Grid, current_main_row: usize) -> Vec<(usize, usize)> {
    let mut start = 0usize;
    for candidate in (0..current_main_row).rev() {
        if main_row_aggregate_formula_name(grid, candidate).is_some() {
            start = candidate + 1;
            break;
        }
    }
    if start < current_main_row {
        vec![(start, current_main_row)]
    } else {
        Vec::new()
    }
}

#[inline(always)]
fn non_aggregate_main_row_runs(grid: &Grid) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for row in 0..grid.main_rows() {
        if main_row_aggregate_formula_name(grid, row).is_some() {
            if let Some(s) = start.take() {
                if s < row {
                    runs.push((s, row));
                }
            }
        } else if start.is_none() {
            start = Some(row);
        }
    }
    if let Some(s) = start {
        if s < grid.main_rows() {
            runs.push((s, grid.main_rows()));
        }
    }
    runs
}

#[inline(always)]
fn aggregate_formula_over_runs(
    func: &str,
    col: &str,
    runs: &[(usize, usize)],
) -> Option<String> {
    let ranges: Vec<String> = runs
        .iter()
        .filter(|(start, end)| start < end)
        .map(|(start, end)| format!("{col}{}:{col}{}", start + 1, end))
        .collect();
    if ranges.is_empty() {
        return None;
    }
    if func == "SUM" && ranges.len() > 1 {
        Some(
            ranges
                .iter()
                .map(|range| format!("SUM({range})"))
                .collect::<Vec<_>>()
                .join("+"),
        )
    } else {
        // ODF/LibreOffice use `;` between function args; TSV generic converts to `,` for Excel
        // via [interop_excel_list_separators] in [generic_interop_cell_text] when
        // `excel_list_arg_comma` is true. Commas here caused Err:508 in Calc (treated as wrong syntax).
        Some(format!("{func}({})", ranges.join(";")))
    }
}

#[inline(always)]
fn left_margin_row_aggregate_formula(
    grid: &Grid,
    logical_row: usize,
    global_col: usize,
) -> Option<String> {
    let main_cols = grid.main_cols();
    if logical_row < HEADER_ROWS || logical_row >= HEADER_ROWS + grid.main_rows() {
        return None;
    }
    // Same rule as [`main_formula_or_value`] for [`CellAddr::Left`]: the rightmost left-margin column
    // (global `MARGIN_COLS - 1`) holds the row key / `=TOTAL` margin key. Generic export must emit
    // `=SUBTOTAL` / `=SUM(…)` there, not the raw `=TOTAL` string (see `generic_tsv_target_footer_total_recomputes_after_parallel_data_edit`).
    let main_col_idx = if global_col == MARGIN_COLS - 1 {
        0usize
    } else if global_col >= MARGIN_COLS && global_col < MARGIN_COLS + main_cols {
        global_col - MARGIN_COLS
    } else {
        return None;
    };
    let main_row = logical_row - HEADER_ROWS;
    let func = main_row_aggregate_formula_name(grid, main_row)?;
    let col = crate::addr::excel_column_name(main_col_idx);
    let body = aggregate_formula_over_runs(func, &col, &raw_main_row_runs_before_aggregate(grid, main_row))?;
    Some(format!("={body}"))
}

#[inline(always)]
fn right_margin_row_aggregate_formula(
    grid: &Grid,
    logical_row: usize,
    global_col: usize,
) -> Option<String> {
    let main_cols = grid.main_cols();
    if logical_row < HEADER_ROWS || logical_row >= HEADER_ROWS + grid.main_rows() {
        return None;
    }
    if global_col < MARGIN_COLS + main_cols {
        return None;
    }
    let func = right_margin_aggregate_formula_name(grid, global_col)?;
    let row = logical_row - HEADER_ROWS + 1;
    let last_col = crate::addr::excel_column_name(main_cols.saturating_sub(1));
    Some(format!("={func}(A{row}:{last_col}{row})"))
}

#[inline(always)]
fn footer_column_aggregate_formula(
    grid: &Grid,
    logical_row: usize,
    global_col: usize,
) -> Option<String> {
    let main_rows = grid.main_rows();
    let main_cols = grid.main_cols();
    if logical_row < HEADER_ROWS + main_rows {
        return None;
    }
    if global_col < MARGIN_COLS {
        return None;
    }
    let footer_row = logical_row - HEADER_ROWS - main_rows;
    let key = grid.text(&CellAddr::Footer {
        row: footer_row as u32,
        col: ColumnAddr::from_global(MARGIN_COLS - 1, main_cols),
    });
    let func = aggregate_formula_name(&key)?;
    let exported_col = global_col - MARGIN_COLS;
    let col = crate::addr::excel_column_name(exported_col);
    let body = aggregate_formula_over_runs(func, &col, &non_aggregate_main_row_runs(grid))?;
    Some(format!("={body}"))
}

#[inline(always)]
fn sheet_row_label(logical_row: usize, main_rows: usize) -> String {
    let hr = HEADER_ROWS;
    if logical_row < hr {
        format!("~{}", HEADER_ROWS - logical_row)
    } else if logical_row < hr + main_rows {
        format!("{}", logical_row - hr + 1)
    } else {
        let fr = logical_row - hr - main_rows;
        format!("_{}", fr + 1)
    }
}

#[inline(always)]
fn cell_value_at(grid: &Grid, logical_row: usize, global_col: usize) -> String {
    let hr = HEADER_ROWS;
    let mr = grid.main_rows();
    let lm = MARGIN_COLS;
    let mc = grid.main_cols();
    let _fr = FOOTER_ROWS;

    if logical_row < hr {
        let r = logical_row as u32;
        grid.text(&CellAddr::Header {
            row: r,
            col: ColumnAddr::from_global(global_col, mc),
        })
    } else if logical_row < hr + mr {
        let mri = logical_row - hr;
        if global_col < lm {
            // Match `SheetCursor::to_addr`: Left `col` is the global margin column (0..lm).
            grid.text(&CellAddr::Left {
                col: global_col,
                row: mri as u32,
            })
        } else if global_col < lm + mc {
            let mc_idx = global_col - lm;
            grid.text(&CellAddr::Main {
                row: mri as u32,
                col: mc_idx as u32,
            })
        } else {
            let rc = global_col - lm - mc; // margin index (usize)
            grid.text(&CellAddr::Right {
                col: rc,
                row: mri as u32,
            })
        }
    } else {
        let fr_idx = logical_row - hr - mr;
        let r = fr_idx as u32;
        grid.text(&CellAddr::Footer {
            row: r,
            col: ColumnAddr::from_global(global_col, mc),
        })
    }
}

#[inline(always)]
fn rendered_value_at(grid: &Grid, logical_row: usize, global_col: usize) -> String {
    use crate::grid::SheetCursor;
    let cur = SheetCursor {
        row: logical_row,
        col: global_col,
    };
    let addr = cur.to_addr(grid);
    let text = crate::ui::tsv_effective_unformatted_string(grid, logical_row, global_col);
    crate::ui::format_cell_display(grid, &addr, text)
}

#[inline(always)]
fn rendered_value_at_ascii(grid: &Grid, logical_row: usize, global_col: usize) -> String {
    let hr = HEADER_ROWS;
    let mr = grid.main_rows();
    let mc = grid.main_cols();
    let lm = MARGIN_COLS;

    if logical_row >= hr && logical_row < hr + mr && global_col < lm {
        let main_row = logical_row - hr;
        let addr = CellAddr::Left {
            col: global_col,
            row: main_row as u32,
        };
        let raw = grid.text(&addr);
        if crate::ods::subtotal_code_for_label(&raw).is_some() {
            return crate::formula::cell_effective_display(grid, &addr);
        }
    }
    if logical_row >= hr + mr && global_col == lm - 1 {
        let footer_row = logical_row - hr - mr;
        let addr = CellAddr::Footer {
            row: footer_row as u32,
            col: ColumnAddr::from_global(lm - 1, mc),
        };
        let raw = grid.text(&addr);
        if crate::ods::subtotal_code_for_label(&raw).is_some() {
            return crate::formula::cell_effective_display(grid, &addr);
        }
    }
    rendered_value_at(grid, logical_row, global_col)
}

/// What one cell would be in TSV/CSV/ASCII for the given [ExportContent] and (for Generic) the
/// same `generic_rebase` that [`export_delimited`] / [`export_tsv_with_options`] use.
///
/// For [`ExportContent::Generic`] only, `generic_excel_list_arg_comma` is passed to
/// [`generic_interop_cell_text`]: `true` = Excel/TSV (`,` between function args), `false` = ODF /
/// LibreOffice in `of:` (`;` between args). Ignored for other modes (pass `true`).
#[inline(always)]
pub fn export_cell_text(
    grid: &Grid,
    logical_row: usize,
    global_col: usize,
    content: ExportContent,
    generic_rebase: Option<(i32, i32)>,
    generic_excel_list_arg_comma: bool,
) -> String {
    match content {
        ExportContent::Values => rendered_value_at(grid, logical_row, global_col),
        ExportContent::Formulas => cell_value_at(grid, logical_row, global_col),
        ExportContent::Generic => {
            generic_interop_cell_text(
                grid,
                logical_row,
                global_col,
                generic_rebase,
                generic_excel_list_arg_comma,
            )
            .unwrap_or_else(|| rendered_value_at(grid, logical_row, global_col))
        }
    }
}

#[inline(always)]
fn needs_csv_quoting(s: &str, delim: char) -> bool {
    s.contains(delim) || s.contains('"') || s.contains('\n') || s.contains('\r')
}

#[inline(always)]
fn csv_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Column span and row order for TSV/CSV; must match [`export_delimited`].
#[inline(always)]
fn delimited_table_col_span_and_rows(
    grid: &Grid,
    options: &DelimitedExportOptions,
) -> (usize, usize, Vec<usize>) {
    let include_margins = options.include_margins;
    let mr = grid.main_rows();
    let mc = grid.main_cols();
    let hr = HEADER_ROWS;
    let lm = MARGIN_COLS;
    let fr = FOOTER_ROWS;
    let total_rows = hr + mr + fr;

    let (col_start, mut col_end) = if include_margins {
        // Preserve ascii_col_bounds when including margins: allow left-margin,
        // main block, and any right-margin columns that contain content. The
        // ascii_col_bounds helper already trims leading/trailing empty margin
        // columns; do not override its result here.
        ascii_col_bounds(grid)
    } else {
        (lm, lm + mc)
    };
    let main_spans = main_row_index_bounds_for_export(grid);

    // For delimited export we prefer rows that matter to the main data block and
    // file consumers: header rows, main rows that have main/right content, and
    // explicit footer rows. Rows that only contain left-margin labels (annotations)
    // are UI-only and historically not included in TSV/CSV output; filter them
    // out here while still preserving contiguous main ranges when `main_spans`
    // forces them to exist.
    let mut rows: Vec<usize> = Vec::new();
    for r in row_order(grid, total_rows) {
        let mut include = false;
        if r < hr {
            // header band: include if any header cell exists for this logical row
            for (addr, _) in grid.iter_nonempty() {
                match addr {
                    CellAddr::Header { row, .. } if row as usize == r => {
                        include = true;
                        break;
                    }
                    _ => {}
                }
            }
        } else if r < hr + mr {
            // main band: include if any Main or Right cell exists for this main row
            let main_row = (r - hr) as u32;
            for (addr, _) in grid.iter_nonempty() {
                match addr {
                    CellAddr::Main { row, .. } if row == main_row => {
                        include = true;
                        break;
                    }
                    CellAddr::Right { row, .. } if row == main_row => {
                        include = true;
                        break;
                    }
                    _ => {}
                }
            }
            // If not present but the main spans force inclusion, include it.
            if !include {
                if let Some((mmin, mmax)) = main_spans {
                    let mr_idx = r - hr;
                    if mr_idx >= mmin && mr_idx <= mmax {
                        include = true;
                    }
                }
            }
            // Include left-margin *aggregate* rows even when they have no Main/Right
            // cells: a left-margin cell like `=TOTAL` marks a subtotal row that must
            // appear in delimited Generic exports. Detect that via the ODS helper
            // that recognizes margin aggregate labels.
            if !include {
                let left_raw = grid.text(&CellAddr::Left {
                    col: MARGIN_COLS - 1,
                    row: main_row,
                });
                if crate::ods::subtotal_code_for_label(&left_raw).is_some() {
                    include = true;
                }
            }
        } else {
            // footer band: include if any Footer cell exists for this footer row
            let fr_idx = (r - hr - mr) as u32;
            for (addr, _) in grid.iter_nonempty() {
                match addr {
                    CellAddr::Footer { row, .. } if row == fr_idx => {
                        include = true;
                        break;
                    }
                    _ => {}
                }
            }
        }
        if include {
            rows.push(r);
        }
    }
    (col_start, col_end, rows)
}

/// Rebase (Δrow, Δcol) for interop `=…` for the given delimited layout ([`delimited_export_matrix`]
/// uses the same span and row list as this).
#[inline(always)]
pub fn delimited_options_generic_rebase(
    grid: &Grid,
    options: &DelimitedExportOptions,
) -> (i32, i32) {
    let (c0, c1, ref rows) = delimited_table_col_span_and_rows(grid, options);
    delimited_generic_rebase(
        c0,
        c1,
        options.include_header_row,
        options.include_row_label_column,
        rows,
    )
}

/// Rebase (Δrow, Δcol) for interop `=…` relative to a default TSV/CSV table (A1 = file top-left
/// with header row, margins, and row key column; same layout as `DelimitedExportOptions::default`).
#[inline(always)]
pub fn delimited_default_generic_rebase(grid: &Grid) -> (i32, i32) {
    delimited_options_generic_rebase(grid, &DelimitedExportOptions::default())
}

#[inline(always)]
fn export_delimited(
    grid: &Grid,
    out: &mut dyn Write,
    delim: char,
    options: &DelimitedExportOptions,
) {
    let include_headers = options.include_header_row;
    let include_margins = options.include_margins;
    let row_key_col = options.include_row_label_column;
    let content = options.content;
    let mr = grid.main_rows();
    let mc = grid.main_cols();
    let _rm = MARGIN_COLS;

    // Trim leading/trailing all-empty margin columns (same span as `export_ascii_table`),
    // but always include the full main block: the last main column can hold fill/spill
    // output without a `main_cells` key, so `logical_col_has_content` may be false.
    let (col_start, col_end, rows) = delimited_table_col_span_and_rows(grid, options);

    if include_headers {
        if include_margins {
            if row_key_col {
                // Match UI: leading row-label column; header cell is blank. First field is empty,
                // so the line starts with the delimiter (tab for TSV, comma for CSV).
                let _ = write!(
                    out,
                    "{}{}",
                    delim,
                    delimited_marginal_header_token(grid, col_start, mc, content)
                );
                for c in (col_start + 1)..col_end {
                    let _ = write!(
                        out,
                        "{}{}",
                        delim,
                        delimited_marginal_header_token(grid, c, mc, content)
                    );
                }
            } else {
                let _ = write!(
                    out,
                    "{}",
                    delimited_marginal_header_token(grid, col_start, mc, content)
                );
                for c in (col_start + 1)..col_end {
                    let _ = write!(
                        out,
                        "{}{}",
                        delim,
                        delimited_marginal_header_token(grid, c, mc, content)
                    );
                }
            }
        } else {
            if row_key_col {
                let _ = write!(out, "{delim}");
            }
            for c in col_start..col_end {
                if c > col_start {
                    let _ = write!(out, "{delim}");
                }
                let label = col_header_label_for_export(grid, c, mc, content);
                let _ = write!(out, "{}", label);
            }
        }
        let _ = writeln!(out);
    }

    let generic_rebase = if content == ExportContent::Generic {
        let (dr, dc) = delimited_generic_rebase(
            col_start,
            col_end,
            include_headers,
            row_key_col,
            &rows,
        );
        Some((dr, dc))
    } else {
        None
    };
    for r in rows {
        if row_key_col {
            let _ = write!(out, "{}", sheet_row_label(r, mr));
            let _ = write!(out, "{delim}");
        }
        let mut first = true;
        for c in col_start..col_end {
            if !first {
                let _ = write!(out, "{delim}");
            }
            first = false;
            let val = export_cell_text(grid, r, c, content, generic_rebase, true);
            if delim == ',' && needs_csv_quoting(&val, delim) {
                let _ = write!(out, "{}", csv_quote(&val));
            } else {
                let _ = write!(out, "{}", val);
            }
        }
        let _ = writeln!(out);
    }
}

/// Matrix of the same text [`export_tsv_with_options`] / [`export_delimited`] would write (one
/// row per `Vec`, fields tab-separated in the original output). For ODS, this matches LibreOffice
/// A1/row 1+ layout when a header row and row-key column are used.
#[inline(always)]
pub fn delimited_export_matrix(
    grid: &Grid,
    options: &DelimitedExportOptions,
) -> (Vec<Vec<String>>, usize, usize, Vec<usize>) {
    let include_headers = options.include_header_row;
    let include_margins = options.include_margins;
    let row_key_col = options.include_row_label_column;
    let content = options.content;
    let mr = grid.main_rows();
    let mc = grid.main_cols();
    let (col_start, col_end, rows) = delimited_table_col_span_and_rows(grid, options);
    let generic_rebase = if content == ExportContent::Generic {
        let (dr, dc) = delimited_generic_rebase(
            col_start,
            col_end,
            include_headers,
            row_key_col,
            &rows,
        );
        Some((dr, dc))
    } else {
        None
    };
    let mut out: Vec<Vec<String>> = Vec::new();
    if include_headers {
        let mut line = Vec::new();
        if include_margins {
            if row_key_col {
                line.push(String::new());
            }
            for c in col_start..col_end {
                line.push(delimited_marginal_header_token(grid, c, mc, content));
            }
        } else {
            if row_key_col {
                line.push(String::new());
            }
            for c in col_start..col_end {
                line.push(col_header_label_for_export(grid, c, mc, content));
            }
        }
        out.push(line);
    }
    for r in &rows {
        let mut line = Vec::new();
        if row_key_col {
            line.push(sheet_row_label(*r, mr));
        }
        for c in col_start..col_end {
            line.push(export_cell_text(
                grid,
                *r,
                c,
                content,
                generic_rebase,
                true,
            ));
        }
        out.push(line);
    }
    (out, col_start, col_end, rows)
}

#[inline(always)]
fn odt_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[inline(always)]
fn odt_content_xml(grid: &Grid) -> String {
    let mr = grid.main_rows();
    let tc = grid.total_cols();
    let total_rows = HEADER_ROWS + mr + FOOTER_ROWS;

    let mut s = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" office:version="1.2"><office:body><office:text><table:table>"#,
    );

    for c in 0..tc {
        s.push_str(&format!(
            r#"<table:table-column table:number-columns-repeated="1" table:style-name="co{}"/>"#,
            c
        ));
    }

    for r in row_order(grid, total_rows) {
        s.push_str("<table:table-row>");
        for c in 0..tc {
            let val = odt_escape(&cell_value_at(grid, r, c));
            let text = if val.is_empty() {
                String::new()
            } else {
                format!(r#"<text:p>{}</text:p>"#, val)
            };
            s.push_str(&format!(
                r#"<table:table-cell office:value-type="string">{}</table:table-cell>"#,
                text
            ));
        }
        s.push_str("</table:table-row>");
    }

    s.push_str("</table:table></office:text></office:body></office:document-content>");
    s
}

#[inline(always)]
fn ascii_row_bounds(grid: &Grid) -> (usize, usize) {
    let hr = HEADER_ROWS;
    let mr = grid.main_rows();
    let fr = FOOTER_ROWS;
    let rows = row_order(grid, hr + mr + fr);
    match (rows.first().copied(), rows.last().copied()) {
        (Some(start), Some(end)) => (start, end + 1),
        _ => (hr, hr + 1),
    }
}

#[inline(always)]
fn ascii_col_bounds(grid: &Grid) -> (usize, usize) {
    let tc = grid.total_cols();
    let mut start = 0;
    while start < tc && !grid.logical_col_has_content(start) {
        start += 1;
    }
    let mut end = tc;
    while end > start && !grid.logical_col_has_content(end - 1) {
        end -= 1;
    }
    (start, end.max(start + 1))
}

#[inline(always)]
fn odt_manifest_xml() -> String {
    String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.2">
<manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.text"/>
<manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>
</manifest:manifest>"#,
    )
}

/// Min/max main **row** indices (0-based) with any main/margin content.
#[inline(always)]
fn main_row_index_bounds_for_export(grid: &Grid) -> Option<(usize, usize)> {
    let mut set = HashSet::new();
    for (addr, _) in grid.iter_nonempty() {
        match addr {
            // Only consider main-block and right-margin cells when deciding the
            // contiguous main-row span for exports. Left-margin-only entries are
            // labels/annotations and should not by themselves extend the exported
            // main-row range (they remain visible in exported rows when paired
            // with main/right content via the filter below).
            CellAddr::Main { row, .. } | CellAddr::Right { row, .. } => {
                set.insert(row as usize);
            }
            _ => {}
        }
    }
    if set.is_empty() {
        None
    } else {
        Some((*set.iter().min().unwrap(), *set.iter().max().unwrap()))
    }
}

#[inline(always)]
fn row_order(grid: &Grid, _total_rows: usize) -> Vec<usize> {
    let hr = HEADER_ROWS;
    let mr = grid.main_rows();
    let mut header_rows = Vec::new();
    let mut main_rows = HashSet::new();
    let mut footer_rows = Vec::new();

    for (addr, _) in grid.iter_nonempty() {
        match addr {
            CellAddr::Header { row, .. } => header_rows.push(row as usize),
            CellAddr::Footer { row, .. } => footer_rows.push(hr + mr + row as usize),
            CellAddr::Main { row, .. }
            | CellAddr::Left { row, .. }
            | CellAddr::Right { row, .. } => {
                main_rows.insert(row as usize);
            }
        }
    }

    header_rows.sort_unstable();
    header_rows.dedup();
    footer_rows.sort_unstable();
    footer_rows.dedup();

    let mut rows = header_rows;
    // Contiguous main row indices: include "gap" main rows (no cells yet) so export matches
    // a sheet that shows row numbers through empty interstitial rows.
    if !main_rows.is_empty() {
        let mmin = *main_rows.iter().min().unwrap();
        let mmax = *main_rows.iter().max().unwrap();
        rows.extend((mmin..=mmax).map(|r| hr + r));
    }
    rows.extend(footer_rows);
    rows
}

#[inline(always)]
pub fn export_sorted_tsv(grid: &Grid, out: &mut dyn Write, sort_cols: &[usize]) {
    let mr = grid.main_rows();
    let mc = grid.main_cols();
    if sort_cols.is_empty() {
        export_tsv(grid, out);
        return;
    }

    let mut rows: Vec<usize> = (0..mr).collect();
    rows.sort_by(|&a, &b| {
        for &c in sort_cols {
            let va = grid.text(&CellAddr::Main {
                row: a as u32,
                col: c as u32,
            });
            let vb = grid.text(&CellAddr::Main {
                row: b as u32,
                col: c as u32,
            });
            let ord = va.cmp(&vb);
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        a.cmp(&b)
    });

    for (i, c) in (0..mc).enumerate() {
        if i > 0 {
            let _ = write!(out, "\t");
        }
        let _ = write!(out, "{}", col_header_label(MARGIN_COLS + c, mc));
    }
    let _ = writeln!(out);

    for r in rows {
        for c in 0..mc {
            if c > 0 {
                let _ = write!(out, "\t");
            }
            let val = grid
                .get(&CellAddr::Main {
                    row: r as u32,
                    col: c as u32,
                })
                .unwrap_or("".to_string());
            let _ = write!(out, "{}", val);
        }
        let _ = writeln!(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

#[inline(always)]
    fn load_fixture(path: &Path) -> crate::ops::WorkbookState {
        let data = std::fs::read_to_string(path).unwrap();
        let mut workbook = crate::ops::WorkbookState::new();
        let mut active_sheet = workbook.sheet_id(workbook.active_sheet);
        for (idx, line) in data.lines().enumerate() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            if t == "SET $1:_6]A :q" {
                continue;
            }
            crate::ops::apply_log_line_to_workbook(t, &mut workbook, &mut active_sheet)
                .unwrap_or_else(|e| panic!("{}:{}: {} => {e}", path.display(), idx + 1, t));
        }
        workbook
    }

    /// `split_top_level_args` must treat `;` like `,` (ODF / aggregate multi-range) or rebase is a
    /// no-op and refs stay in grid-space (`A1` instead of the exported sheet’s `C3`).
    #[test]
#[inline(always)]
    fn rebase_interop_shifts_max_with_semicolon_list_separator() {
        let out = crate::formula::rebase_interop_formula_row_col("=MAX(A1:A5;A7:A9)", 2, 2, 4);
        assert_eq!(out, "=MAX(C3:C7,C9:C11)");
    }

    /// After a successful rebase, the pretty-printer joins args with `,`; ODF still
    /// needs `;` between function arguments.
    #[test]
#[inline(always)]
    fn odf_interop_rewrites_commas_in_calls_to_semicolons() {
        assert_eq!(
            interop_odf_function_commas_to_semicolons("=MAX(C3:C7,C9:C11)"),
            "=MAX(C3:C7;C9:C11)"
        );
    }

#[inline(always)]
    fn parse_delimited(data: &str, delim: char) -> Vec<Vec<String>> {
        data.lines()
            .map(|line| parse_delimited_line(line, delim))
            .collect()
    }

#[inline(always)]
    fn parse_delimited_line(line: &str, delim: char) -> Vec<String> {
        let mut fields = Vec::new();
        let mut current = String::new();
        let mut in_quotes = false;
        let mut chars = line.chars().peekable();
        // Only treat double-quotes as quoting syntax for CSV (comma) output.
        // For other delimiters (e.g. TSV) quotes are literal characters and
        // should not change parsing behavior.
        let use_quotes = delim == ',';

        while let Some(ch) = chars.next() {
            if in_quotes {
                if ch == '"' {
                    if chars.peek() == Some(&'"') {
                        current.push('"');
                        chars.next();
                    } else {
                        in_quotes = false;
                    }
                } else {
                    current.push(ch);
                }
            } else if use_quotes && ch == '"' {
                in_quotes = true;
            } else if ch == delim {
                fields.push(current.clone());
                current.clear();
            } else {
                current.push(ch);
            }
        }
        fields.push(current);
        fields
    }

    /// Same field rules as [`export_delimited`], for comparing CSV vs TSV without evaluating
    /// volatile formulas twice (two full exports can shift `NOW()` between passes).
#[inline(always)]
    fn matrix_render_same_as_export(rows: &[Vec<String>], delim: char) -> String {
        let mut out = String::new();
        for row in rows {
            let mut first = true;
            for val in row {
                if !first {
                    out.push(delim);
                }
                first = false;
                if delim == ',' && needs_csv_quoting(val, delim) {
                    out.push_str(&csv_quote(val));
                } else {
                    out.push_str(val);
                }
            }
            out.push('\n');
        }
        out
    }

    #[test]
#[inline(always)]
    fn ascii_table_trims_empty_margin_columns() {
        let mut g = crate::grid::Grid::new(3, 1);
        g.set(&CellAddr::Main { row: 0, col: 0 }, "Aasdf".into());
        g.set(&CellAddr::Main { row: 2, col: 0 }, "adsf".into());
        let gb = crate::grid::GridBox::from(g);
        let mut out = Vec::new();
        export_ascii_table(&gb, &mut out, false);
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains("<9"));
        assert!(!s.contains(">9"));
        assert!(s.contains("Aasdf"));
        assert!(s.contains("adsf"));
    }

    #[test]
#[inline(always)]
    fn ascii_headers_use_ui_margin_notation() {
        let mut g = crate::grid::Grid::new(1, 1);
        g.set(&CellAddr::Left { row: 0, col: 0 }, "L".into());
        g.set(&CellAddr::Main { row: 0, col: 0 }, "1".into());
        let gb = crate::grid::GridBox::from(g);
        let mut out = Vec::new();
        export_ascii_table(&gb, &mut out, false);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("[A"), "expected UI-style left margin header, got {s}");
        assert!(!s.contains("<0"), "legacy <0 header should not appear, got {s}");
    }

    #[test]
#[inline(always)]
    fn ascii_values_shows_total_label_for_stored_eq_total() {
        let mut g = crate::grid::Grid::new(3, 1);
        g.set(&CellAddr::Main { row: 0, col: 0 }, "1".into());
        g.set(&CellAddr::Main { row: 1, col: 0 }, "2".into());
        g.set(&CellAddr::Left { row: 2, col: MARGIN_COLS - 1 }, "=TOTAL".into());
        let gb = crate::grid::GridBox::from(g);
        let mut out = Vec::new();
        export_ascii_table(&gb, &mut out, false);
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.contains("TOTAL"),
            "left key should display TOTAL (stored =TOTAL) in ASCII output: {s}"
        );
    }

    #[test]
#[inline(always)]
    fn colwidth_fixture_keeps_column_a_narrow_and_b_wide() {
        use std::path::Path;

        let data = std::fs::read_to_string(Path::new("docs/tests/colwidth.corro")).unwrap();
        let mut workbook = crate::ops::WorkbookState::new();
        let mut active_sheet = workbook.sheet_id(workbook.active_sheet);
        for line in data.lines() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            crate::ops::apply_log_line_to_workbook(t, &mut workbook, &mut active_sheet).unwrap();
        }
        let sheet = workbook.sheet_mut_by_id(active_sheet).unwrap();
        for c in 0..sheet.grid.main_cols() {
            sheet
                .grid
                .fit_column_to_content(crate::grid::MARGIN_COLS + c);
        }

        assert_eq!(sheet.grid.col_width(crate::grid::MARGIN_COLS), 4);
        assert_eq!(sheet.grid.col_width(crate::grid::MARGIN_COLS + 1), crate::grid::DEFAULT_MAX_COL_WIDTH);
    }

    #[test]
#[inline(always)]
    fn ascii_export_uses_rendered_widths() {
        use std::path::Path;

        let data = std::fs::read_to_string(Path::new("subtotal.corro")).unwrap();
        let mut workbook = crate::ops::WorkbookState::new();
        let mut active_sheet = workbook.sheet_id(workbook.active_sheet);
        for line in data.lines() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            crate::ops::apply_log_line_to_workbook(t, &mut workbook, &mut active_sheet).unwrap();
        }
        let sheet = workbook.sheet_mut_by_id(active_sheet).unwrap();
        for c in 0..sheet.grid.main_cols() {
            sheet.grid.set_col_width(crate::grid::MARGIN_COLS + c, None);
        }
        let mut out = Vec::new();
        // sheet.grid is already a GridBox
        export_ascii_table(&sheet.grid, &mut out, false);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("TOTAL"));
        assert!(s.lines().next().unwrap().starts_with("+"));
        assert!(s.lines().nth(1).unwrap().starts_with("|"));
        assert!(s.lines().last().unwrap().starts_with("+"));
        assert!(!s.contains("…"));
    }

    /// Default TSV adds a synthetic header row and row-key column before the logical grid. For
    /// `subtotal.corro`, Values match computed display. Generic keeps bare non-formula aggregate
    /// **words** (e.g. `MAX`, `SUM`, …) as on-sheet text, not `=SUBTOTAL(4,…)`; the `=TOTAL` margin
    /// directive uses interop `=SUBTOTAL(9,…)` while the TUI still shows `TOTAL`. True `=SUBTOTAL(4,…)`
    /// interop is a formula whose **Values** are numeric, never the label word `MAX`.
    #[test]
#[inline(always)]
    fn subtotal_delimited_values_match_computed_display_and_generic_matches_non_formula() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("subtotal.corro");
        let workbook = load_fixture(&path);
        let grid = &workbook.active_sheet().grid;

        let opts_v = DelimitedExportOptions {
            content: ExportContent::Values,
            ..Default::default()
        };
        let opts_g = DelimitedExportOptions {
            content: ExportContent::Generic,
            ..Default::default()
        };
        let (m_v, c0, c1, rows) = delimited_export_matrix(grid, &opts_v);
        let (m_g, c0g, c1g, rows_g) = delimited_export_matrix(grid, &opts_g);
        assert_eq!(c0, c0g, "col span start");
        assert_eq!(c1, c1g, "col span end");
        assert_eq!(rows, rows_g, "logical row order");
        assert_eq!(m_v.len(), m_g.len(), "matrix row count");

        let h = if opts_v.include_header_row { 1 } else { 0 };
        let rk = if opts_v.include_row_label_column { 1 } else { 0 };

#[inline(always)]
        fn is_subtotal_following_label_replaced_by_interop(s: &str) -> bool {
            matches!(
                s.trim().to_ascii_uppercase().as_str(),
                "TOTAL" | "SUM" | "MEAN" | "AVERAGE" | "AVG" | "COUNT" | "MAX" | "MAXIMUM" | "MIN"
                    | "MINIMUM"
            )
        }

        for (i, (row_v, row_g)) in m_v.iter().zip(m_g.iter()).enumerate() {
            assert_eq!(
                row_v.len(),
                row_g.len(),
                "row {i} column count mismatch"
            );
            for (j, (v_cell, g_cell)) in row_v.iter().zip(row_g.iter()).enumerate() {
                if i < h {
                    assert_eq!(
                        v_cell, g_cell,
                        "header line {i} field {j} should match Values vs Generic"
                    );
                    continue;
                }
                if j < rk {
                    assert_eq!(
                        v_cell, g_cell,
                        "row-label column i={i} j={j} should match"
                    );
                    continue;
                }
                let data_i = i - h;
                let lr = rows[data_i];
                let gc = c0 + (j - rk);
                let computed =
                    export_cell_text(grid, lr, gc, ExportContent::Values, None, true);
                assert_eq!(
                    v_cell, &computed,
                    "Values export at matrix[{i}][{j}] (logical row {lr}, col {gc})"
                );
                use crate::grid::SheetCursor;
                let cur = SheetCursor { row: lr, col: gc };
                let raw_stored = grid.text(&cur.to_addr(grid));
                let is_bare_agg_label = !raw_stored.is_empty()
                    && !formula::is_formula(&raw_stored)
                    && crate::ods::subtotal_code_for_label(&raw_stored).is_some();
                let g_norm = g_cell.trim();
                if is_bare_agg_label {
                    assert!(
                        !g_norm.contains("SUBTOTAL"),
                        "regression: wrong Generic =SUBTOTAL(…) vs on-sheet label {raw_stored:?} \
                         (e.g. =SUBTOTAL(4,A1:B10) is not the string MAX) at matrix[{i}][{j}] \
                         (lr={lr} gc={gc}), got {g_cell:?}"
                    );
                    assert_eq!(g_norm, raw_stored.trim(), "Generic keeps bare aggregate at [{i}][{j}]");
                    continue;
                }
                let is_subtotal_interop = g_norm
                    .strip_prefix('=')
                    .is_some_and(|r| {
                        r.trim_start()
                            .to_ascii_lowercase()
                            .starts_with("subtotal(")
                    });
                if g_norm.starts_with('=') {
                    if is_subtotal_interop
                        && is_subtotal_following_label_replaced_by_interop(v_cell)
                    {
                        if lr < HEADER_ROWS {
                            // Top/header band: e.g. `=TOTAL` displays as the word TOTAL in Values while
                            // Generic uses `=SUBTOTAL(9,…)` for the same column; not a data-cell bug.
                            continue;
                        }
                        panic!(
                            "Values TSV must show computed subtotal, not label text {v_cell:?}; \
                             Generic is {g_cell:?} at matrix[{i}][{j}] (lr={lr} gc={gc})"
                        );
                    }
                    // `=SUBTOTAL(4,…,A1:B10)` never “evaluates to” the string `MAX` — it evaluates to
                    // a *number*; the matrix cannot assert that here without a separate eval, see
                    // `subtotal4_eval_result_is_not_string_max` below. We only `continue`d before,
                    // which hid the label-vs-formula mix-up for bare `MAX` until the bare-agg branch
                    // above and that test existed.
                    continue;
                }
                assert_eq!(
                    g_cell, &computed,
                    "Generic non-formula at matrix[{i}][{j}] (logical row {lr}, col {gc})"
                );
            }
        }
    }

    /// A right-margin cell that holds the plain word `TOTAL` (not the `=TOTAL` aggregate directive
    /// and with no column header `=TOTAL`) must stay the literal `TOTAL` in Generic export, not
    /// `=SUBTOTAL(9,…)` over the main block.
    #[test]
#[inline(always)]
    fn generic_bare_text_total_in_cell_stays_total_not_subtotal_range() {
        use crate::grid::HEADER_ROWS;

        let mut grid = crate::grid::Grid::new(10, 2);
        for r in 0..10 {
            grid.set(
                &CellAddr::Main {
                    row: r,
                    col: 0,
                },
                format!("{}", (r + 1) * 10).into(),
            );
            grid.set(
                &CellAddr::Main {
                    row: r,
                    col: 1,
                },
                format!("{}", (r + 1) * 10).into(),
            );
        }
        grid.set(&CellAddr::Right { col: 0, row: 9 }, "TOTAL".into());
        let gb = crate::grid::GridBox::from(grid);
        let re = delimited_default_generic_rebase(&gb);
        let lr = HEADER_ROWS + 9;
        let gc = MARGIN_COLS + 2;
        let g = export_cell_text(
            &gb,
            lr,
            gc,
            ExportContent::Generic,
            Some(re),
            true,
        );
        assert_eq!(g.trim(), "TOTAL");
        assert!(
            !g.contains("SUBTOTAL"),
            "expected bare label, not an interop formula, got {g:?}"
        );
    }

    /// A real `=SUBTOTAL(4,…)` evaluates to a **number** (function 4 = MAX in Excel/ODF), never to
    /// the *word* `MAX` — a fact the delimited matrix test does not see when it `continue`s on
    /// `=…` Generic cells. This pins that semantic.
    #[test]
#[inline(always)]
    fn subtotal4_eval_result_is_not_string_max() {
        use crate::grid::HEADER_ROWS;

        // One column, three rows, max is 5; one cell with real interop =SUBTOTAL(4,…, range)
        let mut grid = crate::grid::Grid::new(3, 1);
        for r in 0..3 {
            let v = [1, 5, 2][r];
            grid.set(
                &CellAddr::Main {
                    row: r as u32,
                    col: 0,
                },
                v.to_string().into(),
            );
        }
        grid.set(
            &CellAddr::Right { col: 0, row: 2 },
            "=SUBTOTAL(4;A1:A3)".into(),
        );
        let gb = crate::grid::GridBox::from(grid);
        let lr = HEADER_ROWS + 2;
        let gc = MARGIN_COLS + 1;
        let v = export_cell_text(&gb, lr, gc, ExportContent::Values, None, true);
        assert!(!v.trim().eq_ignore_ascii_case("MAX"), "result was {v:?}, want numeric max, not the label word MAX; =SUBTOTAL(4,…) is not a MAX string");
    }

    #[test]
#[inline(always)]
    fn tsv_export_keeps_left_margin_columns() {
        let mut grid = crate::grid::Grid::new(2, 2);
        grid.set(&CellAddr::Header { row: 0, col: crate::grid::ColumnAddr::Left(0) }, "HDR".into());
        grid.set(&CellAddr::Left { row: 0, col: 0 }, "L0".into());
        grid.set(&CellAddr::Main { row: 0, col: 0 }, "A0".into());
        grid.set(&CellAddr::Main { row: 0, col: 1 }, "B0".into());
        grid.set(&CellAddr::Right { row: 0, col: 0 }, "R0".into());
        grid.set(&CellAddr::Footer { row: 0, col: crate::grid::ColumnAddr::Left(0) }, "FTR".into());
        let gb = crate::grid::GridBox::from(grid);
        let mut out = Vec::new();
        export_tsv(&gb, &mut out);
        let tsv = String::from_utf8(out).unwrap();
        assert!(tsv.contains("HDR"));
        assert!(tsv.contains("L0"));
        assert!(tsv.contains("A0\tB0"));
        assert!(tsv.contains("R0"));
        assert!(tsv.contains("FTR"));
    }

    #[test]
#[inline(always)]
    fn csv_and_tsv_exports_match_for_docs_fixtures() {
        let fixtures_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/tests");
        let mut fixtures: Vec<PathBuf> = std::fs::read_dir(&fixtures_dir)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.extension().and_then(|s| s.to_str()) == Some("corro"))
            .collect();
        fixtures.sort();

        for fixture in fixtures {
            let workbook = load_fixture(&fixture);
            let grid = &workbook.active_sheet().grid;
            let (matrix, ..) = delimited_export_matrix(grid, &DelimitedExportOptions::default());
            let csv = matrix_render_same_as_export(&matrix, ',');
            let tsv = matrix_render_same_as_export(&matrix, '\t');
            let csv_rows = parse_delimited(&csv, ',');
            let tsv_rows = parse_delimited(&tsv, '\t');
            assert_eq!(
                csv_rows,
                tsv_rows,
                "export mismatch for {}",
                fixture.display()
            );
        }
    }

    #[test]
#[inline(always)]
    fn delimited_omit_header_row_starts_with_data() {
        let mut grid = crate::grid::Grid::new(2, 2);
        grid.set(&CellAddr::Main { row: 0, col: 0 }, "V1".into());
        grid.set(&CellAddr::Main { row: 0, col: 1 }, "V2".into());
        let gb = crate::grid::GridBox::from(grid);
        let opts = DelimitedExportOptions {
            include_header_row: false,
            include_margins: false,
            include_row_label_column: false,
            ..Default::default()
        };
        let mut out = Vec::new();
        export_tsv_with_options(&gb, &mut out, &opts);
        let tsv = String::from_utf8(out).unwrap();
        let first = tsv.lines().next().expect("at least one line");
        assert_eq!(first, "V1\tV2", "first line should be data, not A/B");
    }

    #[test]
#[inline(always)]
    fn delimited_main_only_can_keep_row_key_without_margins() {
        let mut grid = crate::grid::Grid::new(2, 2);
        grid.set(&CellAddr::Main { row: 0, col: 0 }, "V1".into());
        grid.set(&CellAddr::Main { row: 0, col: 1 }, "V2".into());
        let gb = crate::grid::GridBox::from(grid);
        let opts = DelimitedExportOptions {
            include_header_row: false,
            include_margins: false,
            include_row_label_column: true,
            ..Default::default()
        };
        let mut out = Vec::new();
        export_tsv_with_options(&gb, &mut out, &opts);
        let tsv = String::from_utf8(out).unwrap();
        let first = tsv.lines().next().expect("at least one line");
        assert!(
            first.starts_with("1\t"),
            "main-only TSV with row# on should start with row label: {first:?}"
        );
        assert!(first.contains("V1"));
    }

    #[test]
#[inline(always)]
    fn generic_tsv_uses_tsv_header_and_interop_formula() {
        use crate::grid::HEADER_ROWS;

        let mut grid = crate::grid::Grid::new(2, 2);
grid.set(
            &CellAddr::Header {
                row: (HEADER_ROWS - 1) as u32,
                col: crate::grid::ColumnAddr::Main(1),
            },
            "=A*0.1 -- TAX".into()
        );
        grid.set(&CellAddr::Main { row: 0, col: 0 }, "100".into());
        let gb = crate::grid::GridBox::from(grid);
        let opts = DelimitedExportOptions {
            content: ExportContent::Generic,
            include_margins: false,
            include_header_row: true,
            include_row_label_column: false,
            ..Default::default()
        };
        let mut out = Vec::new();
        export_tsv_with_options(&gb, &mut out, &opts);
        let tsv = String::from_utf8(out).unwrap();
        let lines: Vec<_> = tsv.lines().collect();
        assert_eq!(lines[0], "A\tB", "synthetic A/B; labeled header text is on the ~1 control row");
        // `row_order` lists non-empty header rows before the main block, so the control-strip row
        // appears as an extra data line before the first main data row.
        assert!(
            lines
                .iter()
                .any(|l| *l == "\tTAX" || l.ends_with("\tTAX")),
            "TAX replaces =A*0.1 on the control row, got {lines:?}"
        );
        let main_row = lines
            .iter()
            .find(|l| l.starts_with("100\t"))
            .expect("main data row with 100 in column A");
        // d_row=2, d_col=0: two body lines (control strip + main) before the first data row, no
        // left columns in this main-only export, so =A1*0.1 => =A3*0.1 (file A1 = top-left).
        assert!(
            (main_row.contains("A3*0.1") && main_row.contains('='))
                || main_row.contains("(A3*0.1)"),
            "TAX column rebased, got {main_row:?}"
        );
    }

    /// A real `=SUBTOTAL(4;…)` (stored formula) should use `,` in function lists for Excel/TSV.
    /// Bare `MAX` labels no longer go through that path (see `subtotal_code_for_label`).
    #[test]
#[inline(always)]
    fn generic_tsv_subtotal_formula_uses_excel_list_commas() {
        use crate::grid::HEADER_ROWS;

        let mut grid = crate::grid::Grid::new(1, 2);
        grid.set(
            &CellAddr::Header {
                row: (HEADER_ROWS - 1) as u32,
                col: ColumnAddr::Main(1),
            },
            "MAX".into(),
        );
        grid.set(&CellAddr::Main { row: 0, col: 0 }, "3".into());
        grid.set(
            &CellAddr::Right { col: 0, row: 0 },
            "=SUBTOTAL(4;A1:B1)".into(),
        );
        let gb = crate::grid::GridBox::from(grid);
        let opts = DelimitedExportOptions {
            content: ExportContent::Generic,
            include_margins: true,
            include_header_row: true,
            include_row_label_column: true,
            ..Default::default()
        };
        let mut out = Vec::new();
        export_tsv_with_options(&gb, &mut out, &opts);
        let tsv = String::from_utf8(out).unwrap();
        assert!(tsv.contains("=SUBTOTAL(4,"), "expect Excel comma, got {tsv}");
    }

    /// "Target" is the **Generic** formula TSV ([`ExportContent::Generic`]), interpreted as its own
    /// spreadsheet: row 0 is the synthetic `A/B/...` header and formulas are already rebased to that
    /// file layout. After a **data** change on the source grid, apply the *identical* field change to a
    /// snapshot of the Generic target. The edited target must equal a full re-export, and its computed
    /// Values must match the edited source for every exported main data cell (including formulas whose
    /// values changed because of the edit).
    #[test]
#[inline(always)]
    fn generic_tsv_target_parallel_data_edit_values_match_source() {
#[inline(always)]
        fn target_grid_from_matrix(m: &[Vec<String>]) -> crate::grid::GridBox {
            let rows = m.len().max(1);
            let cols = m.iter().map(|r| r.len()).max().unwrap_or(1).max(1);
            let mut g = crate::grid::Grid::new(rows as u32, cols as u32);
            for (r, row) in m.iter().enumerate() {
                for (c, val) in row.iter().enumerate() {
                    if !val.is_empty() {
                        g.set(
                            &CellAddr::Main {
                                row: r as u32,
                                col: c as u32,
                            },
                            val.clone(),
                        );
                    }
                }
            }
            crate::grid::GridBox::from(g)
        }

        let mut g = crate::grid::Grid::new(2, 5);
        g.set(&CellAddr::Main { row: 0, col: 0 }, "5".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "3".into());
        g.set(&CellAddr::Main { row: 0, col: 2 }, "=A1+B1".into());
        g.set(&CellAddr::Main { row: 0, col: 3 }, "=C1*2".into());
        g.set(&CellAddr::Main { row: 0, col: 4 }, "=D1-A1".into());
        g.set(&CellAddr::Main { row: 1, col: 0 }, "10".into());
        g.set(&CellAddr::Main { row: 1, col: 1 }, "4".into());
        g.set(&CellAddr::Main { row: 1, col: 2 }, "=A2+B2".into());
        g.set(&CellAddr::Main { row: 1, col: 3 }, "=C2+A1".into());
        g.set(&CellAddr::Main { row: 1, col: 4 }, "=D2-C1".into());
        let opts = DelimitedExportOptions {
            content: ExportContent::Generic,
            include_margins: false,
            include_header_row: true,
            include_row_label_column: false,
            ..Default::default()
        };
        let (matrix0, _c0, _c1, _rows) =
            delimited_export_matrix(&crate::grid::GridBox::from(g.clone()), &opts);
        let old_target = target_grid_from_matrix(&matrix0);
        let h = 1usize; // A | B | C | D | E
        assert!(matrix0.len() > h, "header + at least one data line");
        assert!(
            matrix0[h][2].contains("A2+B2"),
            "formula should be rebased into target file layout, got {:?}",
            matrix0[h][2]
        );

        let before_formula_value = export_cell_text(
            &old_target,
            crate::grid::HEADER_ROWS + h,
            MARGIN_COLS + 2,
            ExportContent::Values,
            None,
            true,
        );

        g.set(&CellAddr::Main { row: 0, col: 0 }, "6".into());
        let gb_after = crate::grid::GridBox::from(g.clone());
        let (matrix_fresh, ..) = delimited_export_matrix(&gb_after, &opts);

        let mut m_parallel = matrix0.clone();
        m_parallel[h][0] = "6".into();
        assert_eq!(
            m_parallel, matrix_fresh,
            "the Generic-export 'target' with the same one-cell edit as the source must match a full re-export"
        );

        let edited_target = target_grid_from_matrix(&m_parallel);
        let after_formula_value = export_cell_text(
            &edited_target,
            crate::grid::HEADER_ROWS + h,
            MARGIN_COLS + 2,
            ExportContent::Values,
            None,
            true,
        );
        assert_ne!(
            before_formula_value, after_formula_value,
            "formula value should change after the parallel data edit, proving the test exercises computation"
        );

        let lr = crate::grid::HEADER_ROWS;
        for source_row in 0..2 {
            for source_col in 0..5 {
                let source_gc = MARGIN_COLS + source_col;
                let target_lr = lr + h + source_row;
                let target_gc = MARGIN_COLS + source_col;
                let v_src = export_cell_text(
                    &gb_after,
                    lr + source_row,
                    source_gc,
                    ExportContent::Values,
                    None,
                    true,
                );
                let v_target = export_cell_text(
                    &edited_target,
                    target_lr,
                    target_gc,
                    ExportContent::Values,
                    None,
                    true,
                );
                assert_eq!(
                    v_src, v_target,
                    "Values mismatch after same edit: source main row {source_row} col {source_col} vs Generic target row {} col {}",
                    h + source_row,
                    source_col
                );
            }
        }
    }

    /// Catches the obvious Generic-export failure:
    /// `2  Hammers  5  =(C4*0.1)  5.5`
    /// where the right-margin `TOTAL` cell is a static value. If only the source data cell and the
    /// corresponding exported target field change, the target's total must still recompute.
    #[test]
#[inline(always)]
    fn generic_tsv_target_right_margin_total_recomputes_after_parallel_data_edit() {
#[inline(always)]
        fn target_grid_from_matrix(m: &[Vec<String>]) -> crate::grid::GridBox {
            let rows = m.len().max(1);
            let cols = m.iter().map(|r| r.len()).max().unwrap_or(1).max(1);
            let mut g = crate::grid::Grid::new(rows as u32, cols as u32);
            for (r, row) in m.iter().enumerate() {
                for (c, val) in row.iter().enumerate() {
                    if !val.is_empty() {
                        g.set(
                            &CellAddr::Main {
                                row: r as u32,
                                col: c as u32,
                            },
                            val.clone(),
                        );
                    }
                }
            }
            crate::grid::GridBox::from(g)
        }

        let mut g = crate::grid::Grid::new(2, 2);
        g.set(
            &CellAddr::Header {
                row: (HEADER_ROWS - 1) as u32,
                col: ColumnAddr::Main(1),
            },
            "=A*0.1 -- TAX".into(),
        );
        g.set(
            &CellAddr::Header {
                row: (HEADER_ROWS - 1) as u32,
                col: ColumnAddr::Main(2),
            },
            "=TOTAL".into(),
        );
        g.set(&CellAddr::Left { col: MARGIN_COLS - 1, row: 0 }, "Hammers".into());
        g.set(&CellAddr::Main { row: 0, col: 0 }, "5".into());

        let opts = DelimitedExportOptions {
            content: ExportContent::Generic,
            include_margins: true,
            include_header_row: true,
            include_row_label_column: true,
            ..Default::default()
        };
        let (matrix0, col_start, _col_end, rows) =
            delimited_export_matrix(&crate::grid::GridBox::from(g.clone()), &opts);
        let h = 1usize;
        let data_row = h + rows.iter().position(|&r| r == HEADER_ROWS).unwrap();
        let amount_field = 1 + (MARGIN_COLS - col_start);
        let total_field = 1 + (MARGIN_COLS + 2 - col_start);
        assert_eq!(matrix0[data_row][amount_field], "5");
        assert!(
            matrix0[data_row][total_field].starts_with('='),
            "right-margin =TOTAL must become a formula in the Generic target, not a static value like 5.5; got {:?}",
            matrix0[data_row][total_field]
        );
        assert!(
            matrix0[data_row][total_field].contains("C3:D3"),
            "right-margin TOTAL formula should reference the target row's amount/tax fields; got {:?}",
            matrix0[data_row][total_field]
        );

        g.set(&CellAddr::Main { row: 0, col: 0 }, "6".into());
        let source_after = crate::grid::GridBox::from(g);

        let mut edited_target_matrix = matrix0.clone();
        edited_target_matrix[data_row][amount_field] = "6".into();
        let edited_target = target_grid_from_matrix(&edited_target_matrix);

        let source_total = export_cell_text(
            &source_after,
            HEADER_ROWS,
            MARGIN_COLS + 2,
            ExportContent::Values,
            None,
            true,
        );
        let target_total = export_cell_text(
            &edited_target,
            HEADER_ROWS + data_row,
            MARGIN_COLS + total_field,
            ExportContent::Values,
            None,
            true,
        );
        assert_eq!(
            source_total, target_total,
            "Generic target right-margin TOTAL must update after the same source data edit"
        );
    }

    /// Footer aggregates (`_2 TOTAL`, `_3 MAX`, `_4 MIN` in subtotal.corro) must not be static
    /// rendered Values in Generic export. If the exported target's source data changes, the footer
    /// must recompute from formulas over the target columns.
    #[test]
#[inline(always)]
    fn generic_tsv_target_footer_total_recomputes_after_parallel_data_edit() {
#[inline(always)]
        fn target_grid_from_matrix(m: &[Vec<String>]) -> crate::grid::GridBox {
            let rows = m.len().max(1);
            let cols = m.iter().map(|r| r.len()).max().unwrap_or(1).max(1);
            let mut g = crate::grid::Grid::new(rows as u32, cols as u32);
            for (r, row) in m.iter().enumerate() {
                for (c, val) in row.iter().enumerate() {
                    if !val.is_empty() {
                        g.set(
                            &CellAddr::Main {
                                row: r as u32,
                                col: c as u32,
                            },
                            val.clone(),
                        );
                    }
                }
            }
            crate::grid::GridBox::from(g)
        }

        let mut g = crate::grid::Grid::new(1, 2);
        g.set(
            &CellAddr::Header {
                row: (HEADER_ROWS - 1) as u32,
                col: ColumnAddr::Main(1),
            },
            "=A*0.1 -- TAX".into(),
        );
        g.set(
            &CellAddr::Header {
                row: (HEADER_ROWS - 1) as u32,
                col: ColumnAddr::Main(2),
            },
            "=TOTAL".into(),
        );
        g.set(&CellAddr::Left { col: MARGIN_COLS - 1, row: 0 }, "Hammers".into());
        g.set(&CellAddr::Main { row: 0, col: 0 }, "5".into());
        g.set(&CellAddr::Left { col: MARGIN_COLS - 1, row: 1 }, "=TOTAL".into());
        g.set(
            &CellAddr::Footer {
                row: 0,
                col: ColumnAddr::Left(MARGIN_COLS - 1),
            },
            "=TOTAL".into(),
        );

        let opts = DelimitedExportOptions {
            content: ExportContent::Generic,
            include_margins: true,
            include_header_row: true,
            include_row_label_column: true,
            ..Default::default()
        };
        let (matrix0, col_start, _col_end, rows) =
            delimited_export_matrix(&crate::grid::GridBox::from(g.clone()), &opts);
        let h = 1usize;
        let data_row = h + rows.iter().position(|&r| r == HEADER_ROWS).unwrap();
        let subtotal_row = h
            + rows
                .iter()
                .position(|&r| r == HEADER_ROWS + 1)
                .unwrap();
        let footer_row = h
            + rows
                .iter()
                .position(|&r| r == HEADER_ROWS + g.main_rows())
                .unwrap();
        let amount_field = 1 + (MARGIN_COLS - col_start);
        let total_field = 1 + (MARGIN_COLS + 2 - col_start);
        assert_eq!(matrix0[data_row][amount_field], "5");
        assert!(
            matrix0[subtotal_row][amount_field].starts_with('='),
            "main TOTAL row amount must be a formula, not a hardcoded subtotal; got {:?}",
            matrix0[subtotal_row][amount_field]
        );
        assert!(
            matrix0[subtotal_row][amount_field].contains("C3:C3"),
            "main TOTAL row should sum only the preceding raw block; got {:?}",
            matrix0[subtotal_row][amount_field]
        );
        assert!(
            matrix0[footer_row][total_field].starts_with('='),
            "footer TOTAL must be a formula in Generic export, not a hardcoded value like 5.5; got {:?}",
            matrix0[footer_row][total_field]
        );
        assert!(
            matrix0[footer_row][total_field].contains("E3:E3"),
            "footer TOTAL must skip the main TOTAL row to avoid double counting; got {:?}",
            matrix0[footer_row][total_field]
        );

        g.set(&CellAddr::Main { row: 0, col: 0 }, "6".into());
        let source_after = crate::grid::GridBox::from(g);

        let mut edited_target_matrix = matrix0.clone();
        edited_target_matrix[data_row][amount_field] = "6".into();
        let edited_target = target_grid_from_matrix(&edited_target_matrix);

        let source_footer_total = export_cell_text(
            &source_after,
            HEADER_ROWS + source_after.main_rows(),
            MARGIN_COLS + 2,
            ExportContent::Values,
            None,
            true,
        );
        let target_footer_total = export_cell_text(
            &edited_target,
            HEADER_ROWS + footer_row,
            MARGIN_COLS + total_field,
            ExportContent::Values,
            None,
            true,
        );
        assert_eq!(
            source_footer_total, target_footer_total,
            "Generic target footer TOTAL must update after the same source data edit"
        );
    }

    /// TSV/ODS generic strings match after `,` / `;` in function-argument positions (Subtotal).
#[test]
#[inline(always)]
fn generic_ods_reuses_tsv_interop_excel_list_swap() {
        use crate::grid::HEADER_ROWS;

        let m = MARGIN_COLS;
        let mut grid = crate::grid::Grid::new(1, 2);
        grid.set(
            &CellAddr::Header {
                row: (HEADER_ROWS - 1) as u32,
                col: ColumnAddr::Main(2),
            },
            "MAX".into(),
        );
        grid.set(&CellAddr::Main { row: 0, col: 0 }, "3".into());
        grid.set(&CellAddr::Right { col: 0, row: 0 },
            "=SUBTOTAL(4;A1:B1)".into());
        let gb = crate::grid::GridBox::from(grid);
        let re = delimited_default_generic_rebase(&gb);
        let lr = HEADER_ROWS;
        let gc = m + 2;
        let tsv_s = generic_interop_cell_text(&gb, lr, gc, Some(re), true).expect("tsv interop");
        let ods_s = generic_interop_cell_text(&gb, lr, gc, Some(re), false).expect("ods interop");
        assert_eq!(tsv_s, super::interop_excel_list_separators(&ods_s));
    }

    /// [aggregate_formula_over_runs] joins multiple non-contiguous ranges with `;` (ODF). LibreOffice
    /// reported Err:508 on `MAX(A1:B1,A2:B2)`-style commas; TSV still uses commas after
    /// [interop_excel_list_separators].
    #[test]
#[inline(always)]
    fn generic_footer_max_multirange_ods_semicolon_tsv_comma() {
        use crate::grid::HEADER_ROWS;

        let mut g = crate::grid::Grid::new(4, 2);
        g.set(
            &CellAddr::Left {
                col: MARGIN_COLS - 1,
                row: 0,
            },
            "a".into(),
        );
        g.set(
            &CellAddr::Left {
                col: MARGIN_COLS - 1,
                row: 1,
            },
            "b".into(),
        );
        g.set(
            &CellAddr::Left {
                col: MARGIN_COLS - 1,
                row: 2,
            },
            "=TOTAL".into(),
        );
        g.set(
            &CellAddr::Left {
                col: MARGIN_COLS - 1,
                row: 3,
            },
            "c".into(),
        );
        g.set(&CellAddr::Main { row: 0, col: 0 }, "1".into());
        g.set(&CellAddr::Main { row: 1, col: 0 }, "2".into());
        g.set(&CellAddr::Main { row: 3, col: 0 }, "3".into());
        g.set(
            &CellAddr::Footer {
                row: 0,
                col: ColumnAddr::Left(MARGIN_COLS - 1),
            },
            "MAX".into(),
        );
        let gb = crate::grid::GridBox::from(g);
        let re = delimited_default_generic_rebase(&gb);
        let lr = HEADER_ROWS + gb.main_rows();
        let gc = MARGIN_COLS;
        let tsv_s = generic_interop_cell_text(&gb, lr, gc, Some(re), true).expect("tsv");
        let ods_s = generic_interop_cell_text(&gb, lr, gc, Some(re), false).expect("ods");
        assert!(
            ods_s.contains("MAX(") && ods_s.contains(';'),
            "ODF interop should use `;` between MAX range args: {ods_s:?}"
        );
        assert_eq!(tsv_s, super::interop_excel_list_separators(&ods_s));
    }

    #[test]
#[inline(always)]
    fn selection_omit_header_row_is_data_first() {
        use crate::grid::HEADER_ROWS;

        let mut grid = crate::grid::Grid::new(2, 2);
        grid.set(&CellAddr::Main { row: 0, col: 0 }, "a".into());
        let gb = crate::grid::GridBox::from(grid);
        let m = MARGIN_COLS;
        let mut out = Vec::new();
        export_selection(
            &gb,
            &mut out,
            &[HEADER_ROWS],
            &[m, m + 1],
            &DelimitedExportOptions {
                include_header_row: false,
                ..Default::default()
            },
        );
        let s = String::from_utf8(out).unwrap();
        let first = s.lines().next().expect("at least one line");
        assert_eq!(first, "a\t", "first line is data, not column labels");

        let mut out2 = Vec::new();
        export_selection(
            &gb,
            &mut out2,
            &[HEADER_ROWS],
            &[m, m + 1],
            &DelimitedExportOptions {
                include_header_row: true,
                ..Default::default()
            },
        );
        let s2 = String::from_utf8(out2).unwrap();
        let first2 = s2.lines().next().expect("at least one line");
        assert_eq!(first2, "A\tB", "with header, first line is A/B for two main columns");
    }

    #[test]
#[inline(always)]
    fn ascii_ems_space_in_cell_glue() {
        let mut g = crate::grid::Grid::new(3, 1);
        g.set(&CellAddr::Main { row: 0, col: 0 }, "x".into());
        let gb = crate::grid::GridBox::from(g);
        let em = '\u{2003}';
        let o = AsciiTableOptions {
            inter_cell_space: AsciiInterCellSpace::EmSpace,
            ..Default::default()
        };
        let mut out = Vec::new();
        export_ascii_table_with_options(&gb, &mut out, &o);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains(em), "expected em U+2003 in output");
    }

    #[test]
#[inline(always)]
    fn ascii_omit_row_label_column_starts_with_column_not_row_numbers() {
        let mut g = crate::grid::Grid::new(3, 1);
        g.set(&CellAddr::Main { row: 0, col: 0 }, "x".into());
        let gb = crate::grid::GridBox::from(g);
        let o = AsciiTableOptions {
            include_row_label_column: false,
            include_margins: false,
            ..Default::default()
        };
        let mut out = Vec::new();
        export_ascii_table_with_options(&gb, &mut out, &o);
        let s = String::from_utf8(out).unwrap();
        let data_line = s.lines().find(|l| l.contains("x")).expect("data row");
        assert!(
            !data_line.contains("  1  ") && !data_line.contains("| 1 |"),
            "no row-number gutter: {data_line}"
        );
        assert!(data_line.contains("x"));
    }

    #[test]
#[inline(always)]
    fn ascii_omit_column_label_row_goes_straight_to_data() {
        let mut g = crate::grid::Grid::new(3, 1);
        g.set(&CellAddr::Main { row: 0, col: 0 }, "val".into());
        let gb = crate::grid::GridBox::from(g);
        let o = AsciiTableOptions {
            include_column_label_row: false,
            ..Default::default()
        };
        let mut out = Vec::new();
        export_ascii_table_with_options(&gb, &mut out, &o);
        let s = String::from_utf8(out).unwrap();
        let lines: Vec<_> = s.lines().collect();
        assert!(lines.len() >= 2);
        // After top border, first line is a data row (row label + cell), not a column-letter row.
        assert!(
            lines[1].contains("val"),
            "line after top border should be data, got: {:?}",
            lines[1]
        );
    }

    #[test]
#[inline(always)]
    fn ascii_header_data_separator_none_drops_line_between_label_and_data() {
        let mut g = crate::grid::Grid::new(3, 1);
        g.set(&CellAddr::Main { row: 0, col: 0 }, "z".into());
        let gb = crate::grid::GridBox::from(g);
        let o_full = AsciiTableOptions {
            header_data_separator: AsciiHeaderDataSeparator::FullBorder,
            ..Default::default()
        };
        let o_none = AsciiTableOptions {
            header_data_separator: AsciiHeaderDataSeparator::None,
            ..Default::default()
        };
        let mut out_full = Vec::new();
        let mut out_none = Vec::new();
        export_ascii_table_with_options(&gb, &mut out_full, &o_full);
        export_ascii_table_with_options(&gb, &mut out_none, &o_none);
        let full = String::from_utf8(out_full).unwrap();
        let none = String::from_utf8(out_none).unwrap();
        assert!(
            full.lines().count() > none.lines().count(),
            "FullBorder should add a line under labels: full={} none={}",
            full.lines().count(),
            none.lines().count()
        );
    }
}
