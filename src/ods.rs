//! ODS import/export for workbook data.

use crate::addr::excel_column_name;
use crate::export::{self, DelimitedExportOptions, ExportContent};
use crate::formula::{cell_effective_display, is_formula};
use crate::grid::{CellAddr, ColumnAddr, CellFormat, GridBox as Grid, NumberFormat, TextAlign, HEADER_ROWS, MARGIN_COLS};
use crate::ops::{SheetRecord, SheetState, WorkbookSnapshot, WorkbookState};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use thiserror::Error;
use zip::write::FileOptions;

#[derive(Debug, Error)]
pub enum OdsError {
    #[error("ODS: {0}")]
    Io(#[from] std::io::Error),
    #[error("ODS XML: {0}")]
    Xml(String),
    #[error("ODS archive: {0}")]
    Zip(#[from] zip::result::ZipError),
}

/// Corro re-import: if the first `table:table-row` uses a `number-rows-repeated` this large,
/// ODF table rows are treated as full logical (header + margins + main + footer). Otherwise, ODF
/// rows are mapped directly onto the main grid (interop, including compact export).
const ODS_GLOBAL_LAYOUT_MIN_FIRST_ROW_REPEAT: u64 = 1_000_000;
const CORRO_ODS_LAYOUT_PATH: &str = "corro-ods-layout";

/// How ODF `style:name` and `table:style-name` are generated. Single-sheet ODS is unchanged
/// (`co0`, `co1`, …). Multi-sheet workbooks use a per-sheet prefix so column styles are unique.
#[derive(Clone, Copy)]
enum OdsColumnStyleNaming {
    /// Legacy single-table: `co{c}`.
    Legacy,
    /// Nth sheet in a multi-sheet ODS: `c{n}_{c}`.
    Sheet(usize),
}

impl OdsColumnStyleNaming {
    fn style_name(self, c: usize) -> String {
        match self {
            OdsColumnStyleNaming::Legacy => format!("co{c}"),
            OdsColumnStyleNaming::Sheet(n) => format!("c{}_{c}", n),
        }
    }
}

fn ods_xml_attr_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => o.push_str("&amp;"),
            '"' => o.push_str("&quot;"),
            '<' => o.push_str("&lt;"),
            _ => o.push(ch),
        }
    }
    o
}

/// How to interpret 0-based ODF table `row_idx` when writing into the Corro grid. Corro exports
/// include a `corro-ods-layout` sidecar; files without it use the first row's
/// [ODS_GLOBAL_LAYOUT_MIN_FIRST_ROW_REPEAT] as before (LibreOffice / interop ODS).
#[derive(Clone)]
enum OdsTableLayout {
    Odf,
    Rebase { min: usize },
    Compact { physical_to_logical: Vec<usize> },
    /// ODF table matches default TSV shape: optional synthetic header row, row-key column, then
    /// `data_logical_rows.len()` data rows. Cell (`odf_r`,`odf_c`) maps to
    /// `(data_logical_rows[odf_r - header_ods], col_start + odf_c - row_key)`.
    TsvParity {
        col_start: usize,
        col_end: usize,
        data_logical_rows: Vec<usize>,
        header_ods_rows: usize,
        row_key_cols: usize,
        /// Main column count on export (enables re-import to classify B vs first right-marginal col).
        export_main_cols: Option<usize>,
    },
}

impl OdsTableLayout {
    /// Map 0-based ODF table (row, col) to Corro `set_ods_cell` coordinates. `None` = synthetic
    /// (TSV header / row key) or out of range: do not write the grid.
    fn map_ods_table_cell(
        &self,
        odf_table_row: usize,
        odf_table_col: usize,
        odf_uses_full_logical: bool,
    ) -> Option<(usize, usize, bool)> {
        match self {
            OdsTableLayout::TsvParity {
                col_start,
                data_logical_rows,
                header_ods_rows,
                row_key_cols,
                ..
            } => {
                if odf_table_row < *header_ods_rows {
                    return None;
                }
                if *row_key_cols > 0 && odf_table_col < *row_key_cols {
                    return None;
                }
                let i = odf_table_row - header_ods_rows;
                if i >= data_logical_rows.len() {
                    return None;
                }
                let lr = data_logical_rows[i];
                let gc = col_start + (odf_table_col - row_key_cols);
                Some((lr, gc, true))
            }
            OdsTableLayout::Odf => {
                let g = odf_uses_full_logical;
                Some((odf_table_row, odf_table_col, g))
            }
            OdsTableLayout::Rebase { min } => Some((
                min.saturating_add(odf_table_row),
                odf_table_col,
                true,
            )),
            OdsTableLayout::Compact {
                physical_to_logical,
            } => {
                let lr = physical_to_logical
                    .get(odf_table_row)
                    .copied()
                    .unwrap_or(odf_table_row);
                Some((lr, odf_table_col, true))
            }
        }
    }
}

/// Body of a `tsv` layout block: numbers only (no `v1` / `tsv` header).
/// Trailing `export_main_cols` helps ODS re-import classify B vs the first right-marginal column
/// when `main_cols()` in the in-progress grid is still 1.
fn corro_ods_layout_tsv_parity_block_lines(
    col_start: usize,
    col_end: usize,
    header_ods_rows: usize,
    row_key_cols: usize,
    data_logical_rows: &[usize],
    export_main_cols: usize,
) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(&mut s, "{}", col_start);
    let _ = writeln!(&mut s, "{}", col_end);
    let _ = writeln!(&mut s, "{}", header_ods_rows);
    let _ = writeln!(&mut s, "{}", row_key_cols);
    let _ = writeln!(&mut s, "{}", data_logical_rows.len());
    for r in data_logical_rows {
        let _ = writeln!(s, "{}", r);
    }
    let _ = writeln!(&mut s, "{}", export_main_cols);
    s
}

fn corro_ods_layout_tsv_parity(
    col_start: usize,
    col_end: usize,
    header_ods_rows: usize,
    row_key_cols: usize,
    data_logical_rows: &[usize],
    export_main_cols: usize,
) -> String {
    let block = corro_ods_layout_tsv_parity_block_lines(
        col_start,
        col_end,
        header_ods_rows,
        row_key_cols,
        data_logical_rows,
        export_main_cols,
    );
    format!("v1\ntsv\n{}", block)
}

/// Multi-sheet export: one `tsv` block per `table:table`, same numeric layout as
/// [`corro_ods_layout_tsv_parity_block_lines`].
fn corro_ods_layout_v2(sheet_blocks: &[String]) -> String {
    use std::fmt::Write;
    let mut s = String::from("v2\n");
    let _ = writeln!(&mut s, "{}", sheet_blocks.len());
    for block in sheet_blocks {
        s.push_str("tsv\n");
        s.push_str(block);
    }
    s
}

/// Parse a `tsv` numeric block starting at `lines[start]` = `col_start` line.
/// Optional trailing line: `export_main_cols` (Corro 0.5+; absent in older ODS with `corro-ods-layout`).
fn parse_tsv_parity_at(lines: &[&str], start: usize) -> Option<(OdsTableLayout, usize)> {
    let col_start = lines.get(start).and_then(|l| l.parse().ok())?;
    let col_end = lines.get(start + 1).and_then(|l| l.parse().ok())?;
    let header_ods_rows = lines.get(start + 2).and_then(|l| l.parse().ok())?;
    let row_key_cols = lines.get(start + 3).and_then(|l| l.parse().ok())?;
    let n: usize = lines.get(start + 4).and_then(|l| l.parse().ok())?;
    let mut need = 5 + n;
    if lines.len() < start + need {
        return None;
    }
    let data: Vec<usize> = (0..n)
        .filter_map(|k| lines.get(start + 5 + k).and_then(|l| l.parse().ok()))
        .collect();
    if data.len() != n {
        return None;
    }
    // Optional: export main col count. Data logical rows are usually ~1e9+; "emc" is small, so
    // a short trailing line (no prior large line) is unambiguous. Next-block `tsv` is non-numeric.
    let mut export_main_cols: Option<usize> = None;
    if let Some(s) = lines.get(start + need).copied() {
        if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(v) = s.parse::<usize>() {
                // Main col counts are always far below 1e9; logical data rows in Corro are ~1e9+.
                let looks_like_emc = n == 0 || v < 1_000_000_000;
                if looks_like_emc {
                    export_main_cols = Some(v);
                    need += 1;
                }
            }
        }
    }
    Some((
        OdsTableLayout::TsvParity {
            col_start,
            col_end,
            data_logical_rows: data,
            header_ods_rows,
            row_key_cols,
            export_main_cols,
        },
        need,
    ))
}

fn parse_corro_ods_layout_v1_lines(lines: &[&str]) -> Vec<OdsTableLayout> {
    if lines.first().copied() != Some("v1") {
        return vec![OdsTableLayout::Odf];
    }
    match lines.get(1).copied() {
        Some("rebase") => {
            if let Some(n) = lines.get(2).and_then(|s| s.parse().ok()) {
                vec![OdsTableLayout::Rebase { min: n }]
            } else {
                vec![OdsTableLayout::Odf]
            }
        }
        Some("compact") => {
            let rows: Vec<usize> = lines
                .get(2..)
                .unwrap_or(&[])
                .iter()
                .filter_map(|l| l.parse().ok())
                .collect();
            if rows.is_empty() {
                vec![OdsTableLayout::Odf]
            } else {
                vec![OdsTableLayout::Compact {
                    physical_to_logical: rows,
                }]
            }
        }
        Some("tsv") => {
            if let Some((lay, _)) = parse_tsv_parity_at(lines, 2) {
                vec![lay]
            } else {
                vec![OdsTableLayout::Odf]
            }
        }
        _ => vec![OdsTableLayout::Odf],
    }
}

fn parse_corro_ods_layout_v2_lines(lines: &[&str]) -> Vec<OdsTableLayout> {
    let n: usize = match lines.first().and_then(|s| s.parse().ok()) {
        Some(x) if x > 0 => x,
        _ => return vec![OdsTableLayout::Odf],
    };
    let mut out = Vec::with_capacity(n);
    let mut i = 1usize;
    for _ in 0..n {
        if lines.get(i) != Some(&"tsv") {
            return vec![OdsTableLayout::Odf];
        }
        i += 1;
        if let Some((lay, consumed)) = parse_tsv_parity_at(lines, i) {
            out.push(lay);
            i += consumed;
        } else {
            return vec![OdsTableLayout::Odf];
        }
    }
    out
}

fn parse_corro_ods_layout_list(buf: &str) -> Vec<OdsTableLayout> {
    let lines: Vec<&str> = buf.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.is_empty() {
        return vec![OdsTableLayout::Odf];
    }
    match lines.first().copied() {
        Some("v2") => parse_corro_ods_layout_v2_lines(&lines[1..]),
        Some("v1") => parse_corro_ods_layout_v1_lines(&lines),
        _ => vec![OdsTableLayout::Odf],
    }
}

pub fn export_ods_bytes(grid: &Grid) -> Result<Vec<u8>, OdsError> {
    let options = DelimitedExportOptions {
        content: ExportContent::Generic,
        ..Default::default()
    };
    export_ods_bytes_with_options(grid, &options)
}

/// Layout (margins, header row, row-key column) follows `options`, same as
/// [`export::export_tsv_with_options`]. Set `options.content` to choose values vs native ODF formulas vs
/// generic interop; the parameterless [export_ods_bytes] uses generic content and
/// [`DelimitedExportOptions::default`].
pub fn export_ods_bytes_with_options(
    grid: &Grid,
    options: &DelimitedExportOptions,
) -> Result<Vec<u8>, OdsError> {
    let (matrix, col_start, col_end, data_rows) = export::delimited_export_matrix(grid, options);
    let content_xml = ods_content_xml_tsv_shaped(
        grid,
        &matrix,
        &options,
        col_start,
        col_end,
        &data_rows,
    );
    let header_ods_rows = if options.include_header_row { 1 } else { 0 };
    let row_key_cols = if options.include_row_label_column { 1 } else { 0 };
    let sidecar = corro_ods_layout_tsv_parity(
        col_start,
        col_end,
        header_ods_rows,
        row_key_cols,
        &data_rows,
        grid.main_cols(),
    );

    let cursor = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(cursor);
    let opt = FileOptions::default().compression_method(zip::CompressionMethod::Stored);

    zip.start_file("mimetype", opt)?;
    zip.write_all(b"application/vnd.oasis.opendocument.spreadsheet")?;

    zip.start_file("content.xml", FileOptions::default())?;
    zip.write_all(content_xml.as_bytes())?;

    zip.start_file(CORRO_ODS_LAYOUT_PATH, FileOptions::default())?;
    zip.write_all(sidecar.as_bytes())?;

    zip.start_file("META-INF/manifest.xml", FileOptions::default())?;
    zip.write_all(ods_manifest_with_corro_layout().as_bytes())?;

    Ok(zip.finish()?.into_inner())
}

/// ODS with one `table:table` per workbook sheet, `table:name` from sheet titles, and a
/// `v2` multi-block [`CORRO_ODS_LAYOUT_PATH`] sidecar for round-trip layout. A single-sheet workbook
/// uses the same bytes as [`export_ods_bytes_with_options`] on that sheet.
pub fn export_ods_bytes_workbook_with_options(
    workbook: &WorkbookState,
    options: &DelimitedExportOptions,
) -> Result<Vec<u8>, OdsError> {
    if workbook.sheets.is_empty() {
        return Ok(Vec::new());
    }
    if workbook.sheets.len() == 1 {
        return export_ods_bytes_with_options(&workbook.sheets[0].state.grid, options);
    }
    let (content_xml, sidecar) = ods_workbook_content_xml_tsv_shaped(workbook, options);
    let cursor = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(cursor);
    let opt = FileOptions::default().compression_method(zip::CompressionMethod::Stored);

    zip.start_file("mimetype", opt)?;
    zip.write_all(b"application/vnd.oasis.opendocument.spreadsheet")?;

    zip.start_file("content.xml", FileOptions::default())?;
    zip.write_all(content_xml.as_bytes())?;

    zip.start_file(CORRO_ODS_LAYOUT_PATH, FileOptions::default())?;
    zip.write_all(sidecar.as_bytes())?;

    zip.start_file("META-INF/manifest.xml", FileOptions::default())?;
    zip.write_all(ods_manifest_with_corro_layout().as_bytes())?;

    Ok(zip.finish()?.into_inner())
}

pub fn import_ods_workbook(path: &Path) -> Result<WorkbookState, OdsError> {
    let bytes = std::fs::read(path)?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    let mut content = String::new();
    archive
        .by_name("content.xml")?
        .read_to_string(&mut content)?;
    let mut sidecar = String::new();
    let layouts = if let Ok(mut f) = archive.by_name(CORRO_ODS_LAYOUT_PATH) {
        if f.read_to_string(&mut sidecar).is_ok() {
            parse_corro_ods_layout_list(&sidecar)
        } else {
            vec![OdsTableLayout::Odf]
        }
    } else {
        vec![OdsTableLayout::Odf]
    };
    parse_ods_content_with_layout(&content, &layouts)
}

fn ods_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn ods_manifest_with_corro_layout() -> String {
    String::from(concat!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.2">
<manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/>
<manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>"#,
        r#"
<manifest:file-entry manifest:full-path="corro-ods-layout" manifest:media-type="text/plain"/>"#,
        r#"
</manifest:manifest>"#
    ))
}

fn ods_column_width_cm(char_width: usize) -> f32 {
    let chars = char_width.max(1) as f32;
    (chars * 0.20 + 0.20).max(0.45)
}

fn ods_column_styles_tsv(
    grid: &Grid,
    tc: usize,
    col_start: usize,
    has_row_key: bool,
    naming: OdsColumnStyleNaming,
) -> String {
    let mut s = String::new();
    for c in 0..tc {
        let width_chars = if has_row_key && c == 0 {
            8
        } else {
            let gc = if has_row_key {
                col_start + c.saturating_sub(1)
            } else {
                col_start + c
            };
            grid.col_width(gc)
        };
        let width_cm = ods_column_width_cm(width_chars);
        let name = naming.style_name(c);
        s.push_str(&format!(
            r#"<style:style style:name="{name}" style:family="table-column"><style:table-column-properties style:column-width="{width_cm:.2}cm"/></style:style>"#
        ));
    }
    s
}

const ODS_CONTENT_XML_PREFIX: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:of="urn:oasis:names:tc:opendocument:xmlns:of:1.2" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0" office:version="1.2"><office:automatic-styles>"#;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
struct OdsTuiFlags {
    agg_cyan: bool,
    footer_bold: bool,
    underlined_boundary_row: bool,
    left_vertical_divider: bool,
    right_vertical_divider: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum OdsHorizontalAlign {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum OdsNumberStyleKey {
    Fixed { decimals: usize },
    Currency { decimals: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct OdsCellStyleKey {
    number: Option<OdsNumberStyleKey>,
    align: Option<OdsHorizontalAlign>,
    agg_cyan: bool,
    footer_bold: bool,
    underlined_boundary_row: bool,
    left_vertical_divider: bool,
    right_vertical_divider: bool,
}

impl OdsCellStyleKey {
    fn is_default(self) -> bool {
        self.number.is_none()
            && self.align.is_none()
            && !self.agg_cyan
            && !self.footer_bold
            && !self.underlined_boundary_row
            && !self.left_vertical_divider
            && !self.right_vertical_divider
    }
}

#[derive(Default)]
struct OdsStyleRegistry {
    number_styles: BTreeMap<OdsNumberStyleKey, String>,
    cell_styles: BTreeMap<OdsCellStyleKey, String>,
    next_number_style: usize,
    next_cell_style: usize,
}

impl OdsStyleRegistry {
    fn cell_style_name(&mut self, key: OdsCellStyleKey) -> Option<String> {
        if key.is_default() {
            return None;
        }
        if let Some(nk) = key.number {
            let _ = self.number_style_name(nk);
        }
        if let Some(n) = self.cell_styles.get(&key) {
            return Some(n.clone());
        }
        self.next_cell_style += 1;
        let name = format!("ce{}", self.next_cell_style);
        self.cell_styles.insert(key, name.clone());
        Some(name)
    }

    fn number_style_name(&mut self, key: OdsNumberStyleKey) -> String {
        if let Some(n) = self.number_styles.get(&key) {
            return n.clone();
        }
        self.next_number_style += 1;
        let name = format!("ns{}", self.next_number_style);
        self.number_styles.insert(key, name.clone());
        name
    }

    fn xml(&self) -> String {
        let mut out = String::new();
        for (key, name) in &self.number_styles {
            match key {
                OdsNumberStyleKey::Fixed { decimals } => out.push_str(&format!(
                    r#"<number:number-style style:name="{name}"><number:number number:min-integer-digits="1" number:decimal-places="{decimals}" number:min-decimal-places="{decimals}"/></number:number-style>"#
                )),
                OdsNumberStyleKey::Currency { decimals } => out.push_str(&format!(
                    r#"<number:currency-style style:name="{name}"><number:currency-symbol>$</number:currency-symbol><number:number number:min-integer-digits="1" number:decimal-places="{decimals}" number:min-decimal-places="{decimals}"/></number:currency-style>"#
                )),
            }
        }
        for (key, name) in &self.cell_styles {
            let data_style_name = key.number.and_then(|n| self.number_styles.get(&n).cloned());
            let data_attr = data_style_name
                .as_deref()
                .map(|n| format!(r#" style:data-style-name="{n}""#))
                .unwrap_or_default();
            let mut props = String::new();
            match key.align {
                Some(OdsHorizontalAlign::Left) => props.push_str(r#" fo:text-align="start""#),
                Some(OdsHorizontalAlign::Center) => props.push_str(r#" fo:text-align="center""#),
                Some(OdsHorizontalAlign::Right) => props.push_str(r#" fo:text-align="end""#),
                None => {}
            }
            if key.underlined_boundary_row {
                props.push_str(r#" fo:border-bottom="0.018cm solid #6b7280""#);
            }
            if key.left_vertical_divider {
                props.push_str(r#" fo:border-right="0.018cm solid #6b7280""#);
            }
            if key.right_vertical_divider {
                props.push_str(r#" fo:border-right="0.018cm solid #6b7280""#);
            }
            let table_props = if props.is_empty() {
                String::new()
            } else {
                format!(r#"<style:table-cell-properties{props}/>"#)
            };

            let mut text_props = String::new();
            if key.agg_cyan {
                text_props.push_str(r##" fo:color="#00bcd4""##);
            }
            if key.footer_bold {
                text_props.push_str(r#" fo:font-weight="bold" style:font-weight-asian="bold" style:font-weight-complex="bold""#);
            }
            let text_props = if text_props.is_empty() {
                String::new()
            } else {
                format!(r#"<style:text-properties{text_props}/>"#)
            };
            let paragraph_props = match key.align {
                Some(OdsHorizontalAlign::Left) => {
                    r#"<style:paragraph-properties fo:text-align="start"/>"#.to_string()
                }
                Some(OdsHorizontalAlign::Center) => {
                    r#"<style:paragraph-properties fo:text-align="center"/>"#.to_string()
                }
                Some(OdsHorizontalAlign::Right) => {
                    r#"<style:paragraph-properties fo:text-align="end"/>"#.to_string()
                }
                None => String::new(),
            };
            out.push_str(&format!(
                r#"<style:style style:name="{name}" style:family="table-cell"{data_attr}>{table_props}{text_props}{paragraph_props}</style:style>"#
            ));
        }
        out
    }
}

/// One `table:table` and its `table:column` definitions, rows/cells. `table_name` sets `table:name`
/// (sheet tab in Calc); `None` keeps the legacy single-table export (no `table:name` attribute).
fn ods_table_fragment_tsv_shaped(
    grid: &Grid,
    matrix: &[Vec<String>],
    options: &DelimitedExportOptions,
    col_start: usize,
    data_rows: &[usize],
    column_style_naming: OdsColumnStyleNaming,
    table_name: Option<&str>,
) -> (String, String) {
    let export_content = options.content;
    let generic_tsv_rebase = if export_content == ExportContent::Generic {
        Some(export::delimited_options_generic_rebase(grid, options))
    } else {
        None
    };
    let tc = matrix
        .iter()
        .map(|r| r.len())
        .max()
        .unwrap_or(1)
        .max(1);
    let mut s = String::new();
    let mut styles = OdsStyleRegistry::default();
    let last_display_main_row = data_rows
        .iter()
        .copied()
        .filter(|r| *r >= HEADER_ROWS && *r < HEADER_ROWS + grid.main_rows())
        .max();
    let export_main_has_left = (0..tc).any(|c| {
        let gc = col_start + c.saturating_sub(if options.include_row_label_column { 1 } else { 0 });
        gc == MARGIN_COLS
    });
    let export_has_right_margin = (0..tc).any(|c| {
        let gc = col_start + c.saturating_sub(if options.include_row_label_column { 1 } else { 0 });
        gc == MARGIN_COLS + grid.main_cols()
    });
    match table_name {
        None => s.push_str("<table:table>"),
        Some(n) => {
            let a = ods_xml_attr_escape(n);
            s.push_str(&format!(r#"<table:table table:name="{a}">"#));
        }
    }

    for c in 0..tc {
        let st = column_style_naming.style_name(c);
        s.push_str(&format!(
            r#"<table:table-column table:style-name="{st}"/>"#
        ));
    }

    let rk: usize = if options.include_row_label_column { 1 } else { 0 };
    for (i, row) in matrix.iter().enumerate() {
        s.push_str("<table:table-row>");
        for j in 0..tc {
            let cell_str = row.get(j).map(|s| s.as_str()).unwrap_or("");
            if options.include_header_row && i == 0 {
                s.push_str(&ods_cell_from_export_matrix_string(cell_str, export_content));
            } else if options.include_row_label_column && j < rk {
                s.push_str(&ods_cell_from_export_matrix_string(cell_str, export_content));
            } else {
                let data_i = if options.include_header_row { i - 1 } else { i };
                let logical = data_rows.get(data_i).copied().unwrap_or(HEADER_ROWS);
                let global = col_start + j.saturating_sub(rk);
                let flags = ods_tui_flags(
                    grid,
                    logical,
                    global,
                    last_display_main_row,
                    export_main_has_left,
                    export_has_right_margin,
                );
                s.push_str(&ods_cell_xml(
                    grid,
                    logical,
                    global,
                    export_content,
                    generic_tsv_rebase,
                    &mut styles,
                    flags,
                ));
            }
        }
        s.push_str("</table:table-row>");
    }
    s.push_str("</table:table>");
    (s, styles.xml())
}

fn ods_workbook_content_xml_tsv_shaped(
    workbook: &WorkbookState,
    options: &DelimitedExportOptions,
) -> (String, String) {
    let mut column_styles = String::new();
    let mut cell_styles = String::new();
    let mut tables = String::new();
    let mut blocks: Vec<String> = Vec::new();

    for (si, sheet) in workbook.sheets.iter().enumerate() {
        let g = &sheet.state.grid;
        let (matrix, col_start, col_end, data_rows) = export::delimited_export_matrix(g, options);
        let header_ods_rows = if options.include_header_row { 1 } else { 0 };
        let row_key_cols = if options.include_row_label_column { 1 } else { 0 };
        blocks.push(corro_ods_layout_tsv_parity_block_lines(
            col_start,
            col_end,
            header_ods_rows,
            row_key_cols,
            &data_rows,
            g.main_cols(),
        ));

        let tc = matrix
            .iter()
            .map(|r| r.len())
            .max()
            .unwrap_or(1)
            .max(1);
        let naming = OdsColumnStyleNaming::Sheet(si);
        column_styles.push_str(&ods_column_styles_tsv(
            g,
            tc,
            col_start,
            options.include_row_label_column,
            naming,
        ));
        let title = {
            let t = sheet.title.trim();
            if t.is_empty() {
                format!("Sheet{}", si + 1)
            } else {
                t.to_string()
            }
        };
        let (table, styles) = ods_table_fragment_tsv_shaped(
            g,
            &matrix,
            options,
            col_start,
            &data_rows,
            naming,
            Some(&title),
        );
        tables.push_str(&table);
        cell_styles.push_str(&styles);
    }

    let content = format!(
        "{ODS_CONTENT_XML_PREFIX}{column_styles}{cell_styles}</office:automatic-styles><office:body><office:spreadsheet>{tables}</office:spreadsheet></office:body></office:document-content>"
    );
    (content, corro_ods_layout_v2(&blocks))
}

/// TSV/CSV default layout: one ODF table row per matrix row, same column count as
/// [export::delimited_export_matrix] (synthetic header + row key + grid columns).
fn ods_content_xml_tsv_shaped(
    grid: &Grid,
    matrix: &[Vec<String>],
    options: &DelimitedExportOptions,
    col_start: usize,
    _col_end: usize,
    data_rows: &[usize],
) -> String {
    let tc = matrix
        .iter()
        .map(|r| r.len())
        .max()
        .unwrap_or(1)
        .max(1);
    let column_styles = ods_column_styles_tsv(
        grid,
        tc,
        col_start,
        options.include_row_label_column,
        OdsColumnStyleNaming::Legacy,
    );
    let (table, cell_styles) = ods_table_fragment_tsv_shaped(
        grid,
        matrix,
        options,
        col_start,
        data_rows,
        OdsColumnStyleNaming::Legacy,
        None,
    );
    format!(
        "{ODS_CONTENT_XML_PREFIX}{column_styles}{cell_styles}</office:automatic-styles><office:body><office:spreadsheet>{table}</office:spreadsheet></office:body></office:document-content>"
    )
}

fn ods_cell_from_export_matrix_string(s: &str, export_content: ExportContent) -> String {
    if s.trim().is_empty() {
        return "<table:table-cell/>".into();
    }
    match export_content {
        ExportContent::Values => ods_cell_xml_values_only(s, s, s, None),
        ExportContent::Formulas => {
            if s.trim().starts_with('=') || is_formula(s) {
                let formula = ods_formula_expr(s).unwrap_or_else(|| s.to_string());
                let value_attrs = match s.trim().parse::<f64>() {
                    Ok(n) => format!(r#" office:value-type="float" office:value="{n}""#),
                    Err(_) => r#" office:value-type="string""#.to_string(),
                };
                return format!(
                    r#"<table:table-cell{} table:formula="of:{}"><text:p>{}</text:p></table:table-cell>"#,
                    value_attrs,
                    ods_escape(&formula),
                    ods_escape(s)
                );
            }
            ods_cell_xml_values_only(s, s, s, None)
        }
        ExportContent::Generic => {
            let tsv = s;
            if tsv.trim_start().starts_with('=') {
                let formula = ods_formula_expr(&tsv).unwrap_or_else(|| tsv.to_string());
                let value_attrs = match tsv.trim().parse::<f64>() {
                    Ok(n) => format!(r#" office:value-type="float" office:value="{n}""#),
                    Err(_) => r#" office:value-type="string""#.to_string(),
                };
                return format!(
                    r#"<table:table-cell{} table:formula="of:{}"><text:p>{}</text:p></table:table-cell>"#,
                    value_attrs,
                    ods_escape(&formula),
                    ods_escape(&tsv)
                );
            }
            ods_cell_xml_values_only(&tsv, &tsv, &tsv, None)
        }
    }
}

fn ods_cell_xml(
    grid: &Grid,
    logical_row: usize,
    global_col: usize,
    export_content: ExportContent,
    generic_tsv_rebase: Option<(i32, i32)>,
    styles: &mut OdsStyleRegistry,
    flags: OdsTuiFlags,
) -> String {
    let hr = HEADER_ROWS;
    let mr = grid.main_rows();
    let mc = grid.main_cols();
    let addr = ods_cell_addr(grid, logical_row, global_col);
    let raw = grid.text(&addr);
    let display = export::export_cell_text(
        grid,
        logical_row,
        global_col,
        ExportContent::Values,
        None,
        true,
    );
    let style_attr = ods_cell_style_attr(styles, grid, &addr, &display, flags);

    let value = if logical_row < hr {
        header_formula_or_value(grid, logical_row, global_col, mc)
    } else if logical_row < hr + mr {
        main_formula_or_value(grid, logical_row - hr, global_col, mc)
    } else {
        footer_formula_or_value(grid, logical_row - hr - mr, global_col, mc)
    };

    if export_content == ExportContent::Values {
        if value.is_empty() && raw.is_empty() {
            if let Some(style_attr) = style_attr.as_deref() {
                return format!(r#"<table:table-cell{style_attr}/>"#);
            }
            return "<table:table-cell/>".into();
        }
        return ods_cell_xml_values_only(&display, &value, &raw, style_attr.as_deref());
    }
    // Generic: same interop `=…` as TSV generic after rebase, but ODF `;` list separators (not `,`),
    // or LibreOffice reports Err:508 in `of:` / <text:p>.
    if export_content == ExportContent::Generic {
        let odf = export::export_cell_text(
            grid,
            logical_row,
            global_col,
            ExportContent::Generic,
            generic_tsv_rebase,
            false,
        );
        if odf.trim().is_empty() {
            if let Some(style_attr) = style_attr.as_deref() {
                return format!(r#"<table:table-cell{style_attr}/>"#);
            }
            return "<table:table-cell/>".into();
        }
        if odf.trim_start().starts_with('=') {
            let formula = ods_formula_expr(&odf).unwrap_or_else(|| odf.clone());
            let value_attrs = match display.trim().parse::<f64>() {
                Ok(n) => format!(r#" office:value-type="float" office:value="{n}""#),
                Err(_) => r#" office:value-type="string""#.to_string(),
            };
            return format!(
                r#"<table:table-cell{}{} table:formula="of:{}"><text:p>{}</text:p></table:table-cell>"#,
                style_attr.as_deref().unwrap_or(""),
                value_attrs,
                ods_escape(&formula),
                ods_escape(&odf)
            );
        }
        return ods_cell_xml_values_only(&odf, &odf, &odf, style_attr.as_deref());
    }
    if value.is_empty() && raw.is_empty() {
        if let Some(style_attr) = style_attr.as_deref() {
            return format!(r#"<table:table-cell{style_attr}/>"#);
        }
        return "<table:table-cell/>".into();
    }
    if value.starts_with('=') || is_formula(&raw) {
        let formula = if value.starts_with('=') { value } else { raw };
        let formula = ods_formula_expr(&formula).unwrap_or(formula);
        let value_attrs = match display.trim().parse::<f64>() {
            Ok(n) => format!(r#" office:value-type="float" office:value="{n}""#),
            Err(_) => r#" office:value-type="string""#.to_string(),
        };
        format!(
            r#"<table:table-cell{}{} table:formula="of:{}"><text:p>{}</text:p></table:table-cell>"#,
            style_attr.as_deref().unwrap_or(""),
            value_attrs,
            ods_escape(&formula),
            ods_escape(&display)
        )
    } else {
        format!(
            r#"<table:table-cell{} office:value-type="string"><text:p>{}</text:p></table:table-cell>"#,
            style_attr.as_deref().unwrap_or(""),
            ods_escape(if display.is_empty() { &value } else { &display })
        )
    }
}

/// Static ODF cell: `display` is preferred (evaluated for formulas), then `value` / `raw`.
fn ods_cell_xml_values_only(
    display: &str,
    value: &str,
    raw: &str,
    style_attr: Option<&str>,
) -> String {
    let show = if !display.is_empty() {
        display
    } else if !value.is_empty() {
        value
    } else {
        raw
    };
    if show.trim().is_empty() {
        return "<table:table-cell/>".into();
    }
    if let Ok(n) = show.trim().parse::<f64>() {
        return format!(
            r#"<table:table-cell{} office:value-type="float" office:value="{n}"><text:p>{}</text:p></table:table-cell>"#,
            style_attr.unwrap_or(""),
            ods_escape(show)
        );
    }
    format!(
        r#"<table:table-cell{} office:value-type="string"><text:p>{}</text:p></table:table-cell>"#,
        style_attr.unwrap_or(""),
        ods_escape(show)
    )
}

fn ods_cell_style_attr(
    styles: &mut OdsStyleRegistry,
    grid: &Grid,
    addr: &CellAddr,
    display: &str,
    flags: OdsTuiFlags,
) -> Option<String> {
    let fmt = grid.format_for_addr(addr);
    let number = match fmt.number {
        Some(NumberFormat::DecimalGeneric) => None,
        Some(NumberFormat::Fixed { decimals }) => Some(OdsNumberStyleKey::Fixed { decimals }),
        Some(NumberFormat::Currency { decimals }) => Some(OdsNumberStyleKey::Currency { decimals }),
        Some(NumberFormat::Rational) => None,
        None => None,
    };
    let align = effective_ods_align(fmt, display).or_else(|| ods_default_align_for_addr(addr));
    let key = OdsCellStyleKey {
        number,
        align,
        agg_cyan: flags.agg_cyan,
        footer_bold: flags.footer_bold,
        underlined_boundary_row: flags.underlined_boundary_row,
        left_vertical_divider: flags.left_vertical_divider,
        right_vertical_divider: flags.right_vertical_divider,
    };
    styles
        .cell_style_name(key)
        .map(|name| format!(r#" table:style-name="{name}""#))
}

fn effective_ods_align(fmt: CellFormat, display: &str) -> Option<OdsHorizontalAlign> {
    match fmt.align {
        Some(TextAlign::Left) => Some(OdsHorizontalAlign::Left),
        Some(TextAlign::Center) => Some(OdsHorizontalAlign::Center),
        Some(TextAlign::Right) => Some(OdsHorizontalAlign::Right),
        Some(TextAlign::Default) => None,
        None => {
            if fmt.number.is_some() || display.trim().parse::<f64>().is_ok() {
                Some(OdsHorizontalAlign::Right)
            } else {
                None
            }
        }
    }
}

fn ods_default_align_for_addr(addr: &CellAddr) -> Option<OdsHorizontalAlign> {
    match addr {
        // Match the TUI visual: left-margin labels sit against the divider side.
        CellAddr::Left { .. } => Some(OdsHorizontalAlign::Right),
        _ => None,
    }
}

fn parse_num(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    t.parse::<f64>().ok()
}

fn fold_numbers(func: crate::ops::AggFunc, xs: &[f64]) -> String {
    if xs.is_empty() {
        return String::new();
    }
    match func {
        crate::ops::AggFunc::Sum => format!("{}", xs.iter().sum::<f64>()),
        crate::ops::AggFunc::Mean => format!("{}", xs.iter().sum::<f64>() / xs.len() as f64),
        crate::ops::AggFunc::Median => {
            let mut ys = xs.to_vec();
            ys.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let n = ys.len();
            let m = if n % 2 == 1 {
                ys[n / 2]
            } else {
                (ys[n / 2 - 1] + ys[n / 2]) / 2.0
            };
            format!("{m}")
        }
        crate::ops::AggFunc::Min => xs
            .iter()
            .copied()
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|v| format!("{v}"))
            .unwrap_or_default(),
        crate::ops::AggFunc::Max => xs
            .iter()
            .copied()
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|v| format!("{v}"))
            .unwrap_or_default(),
        crate::ops::AggFunc::Count => format!("{}", xs.len()),
    }
}

fn footer_row_agg_func(grid: &Grid, footer_row_idx: usize) -> Option<crate::ops::AggFunc> {
    let key_col = ColumnAddr::Left(MARGIN_COLS - 1);
    let val = grid.get(&CellAddr::Footer {
        row: footer_row_idx as u32,
        col: key_col,
    })?;
    crate::ops::margin_key_agg_func(&val)
}

fn right_col_agg_func(grid: &Grid, global_col: usize) -> Option<crate::ops::AggFunc> {
    let mut labels: Vec<(u32, String)> = grid
        .iter_nonempty()
        .filter_map(|(addr, val)| match addr {
            CellAddr::Header { row, col } if col.to_global(grid.main_cols()) == global_col => Some((row, val)),
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

fn left_margin_agg_func(grid: &Grid, main_row: u32) -> Option<crate::ops::AggFunc> {
    let key_col = MARGIN_COLS - 1;
    let val = grid.get(&CellAddr::Left {
        col: key_col,
        row: main_row,
    })?;
    crate::ops::margin_key_agg_func(&val)
}

fn data_main_col_count(grid: &Grid) -> usize {
    crate::agg::helpers::data_main_col_count(grid)
}

fn previous_raw_block(grid: &Grid, current_main_row: u32) -> Option<(u32, u32)> {
    crate::agg::helpers::previous_raw_block(grid, current_main_row)
}

fn left_margin_main_col_aggregate(
    grid: &Grid,
    subtotal_func: crate::ops::AggFunc,
    main_row: u32,
    main_col: u32,
) -> String {
    crate::agg::helpers::left_margin_main_col_aggregate(grid, subtotal_func, main_row, main_col)
}

fn left_margin_special_col_aggregate(
    grid: &Grid,
    subtotal_func: crate::ops::AggFunc,
    global_col: usize,
    row_start: u32,
    row_end: u32,
    data_cols: usize,
) -> Option<String> {
    crate::agg::helpers::left_margin_special_col_aggregate(
        grid,
        subtotal_func,
        global_col,
        row_start,
        row_end,
        data_cols,
    )
}

fn footer_special_col_aggregate(
    grid: &Grid,
    footer_func: crate::ops::AggFunc,
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
                &crate::ops::AggregateDef {
                    func,
                    source: crate::grid::MainRange {
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
                    col: global_col - MARGIN_COLS - main_cols,
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

fn ods_tui_flags(
    grid: &Grid,
    logical_row: usize,
    global_col: usize,
    last_display_main_row: Option<usize>,
    export_main_has_left: bool,
    export_has_right_margin: bool,
) -> OdsTuiFlags {
    let hr = HEADER_ROWS;
    let mr = grid.main_rows();
    let lm = MARGIN_COLS;
    let mc = grid.main_cols();
    let is_underlined_boundary_row =
        (hr > 0 && logical_row == hr - 1) || last_display_main_row == Some(logical_row);
    let footer_agg = if logical_row >= hr + mr {
        footer_row_agg_func(grid, logical_row - hr - mr)
    } else {
        None
    };
    let main_row_idx = if logical_row >= hr && logical_row < hr + mr {
        Some((logical_row - hr) as u32)
    } else {
        None
    };
    let left_margin_agg = main_row_idx.and_then(|mri| left_margin_agg_func(grid, mri));
    let left_margin_block_start = main_row_idx.map(|mri| row_total_block_start(grid, mri));
    let right_col_agg = right_col_agg_func(grid, global_col);
    let mut is_agg_cell = false;
    if let Some(func) = footer_agg {
        if right_col_agg.is_some() {
            is_agg_cell = footer_special_col_aggregate(grid, func, global_col, mr, mc).is_some();
        } else if global_col >= lm && global_col < lm + mc {
            is_agg_cell = true;
        }
    } else if let (Some(func), Some(block_start), Some(main_row)) =
        (left_margin_agg, left_margin_block_start, main_row_idx)
    {
        if global_col >= lm && global_col < lm + mc {
            is_agg_cell = true;
            if right_col_agg.is_some() {
                let data_cols = data_main_col_count(grid);
                let (row_start, row_end) = if block_start < main_row {
                    (block_start, main_row)
                } else {
                    previous_raw_block(grid, main_row).unwrap_or((0, main_row))
                };
                let _ = left_margin_special_col_aggregate(
                    grid, func, global_col, row_start, row_end, data_cols,
                );
            } else {
                let main_col = (global_col - lm) as u32;
                let _ = left_margin_main_col_aggregate(grid, func, main_row, main_col);
            }
        } else if right_col_agg.is_some() {
            is_agg_cell = true;
        }
    } else if logical_row >= hr && logical_row < hr + mr && right_col_agg.is_some() {
        is_agg_cell = true;
    }
    OdsTuiFlags {
        agg_cyan: is_agg_cell,
        footer_bold: is_agg_cell && footer_agg.is_some(),
        underlined_boundary_row: is_underlined_boundary_row,
        left_vertical_divider: global_col == lm - 1 && lm > 0 && export_main_has_left,
        right_vertical_divider: global_col == lm + mc - 1 && export_has_right_margin,
    }
}

fn ods_cell_addr(grid: &Grid, logical_row: usize, global_col: usize) -> CellAddr {
    let hr = HEADER_ROWS;
    let mr = grid.main_rows();
    let lm = MARGIN_COLS;
    let mc = grid.main_cols();

    if logical_row < hr {
        CellAddr::Header {
            row: logical_row as u32,
            col: ColumnAddr::from_global(global_col, mc),
        }
    } else if logical_row < hr + mr {
        let main_row = (logical_row - hr) as u32;
        if global_col < lm {
            CellAddr::Left {
                col: global_col,
                row: main_row,
            }
        } else if global_col < lm + mc {
            CellAddr::Main {
                row: main_row,
                col: (global_col - lm) as u32,
            }
        } else {
            CellAddr::Right {
                col: global_col - lm - mc,
                row: main_row,
            }
        }
    } else {
        CellAddr::Footer {
            row: (logical_row - hr - mr) as u32,
            col: ColumnAddr::from_global(global_col, mc),
        }
    }
}

/// Strip a stored cell formula to a single `=…` (no ` -- LABEL`), for ODF or generic export.
pub fn ods_labeled_prefix_strip_to_formula(raw: &str) -> Option<String> {
    let t = raw.trim();
    let expr = t.strip_prefix('=')?;
    let expr = expr
        .split_once(" -- ")
        .map_or(expr, |(expr, _)| expr)
        .trim();
    if expr.is_empty() {
        None
    } else {
        Some(format!("={expr}"))
    }
}

fn ods_formula_expr(raw: &str) -> Option<String> {
    ods_labeled_prefix_strip_to_formula(raw)
}

/// Same `value` string as [`ods_cell_xml`] uses for `table:formula` (SUBTOTAL, raw, header base, …).
pub fn cell_export_value_string(
    grid: &Grid,
    logical_row: usize,
    global_col: usize,
) -> String {
    let hr = HEADER_ROWS;
    let mr = grid.main_rows();
    let mc = grid.main_cols();
    if logical_row < hr {
        header_formula_or_value(grid, logical_row, global_col, mc)
    } else if logical_row < hr + mr {
        main_formula_or_value(grid, logical_row - hr, global_col, mc)
    } else {
        footer_formula_or_value(grid, logical_row - hr - mr, global_col, mc)
    }
}

fn header_formula_or_value(grid: &Grid, row: usize, global_col: usize, main_cols: usize) -> String {
    let base = grid.text(&CellAddr::Header {
        row: row as u32,
        col: ColumnAddr::from_global(global_col, main_cols),
    });
    if global_col < MARGIN_COLS || global_col >= MARGIN_COLS + main_cols {
        return base;
    }
    if let Some(code) = subtotal_code_for_label(&base) {
        let col = excel_column_name(global_col - MARGIN_COLS);
        return format!("=SUBTOTAL({code};{col}1:{col}{})", grid.main_rows());
    }
    base
}

fn main_formula_or_value(
    grid: &Grid,
    main_row: usize,
    global_col: usize,
    main_cols: usize,
) -> String {
    let lm = MARGIN_COLS;
    let mr = grid.main_rows();
    if global_col < lm {
        let c = global_col;
        let raw = grid.text(&CellAddr::Left {
            col: c,
            row: main_row as u32,
        });
        if let Some(code) = subtotal_code_for_label(&raw) {
            let start = row_total_block_start(grid, main_row as u32);
            let col = excel_column_name(0);
            return format!("=SUBTOTAL({code};{col}{}:{col}{})", start + 1, main_row + 1);
        }
        return raw;
    }
    if global_col < lm + main_cols {
        let raw = grid.text(&CellAddr::Main {
            row: main_row as u32,
            col: (global_col - lm) as u32,
        });
        if is_formula(&raw) {
            raw
        } else {
            raw
        }
    } else {
        let rc = global_col - lm - main_cols;
        let raw = grid.text(&CellAddr::Right {
            col: rc,
            row: main_row as u32,
        });
        if let Some(code) = subtotal_code_for_label(&raw) {
            return format!(
                "=SUBTOTAL({code};{}1:{}{})",
                excel_column_name(0),
                excel_column_name(main_cols - 1),
                mr
            );
        }
        raw
    }
}

fn footer_formula_or_value(
    grid: &Grid,
    footer_row: usize,
    global_col: usize,
    main_cols: usize,
) -> String {
    let raw = grid.text(&CellAddr::Footer {
        row: footer_row as u32,
        col: ColumnAddr::from_global(global_col, main_cols),
    });
    if let Some(code) = subtotal_code_for_label(&raw) {
        return format!(
            "=SUBTOTAL({code};{}1:{}{})",
            excel_column_name(0),
            excel_column_name(main_cols - 1),
            grid.main_rows()
        );
    }
    raw
}

pub(crate) fn subtotal_code_for_label(raw: &str) -> Option<u8> {
    use crate::ops::AggFunc;
    match crate::ops::margin_key_agg_func(raw)? {
        AggFunc::Sum => Some(9),
        AggFunc::Mean => Some(1),
        AggFunc::Count => Some(2),
        AggFunc::Max => Some(4),
        AggFunc::Min => Some(5),
        AggFunc::Median => None,
    }
}

fn row_total_block_start(grid: &Grid, current_main_row: u32) -> u32 {
    for candidate in (0..current_main_row).rev() {
        if grid
            .get(&CellAddr::Left {
                col: MARGIN_COLS - 1,
                row: candidate,
            })
            .is_some()
        {
            return candidate + 1;
        }
    }
    0
}

/// Convert ODF OpenFormula (string after the `of:` prefix) to Corro/Excel-style syntax: bracket
/// references `[.A1]`, `[.A1:.B2]` become `A1`, `A1:B2`; list separators `;` become `,`.
fn odf_openformula_to_corro(expr: &str) -> String {
    let mut out = String::with_capacity(expr.len());
    let mut i = 0usize;
    let b = expr.as_bytes();
    while i < b.len() {
        if b[i] == b'[' && b.get(i + 1) == Some(&b'.') {
            if let Some((end, rep)) = try_replace_odf_bracket_ref(expr, i) {
                out.push_str(&rep);
                i = end;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out.replace(';', ",")
}

/// Parse `[.ColRow]` or `[.C1:.C2:...]` *range* (second part is `:` optionally followed by `.`). Returns
/// (byte index after closing `]`, A1 or A1:B2 string).
fn try_replace_odf_bracket_ref(s: &str, start: usize) -> Option<(usize, String)> {
    let b = s.as_bytes();
    if b.get(start) != Some(&b'[') || b.get(start + 1) != Some(&b'.') {
        return None;
    }
    let (first, p1) = parse_odf_col_row_tokens(s, start + 2)?;
    if p1 < b.len() && b[p1] == b':' {
        let mut p2 = p1 + 1;
        if b.get(p2) == Some(&b'.') {
            p2 += 1;
        }
        let (second, p3) = parse_odf_col_row_tokens(s, p2)?;
        if p3 < b.len() && b[p3] == b']' {
            return Some((p3 + 1, format!("{first}:{second}")));
        }
        return None;
    }
    if p1 < b.len() && b[p1] == b']' {
        return Some((p1 + 1, first));
    }
    None
}

/// One ODF A1 token inside a bracket ref (optional `$` before col/row, letters + digits). Returns
/// `C3` and index after the row digits.
fn parse_odf_col_row_tokens(s: &str, start: usize) -> Option<(String, usize)> {
    let b = s.as_bytes();
    let mut i = start;
    if b.get(i) == Some(&b'$') {
        i += 1;
    }
    let col0 = i;
    while i < b.len() && b[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i == col0 {
        return None;
    }
    let col = s[col0..i].to_ascii_uppercase();
    if b.get(i) == Some(&b'$') {
        i += 1;
    }
    let r0 = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == r0 {
        return None;
    }
    let row = &s[r0..i];
    Some((format!("{col}{row}"), i))
}

fn ods_openformula_body_to_cell_text(formula: &str) -> String {
    // `table:formula` is `of:...`; OpenDocument uses `of:=` when the body includes `=`.
    let t = formula.strip_prefix("of:").unwrap_or(formula);
    let t = t.strip_prefix('=').unwrap_or(t);
    odf_openformula_to_corro(t)
}

fn ods_layout_for_table_index<'a>(
    layouts: &'a [OdsTableLayout],
    table_index: usize,
    default: &'a OdsTableLayout,
) -> &'a OdsTableLayout {
    if layouts.is_empty() {
        return default;
    }
    if layouts.len() == 1 {
        return &layouts[0];
    }
    layouts.get(table_index).unwrap_or(default)
}

fn parse_ods_content_with_layout(
    xml: &str,
    layouts: &[OdsTableLayout],
) -> Result<WorkbookState, OdsError> {
    let default_layout = OdsTableLayout::Odf;
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut workbook = WorkbookState::new();
    workbook.sheets.clear();
    // `new()` left `next_sheet_id` at 2; the first imported sheet would incorrectly get id 2.
    workbook.next_sheet_id = 1;
    let mut current_sheet: Option<SheetRecord> = None;
    let mut open_table_i: Option<usize> = None;
    let mut next_table: usize = 0;
    let mut odf_uses_global_logical: Option<bool> = None;
    let mut row_idx = 0usize;
    let mut col_idx = 0usize;
    let mut pending_value = String::new();
    let mut pending_formula: Option<String> = None;
    let mut in_p = false;
    let mut in_cell = false;
    let mut current_row_repeat = 1usize;
    let mut table_cell_num_cols = 1usize;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"table:table" => {
                    let title = attr_value(&e, b"table:name").unwrap_or_else(|| "Sheet1".into());
                    let ti = next_table;
                    next_table += 1;
                    open_table_i = Some(ti);
                    current_sheet = Some(SheetRecord {
                        id: workbook.next_sheet_id,
                        title,
                        state: SheetState::new(1, 1),
                        linked_source: None,
                    });
                    workbook.next_sheet_id += 1;
                    row_idx = 0;
                    col_idx = 0;
                    odf_uses_global_logical = None;
                    in_cell = false;
                    in_p = false;
                    pending_value.clear();
                    pending_formula = None;
                    table_cell_num_cols = 1;
                }
                b"table:table-row" => {
                    col_idx = 0;
                    in_cell = false;
                    in_p = false;
                    current_row_repeat = attr_value(&e, b"table:number-rows-repeated")
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(1);
                    if odf_uses_global_logical.is_none() {
                        odf_uses_global_logical = Some(
                            (current_row_repeat as u64) >= ODS_GLOBAL_LAYOUT_MIN_FIRST_ROW_REPEAT,
                        );
                    }
                }
                b"text:p" => {
                    in_p = true;
                    pending_value.clear();
                }
                b"table:table-cell" => {
                    in_cell = true;
                    pending_value.clear();
                    pending_formula = attr_value(&e, b"table:formula");
                    table_cell_num_cols = attr_value(&e, b"table:number-columns-repeated")
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(1)
                        .max(1);
                }
                _ => {}
            },
            Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"table:table-cell" => {
                    // Only ODF `table:table-cell` inside a `table:table` maps to a sheet. Advancing
                    // `col_idx` with no `current_sheet` (e.g. interleaved or malformed XML) would
                    // desync columns and shift TsvParity re-import.
                    if let Some(sheet) = current_sheet.as_mut() {
                        let n = attr_value(&e, b"table:number-columns-repeated")
                            .and_then(|n| n.parse().ok())
                            .unwrap_or(1)
                            .max(1);
                        let empty_formula = attr_value(&e, b"table:formula");
                        let odf_full = odf_uses_global_logical == Some(true);
                        let tidx = open_table_i.unwrap_or(0);
                        let table_layout = ods_layout_for_table_index(layouts, tidx, &default_layout);
                        for c_off in 0..n {
                            let c = col_idx + c_off;
                            if let Some((lr, gc, g)) =
                                table_layout.map_ods_table_cell(row_idx, c, odf_full)
                            {
                                apply_ods_table_cell(
                                    &mut sheet.state,
                                    table_layout,
                                    lr,
                                    gc,
                                    empty_formula.as_deref(),
                                    "",
                                    g,
                                );
                            }
                        }
                        col_idx += n;
                    }
                }
                _ => {}
            },
            Ok(Event::Text(t)) => {
                if in_p || in_cell {
                    pending_value.push_str(&String::from_utf8_lossy(t.as_ref()));
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"text:p" => in_p = false,
                b"table:table-cell" => {
                    if let Some(sheet) = current_sheet.as_mut() {
                        let n = table_cell_num_cols;
                        let odf_full = odf_uses_global_logical == Some(true);
                        let tidx = open_table_i.unwrap_or(0);
                        let table_layout = ods_layout_for_table_index(layouts, tidx, &default_layout);
                        for c_off in 0..n {
                            let c = col_idx + c_off;
                            if let Some((lr, gc, g)) =
                                table_layout.map_ods_table_cell(row_idx, c, odf_full)
                            {
                                apply_ods_table_cell(
                                    &mut sheet.state,
                                    table_layout,
                                    lr,
                                    gc,
                                    pending_formula.as_deref(),
                                    &pending_value,
                                    g,
                                );
                            }
                        }
                        col_idx += n;
                    }
                    in_cell = false;
                }
                b"table:table-row" => row_idx += current_row_repeat,
                b"table:table" => {
                    if let Some(sheet) = current_sheet.take() {
                        workbook.sheets.push(sheet);
                    }
                    open_table_i = None;
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => return Err(OdsError::Xml(e.to_string())),
            _ => {}
        }
        buf.clear();
    }

    if workbook.sheets.is_empty() {
        return Err(OdsError::Xml("no sheets found".into()));
    }
    let active_sheet_id = workbook
        .sheets
        .first()
        .map(|s| s.id)
        .unwrap_or(1);
    let snapshot = WorkbookSnapshot {
        next_sheet_id: workbook.next_sheet_id,
        active_sheet_id,
        sheets: workbook.sheets,
        volatile_seed: 0,
    };
    Ok(WorkbookState::from_snapshot(&snapshot))
}

/// Map ODF (logical row, global col) into the five regions using [SheetState] extents.
fn full_logical_addr(state: &SheetState, row: usize, col: usize) -> CellAddr {
    if row < HEADER_ROWS {
        CellAddr::Header {
            row: row as u32,
            col: ColumnAddr::from_global(col, state.grid.main_cols()),
        }
    } else if row < HEADER_ROWS + state.grid.main_rows() {
        let mr = row - HEADER_ROWS;
        if col < MARGIN_COLS {
            CellAddr::Left {
                col: MARGIN_COLS - 1 - col,
                row: mr as u32,
            }
        } else if col < MARGIN_COLS + state.grid.main_cols() {
            CellAddr::Main {
                row: mr as u32,
                col: (col - MARGIN_COLS) as u32,
            }
        } else {
            CellAddr::Right {
                col: col - MARGIN_COLS - state.grid.main_cols(),
                row: mr as u32,
            }
        }
    } else {
        let fr = row - HEADER_ROWS - state.grid.main_rows();
        CellAddr::Footer {
            row: fr as u32,
            col: ColumnAddr::from_global(col, state.grid.main_cols()),
        }
    }
}

fn set_ods_cell_full_logical(
    state: &mut SheetState,
    row: usize,
    col: usize,
    formula: Option<&str>,
    value: &str,
) {
    place_full_logical_cell(state, row, col, formula, value, None);
}

/// `tsv_ods_deltas` = the `(d_row, d_col)` from [`crate::export::delimited_layout_generic_rebase`]
/// (same as export’s generic TSV/ODS interop rebase). The ODF `of:` text is in “file A1” space,
/// so we negate to restore Corro’s grid A1 in [`set_ods_cell_tsv_parity`].
fn place_full_logical_cell(
    state: &mut SheetState,
    row: usize,
    col: usize,
    formula: Option<&str>,
    value: &str,
    tsv_ods_deltas: Option<(i32, i32)>,
) {
    let target = full_logical_addr(state, row, col);
    if let Some(f) = formula {
        let value_trim = value.trim();
        if value_trim.starts_with('=') && crate::formula::split_labeled_formula(value_trim).is_some() {
            let cell = if let Some((dr, dc)) = tsv_ods_deltas {
                crate::formula::rebase_interop_formula_row_col(value_trim, -dr, -dc, state.grid.main_cols())
            } else {
                value_trim.to_string()
            };
            state.grid.set(&target, cell);
            return;
        }
        let body = ods_openformula_body_to_cell_text(f);
        let cell = if let Some((dr, dc)) = tsv_ods_deltas {
            crate::formula::rebase_interop_formula_row_col(&format!("={body}"), -dr, -dc, state.grid.main_cols())
        } else {
            format!("={body}")
        };
        state.grid.set(&target, cell);
    } else {
        state.grid.set(&target, normalize_ods_import_plain_cell(value));
    }
}

/// Re-import a cell from a Corro TSV-mapped ODF table. [`set_ods_cell_full_logical`] classifies
/// "main" vs "right" using the grid's `main_rows` / `main_cols`, which is still 1×1 for the first
/// few cells. Grow to the exported extents first so B and the second data row are not sent to
/// the right margin or to the footer.
fn set_ods_cell_tsv_parity(
    state: &mut SheetState,
    lr: usize,
    gc: usize,
    export_main_cols: Option<usize>,
    interop_d_row: i32,
    interop_d_col: i32,
    formula: Option<&str>,
    value: &str,
) {
    if value.is_empty() && formula.is_none() {
        return;
    }
    let need_for_gc = if gc < MARGIN_COLS {
        1
    } else {
        (gc - MARGIN_COLS + 1).max(1)
    };
    let main_cols = state
        .grid
        .main_cols()
        .max(export_main_cols.unwrap_or(0))
        .max(need_for_gc);
    if lr >= HEADER_ROWS {
        let mri = lr - HEADER_ROWS;
        let row_need = mri + 1;
        // Never shrink: `set_main_size` truncates to smaller main_cols and can wipe the entire
        // first main row (B..) while processing A of row 2, if we used only (gc - M + 1) for A.
        state.grid.set_main_size(
            state.grid.main_rows().max(row_need),
            main_cols,
        );
    } else {
        // For header rows, grow main_cols so that `from_global` used
        // by `place_full_logical_cell` produces the correct ColumnAddr
        // variant (Main instead of Right) for header cells.
        state.grid.set_main_size(
            state.grid.main_rows(),
            main_cols,
        );
    }
    let deltas = if interop_d_row != 0 || interop_d_col != 0 {
        Some((interop_d_row, interop_d_col))
    } else {
        None
    };
    place_full_logical_cell(state, lr, gc, formula, value, deltas);
}

fn apply_ods_table_cell(
    state: &mut SheetState,
    table_layout: &OdsTableLayout,
    lr: usize,
    gc: usize,
    formula: Option<&str>,
    value: &str,
    odf_uses_global_logical: bool,
) {
    match table_layout {
        OdsTableLayout::TsvParity {
            col_start,
            col_end,
            data_logical_rows,
            header_ods_rows,
            row_key_cols,
            export_main_cols,
        } => {
            let (dr, dc) = export::delimited_layout_generic_rebase(
                *col_start,
                *col_end,
                *header_ods_rows > 0,
                *row_key_cols > 0,
                data_logical_rows,
            );
            set_ods_cell_tsv_parity(
                state,
                lr,
                gc,
                *export_main_cols,
                dr,
                dc,
                formula,
                value,
            );
        }
        _ => {
            set_ods_cell(
                state,
                lr,
                gc,
                formula,
                value,
                odf_uses_global_logical,
            );
        }
    }
}

/// Map plain ODF cell text to Corro storage: decimal literals become exact [`Number`] literals.
fn normalize_ods_import_plain_cell(value: &str) -> String {
    if let Some(n) = crate::formula::parse_number_literal(value.trim()) {
        n.to_formula_string()
    } else {
        value.to_string()
    }
}

fn set_ods_cell(
    state: &mut SheetState,
    odf_table_row: usize,
    odf_table_col: usize,
    formula: Option<&str>,
    value: &str,
    odf_uses_global_logical: bool,
) {
    if value.is_empty() && formula.is_none() {
        return;
    }
    // Interop: ODF/Calc tables are a plain rectangle — map directly to the main grid. (Do not add
    // HEADER_ROWS / MARGIN_COLS; the old `row < … + main_rows()` check misclassified ODF row 2+
    // as footer while `main_rows` was still 1.)
    if !odf_uses_global_logical {
        let target = CellAddr::Main {
            row: odf_table_row as u32,
            col: odf_table_col as u32,
        };
        if let Some(f) = formula {
            let body = ods_openformula_body_to_cell_text(f);
            state.grid.set(&target, format!("={body}"));
        } else {
            state.grid.set(&target, normalize_ods_import_plain_cell(value));
        }
        return;
    }

    set_ods_cell_full_logical(state, odf_table_row, odf_table_col, formula, value);
}

fn attr_value(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    for a in e.attributes().flatten() {
        if a.key.as_ref() == key {
            return Some(String::from_utf8_lossy(a.value.as_ref()).into_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export;
    use crate::export::DelimitedExportOptions;
    use std::io::{ErrorKind, Read};
    use std::process::Command;
    use tempfile::tempdir;
    use tempfile::NamedTempFile;

    #[test]
    fn export_writes_ods_zip() {
        let mut grid = crate::grid::Grid::new(2, 2);
        grid.set(&CellAddr::Main { row: 0, col: 0 }, "1".into());
        let gb = crate::grid::GridBox::from(grid);
        let bytes = export_ods_bytes(&gb).unwrap();
        assert!(bytes.starts_with(b"PK"));
    }

    /// ODS “generic” &lt;text:p&gt; uses ODF `;` list args; TSV generic = Excel `,` =
    /// [export::interop_excel_list_separators] applied to the ODF string.
    #[test]
    fn ods_generic_text_p_matches_tsv_cell_text() {
        let mut grid = crate::grid::Grid::new(2, 2);
        grid.set(&CellAddr::Main { row: 0, col: 0 }, "=2+2".into());
        let gb = crate::grid::GridBox::from(grid);
        let rebase = export::delimited_default_generic_rebase(&gb);
        let expected_ods = export::export_cell_text(
            &gb,
            HEADER_ROWS,
            MARGIN_COLS,
            ExportContent::Generic,
            Some(rebase),
            false,
        );
        let tsv_style = export::export_cell_text(
            &gb,
            HEADER_ROWS,
            MARGIN_COLS,
            ExportContent::Generic,
            Some(rebase),
            true,
        );
        assert_eq!(tsv_style, export::interop_excel_list_separators(&expected_ods));
        assert!(
            expected_ods.contains("2+2") || expected_ods.trim_start().starts_with('='),
            "unexpected generic cell {expected_ods:?}"
        );
        let content_xml = {
            let opts = DelimitedExportOptions {
                content: ExportContent::Generic,
                ..Default::default()
            };
            let bytes = export_ods_bytes_with_options(&gb, &opts).unwrap();
            let mut a = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
            let mut s = String::new();
            a.by_name("content.xml")
                .unwrap()
                .read_to_string(&mut s)
                .unwrap();
            s
        };
        assert!(
            content_xml.contains(&format!("<text:p>{}</text:p>", ods_escape(&expected_ods))),
            "ODS <text:p> should equal ODF-style generic for that cell; want {:?}\n",
            expected_ods
        );
    }

    #[test]
    fn odf_openformula_to_corro_rewrites_lo_refs_and_semicolons() {
        assert_eq!(&odf_openformula_to_corro("([.C3]*0.1)"), "(C3*0.1)");
        assert_eq!(&odf_openformula_to_corro("SUM([.C3:.E3])"), "SUM(C3:E3)");
        assert_eq!(
            &odf_openformula_to_corro("MAX([.C3:.C7];[.C9:.C11])"),
            "MAX(C3:C7,C9:C11)"
        );
    }

    #[test]
    fn ods_openformula_body_strips_of_prefix_and_leading_eq() {
        assert_eq!(&ods_openformula_body_to_cell_text("of:=([.C3]*0.1)"), "(C3*0.1)");
        assert_eq!(&ods_openformula_body_to_cell_text("of:SUM([.A1:.B2])"), "SUM(A1:B2)");
    }

    #[test]
    fn import_subtotal_lo_ods_succeeds() {
        use crate::formula::cell_effective_display;
        use crate::grid::CellAddr;

        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("subtotal_lo.ods");
        if !p.is_file() {
            return;
        }
        let w = import_ods_workbook(&p).expect("import subtotal_lo.ods");
        // First data row, TAX (col D) had `of:=([.C3]*0.1)` in LibreOffice export — should not
        // #PARSE as Corro/Excel.
        let g = &w.active_sheet().grid;
        let disp = cell_effective_display(g, &CellAddr::Main { row: 2, col: 3 });
        assert!(!disp.contains("PARSE"), "unexpected {disp}");
    }

    #[test]
    fn import_ods_roundtrip_basic_sheet() {
        let mut grid = crate::grid::Grid::new(2, 2);
        grid.set(&CellAddr::Main { row: 0, col: 0 }, "42".into());
        let gb = crate::grid::GridBox::from(grid);
        let bytes = export_ods_bytes(&gb).unwrap();
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), bytes).unwrap();
        let workbook = import_ods_workbook(tmp.path()).unwrap();
        assert_eq!(
            workbook
                .active_sheet()
                .grid
                .get(&CellAddr::Main { row: 0, col: 0 })
                .as_deref(),
            Some("42")
        );
    }

    #[test]
    fn export_workbook_two_sheets_roundtrips() {
        use crate::ops::SheetState;
        use crate::ops::WorkbookState;
        use tempfile::NamedTempFile;

        let mut wb = WorkbookState::new();
        wb.sheets[0]
            .state
            .grid
            .set(&CellAddr::Main { row: 0, col: 0 }, "alpha".into());
        let mut s2 = SheetState::new(1, 1);
        s2.grid
            .set(&CellAddr::Main { row: 0, col: 0 }, "beta".into());
        wb.add_sheet("Second".into(), s2);
        let opts = DelimitedExportOptions::default();
        let bytes = export_ods_bytes_workbook_with_options(&wb, &opts).unwrap();
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &bytes).unwrap();
        let back = import_ods_workbook(tmp.path()).unwrap();
        assert_eq!(back.sheet_count(), 2);
        assert_eq!(back.sheets[0].title, "Sheet1");
        assert_eq!(back.sheets[1].title, "Second");
        assert_eq!(
            back.sheets[0]
                .state
                .grid
                .get(&CellAddr::Main { row: 0, col: 0 })
                .as_deref(),
            Some("alpha")
        );
        assert_eq!(
            back.sheets[1]
                .state
                .grid
                .get(&CellAddr::Main { row: 0, col: 0 })
                .as_deref(),
            Some("beta")
        );
    }

    /// LibreOffice/Calc: first `table:table-row` has a small (or no) `number-rows-repeated`; ODF
    /// 0,0 should become corro main (0,0), not a `~N` header row.
    #[test]
    fn import_interop_ods_first_cell_goes_to_main() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" office:version="1.2">
<office:body><office:spreadsheet>
<table:table table:name="Sheet1">
<table:table-row>
<table:table-cell><text:p>hello</text:p></table:table-cell>
</table:table-row>
</table:table>
</office:spreadsheet></office:body>
</office:document-content>
"#;
        let odf = [OdsTableLayout::Odf];
        let workbook = parse_ods_content_with_layout(xml, &odf).unwrap();
        assert_eq!(
            workbook
                .active_sheet()
                .grid
                .get(&CellAddr::Main { row: 0, col: 0 })
                .as_deref(),
            Some("hello")
        );
    }

    /// ODF row 1+ must stay in the main grid, and `set_main_size` must not use `total_cols` (margins + main).
    #[test]
    fn import_interop_ods_row2_and_few_main_cols() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" office:version="1.2">
<office:body><office:spreadsheet>
<table:table table:name="S">
<table:table-row><table:table-cell><text:p>a1</text:p></table:table-cell><table:table-cell><text:p>b1</text:p></table:table-cell></table:table-row>
<table:table-row><table:table-cell><text:p>a2</text:p></table:table-cell><table:table-cell><text:p>b2</text:p></table:table-cell></table:table-row>
</table:table>
</office:spreadsheet></office:body>
</office:document-content>
"#;
        let odf = [OdsTableLayout::Odf];
        let workbook = parse_ods_content_with_layout(xml, &odf).unwrap();
        let s = workbook.active_sheet();
        assert_eq!(
            s.grid.get(&CellAddr::Main { row: 1, col: 1 }).as_deref(),
            Some("b2")
        );
        assert_eq!(s.grid.main_cols(), 2);
        assert!(s.grid.get(&CellAddr::Footer { row: 0, col: ColumnAddr::Main(0) }).is_none());
    }

    #[test]
    fn export_trims_trailing_blank_rows_and_columns() {
        let mut grid = crate::grid::Grid::new(2, 2);
        grid.set(&CellAddr::Main { row: 0, col: 0 }, "42".into());
        let gb = crate::grid::GridBox::from(grid);
        let opts = export::DelimitedExportOptions::default();
        let (matrix, _cs, _ce, _dr) = export::delimited_export_matrix(&gb, &opts);
        let content = exported_content_xml(&gb);
        assert_eq!(content.matches("<table:table-row>").count(), matrix.len());
        assert!(
            !content.contains(&format!(
                r#"<table:table-row table:number-rows-repeated="{}">"#,
                HEADER_ROWS
            )),
            "TSV-shaped ODF does not use a gap filler row; huge logical gaps are not in the table"
        );
        let bytes = export_ods_bytes(&gb).unwrap();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut sidecar = String::new();
        archive
            .by_name(CORRO_ODS_LAYOUT_PATH)
            .unwrap()
            .read_to_string(&mut sidecar)
            .unwrap();
        assert!(sidecar.contains("tsv"));
        let tc = matrix.first().map(|r| r.len()).unwrap_or(0);
        assert_eq!(content.matches("<table:table-column").count(), tc);
    }

    /// Generic ODS `of:` matches TSV generic for a *stored* `=SUBTOTAL(4;…)` (bare `MAX` is plain text, no
    /// `of:` — see [export::generic_interop_cell_text]).
    #[test]
    fn export_generic_ods_formula_attribute_matches_tsv_generic_interop() {
        use crate::grid::HEADER_ROWS;

        let m = MARGIN_COLS;
let mut grid = crate::grid::Grid::new(1, 2);
grid.set(
            &CellAddr::Footer {
                row: 0,
                col: ColumnAddr::Left(1),
            },
            "=TOTAL".into(),
        );
        grid.set_column_format(
            crate::grid::FormatScope::Data,
            (crate::grid::MARGIN_COLS - 1) as usize,
            CellFormat {
                number: Option::None,
                align: None,
            },
        );
        grid.set(&CellAddr::Main { row: 0, col: 0 }, "3".into());
        grid.set(
            &CellAddr::Right { col: 0, row: 0 },
            "=SUBTOTAL(4;A1:B1)".into(),
        );
        let gb = crate::grid::GridBox::from(grid);
        let re = export::delimited_default_generic_rebase(&gb);
        let odf = export::export_cell_text(
            &gb,
            HEADER_ROWS,
            m + 2,
            ExportContent::Generic,
            Some(re),
            false,
        );
        let tsv = export::export_cell_text(
            &gb,
            HEADER_ROWS,
            m + 2,
            ExportContent::Generic,
            Some(re),
            true,
        );
        assert_eq!(tsv, export::interop_excel_list_separators(&odf));
        let formula = ods_formula_expr(&odf).expect("formula");
        assert!(odf.trim_start().starts_with('='), "{odf}");
        let opts = DelimitedExportOptions {
            content: ExportContent::Generic,
            ..Default::default()
        };
        let bytes = export_ods_bytes_with_options(&gb, &opts).unwrap();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut content = String::new();
        archive
            .by_name("content.xml")
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        let expected_in_xml = format!(r#"table:formula="of:{}"#, ods_escape(&formula));
        assert!(
            content.contains(&expected_in_xml),
            "expected {} in ODS, got (fragment): {}",
            expected_in_xml,
            if content.contains("table:formula") {
                "has table:formula not matching"
            } else {
                "no table:formula"
            }
        );
    }

    // ExportContent::Formulas: native ODF SUBTOTAL for aggregate margin labels.
    #[test]
    fn export_converts_total_to_subtotal_formula() {
        let mut grid = crate::grid::Grid::new(1, 1);
        grid.set(
            &CellAddr::Header {
                row: 0,
                col: ColumnAddr::Main(0),
            },
            "=TOTAL".into(),
        );
        let gb = crate::grid::GridBox::from(grid);
        let opts = DelimitedExportOptions {
            content: ExportContent::Formulas,
            ..Default::default()
        };
        let content = exported_content_xml_with_options(&gb, &opts);
        assert!(content.contains(r#"table:formula="of:=SUBTOTAL(9;A1:A1)""#));
    }

    #[test]
    fn export_translates_other_aggregate_labels() {
        let mut grid = crate::grid::Grid::new(1, 1);
        grid.set(
&CellAddr::Footer {
                row: 0,
                col: ColumnAddr::Main(0),
            },
            "MAX".into(),
        );
        let gb = crate::grid::GridBox::from(grid);
        let opts = DelimitedExportOptions {
            content: ExportContent::Formulas,
            ..Default::default()
        };
        let content = exported_content_xml_with_options(&gb, &opts);
        assert!(
            content.contains(r#"table:formula="of:=SUBTOTAL(4;A1:A1)""#),
            "{}",
            content
        );
    }

    #[test]
    fn export_emits_column_styles() {
        let mut grid = crate::grid::Grid::new(2, 2);
        grid.set(&CellAddr::Main { row: 0, col: 0 }, "42".into());
        grid.set(&CellAddr::Left { row: 0, col: 0 }, "X".into());
        let gb = crate::grid::GridBox::from(grid);
        let content = exported_content_xml(&gb);
        assert!(content.contains(r#"style:style style:name="co0" style:family="table-column""#));
        assert!(content.contains(r#"style:column-width=""#));
        assert!(content.contains(r#"table:style-name="co0""#));
    }

    #[test]
    fn export_values_uses_tsv_rendered_display_for_aggregates() {
        let mut grid = crate::grid::Grid::new(2, 1);
        grid.set(&CellAddr::Main { row: 0, col: 0 }, "2".into());
        grid.set(&CellAddr::Main { row: 1, col: 0 }, "3".into());
        grid.set(
            &CellAddr::Footer {
                row: 0,
                col: ColumnAddr::Left(MARGIN_COLS - 1),
            },
            "=TOTAL".into(),
        );
        let gb = crate::grid::GridBox::from(grid);
        let opts = DelimitedExportOptions {
            content: ExportContent::Values,
            ..Default::default()
        };
        let content = exported_content_xml_with_options(&gb, &opts);
        assert!(
            content.contains("<text:p>5</text:p>"),
            "footer aggregate should be exported as rendered value, got {content}"
        );
    }

    #[test]
    fn export_emits_cell_number_and_tui_decoration_styles() {
        let mut grid = crate::grid::Grid::new(2, 1);
        grid.set(&CellAddr::Main { row: 0, col: 0 }, "2".into());
        grid.set(&CellAddr::Main { row: 1, col: 0 }, "3".into());
        grid.set(
            &CellAddr::Footer {
                row: 0,
                col: ColumnAddr::Left(MARGIN_COLS - 1),
            },
            "=TOTAL".into(),
        );
        grid.set_column_format(
            crate::grid::FormatScope::Data,
            MARGIN_COLS,
            crate::grid::CellFormat {
                number: Some(crate::grid::NumberFormat::Currency { decimals: 2 }),
                align: Some(crate::grid::TextAlign::Right),
            },
        );
        let gb = crate::grid::GridBox::from(grid);
        let opts = DelimitedExportOptions {
            content: ExportContent::Values,
            ..Default::default()
        };
        let content = exported_content_xml_with_options(&gb, &opts);
        assert!(
            content.contains("number:currency-style"),
            "expected currency data style in content.xml"
        );
        assert!(
            content.contains("style:data-style-name"),
            "expected cell style to reference a data style"
        );
        assert!(
            content.contains(r##"fo:color="#00bcd4""##),
            "expected aggregate cyan text style"
        );
        assert!(
            content.contains(r#"fo:font-weight="bold""#),
            "expected footer aggregate bold style"
        );
        assert!(
            content.contains(r#"fo:border-bottom="0.018cm solid #6b7280""#),
            "expected boundary underline border style"
        );
        assert!(
            content.contains(r#"fo:border-right="0.018cm solid #6b7280""#),
            "expected vertical divider border style"
        );
    }

    #[test]
    fn subtotal_fixture_roundtrips_through_ods() {
        let workbook = workbook_from_fixture("subtotal.corro");
        let grid = &workbook.active_sheet().grid;
        let content = exported_content_xml(grid);
        assert!(
            content.contains(r#"table:formula="of:="#) && content.contains("SUBTOTAL")
                || content.contains("of:=SUM(")
                || content.contains("of:=MAX(")
                || content.contains("of:=MIN(")
                || content.contains("of:=AVERAGE(")
                || content.contains("of:=COUNT("),
            "default (generic) ODS should write interop `of:` formulas; (fragment): {}",
            &content[..content.len().min(2000)]
        );

        let bytes = export_ods_bytes(grid).unwrap();
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), bytes).unwrap();
        let reimported = import_ods_workbook(tmp.path()).unwrap();

        let sheet = reimported.active_sheet();
        let mut saw_formula = false;
        let mut saw_interop_fn = false;
        scan_grid(&sheet.grid, |value| {
            if value.starts_with('=') {
                saw_formula = true;
            }
            if value.contains("SUBTOTAL(")
                || value.contains("SUM(")
                || value.contains("MAX(")
                || value.contains("MIN(")
            {
                saw_interop_fn = true;
            }
        });
        assert!(
            saw_formula,
            "expected at least one formula cell to survive roundtrip"
        );
        assert!(
            saw_interop_fn,
            "expected spreadsheet-style function names in reimported cells"
        );
    }

    #[test]
    fn subtotal_fixture_opens_in_libreoffice_if_available() {
        let Some(soffice) = libreoffice_binary() else {
            return;
        };

        let workbook = workbook_from_fixture("subtotal.corro");
        let grid = &workbook.active_sheet().grid;
        let dir = tempdir().unwrap();
        let ods_path = dir.path().join("subtotal.ods");
        std::fs::write(&ods_path, export_ods_bytes(grid).unwrap()).unwrap();
        let out_dir = dir.path().join("out");
        std::fs::create_dir(&out_dir).unwrap();

        let status = match Command::new(&soffice)
            .args([
                "--headless",
                "--nologo",
                "--nolockcheck",
                "--nodefault",
                "--convert-to",
                "xlsx",
                "--outdir",
                out_dir.to_str().unwrap(),
                ods_path.to_str().unwrap(),
            ])
            .status()
        {
            Ok(status) => status,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                eprintln!("LibreOffice not found; skipping ODS smoke test");
                return;
            }
            Err(err) => panic!("failed to run LibreOffice: {err}"),
        };
        assert!(status.success(), "LibreOffice conversion failed");

        let xlsx = out_dir.join("subtotal.xlsx");
        assert!(xlsx.exists(), "LibreOffice did not write xlsx output");
        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(std::fs::read(&xlsx).unwrap())).unwrap();
        let mut sheet_xml = String::new();
        archive
            .by_name("xl/worksheets/sheet1.xml")
            .unwrap()
            .read_to_string(&mut sheet_xml)
            .unwrap();
        assert!(
            sheet_xml.contains("SUBTOTAL")
                || sheet_xml.contains("SUM")
                || sheet_xml.contains("MAX")
                || sheet_xml.contains("MIN")
        );
        assert!(sheet_xml.contains("<f"));
    }

    fn workbook_from_fixture(name: &str) -> WorkbookState {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
        let data = std::fs::read_to_string(path).unwrap();
        let mut workbook = WorkbookState::new();
        let mut active_sheet = workbook.sheet_id(workbook.active_sheet);
        for line in data.lines() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            crate::ops::apply_log_line_to_workbook(t, &mut workbook, &mut active_sheet).unwrap();
        }
        workbook
    }

    fn scan_grid<F: FnMut(&str)>(grid: &Grid, mut f: F) {
        for (_, value) in grid.iter_nonempty() {
            if !value.is_empty() {
                f(&value);
            }
        }
    }

    fn libreoffice_binary() -> Option<String> {
        for name in ["soffice", "libreoffice"] {
            let Ok(output) = Command::new(name).arg("--version").output() else {
                continue;
            };
            if output.status.success() {
                return Some(name.to_string());
            }
        }
        None
    }

    fn exported_content_xml(grid: &Grid) -> String {
        let opts = DelimitedExportOptions {
            content: ExportContent::Generic,
            ..Default::default()
        };
        exported_content_xml_with_options(grid, &opts)
    }

    fn exported_content_xml_with_options(grid: &Grid, options: &DelimitedExportOptions) -> String {
        let bytes = export_ods_bytes_with_options(grid, options).unwrap();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut s = String::new();
        archive
            .by_name("content.xml")
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        s
    }

    /// TSV-mapped re-import: the 4th ODF `table:table-cell` in a data row must map to
    /// `col_start + 2` global (with `row_key_cols == 1`), e.g. 701+2 = 703 for the TAX label.
    #[test]
    fn import_tsv_parity_data_row_taxes_ods_col_maps_to_703() {
        let data_lr = 999_999_998usize;
        let layouts = vec![OdsTableLayout::TsvParity {
            col_start: 701,
            col_end: 706,
            data_logical_rows: vec![data_lr],
            header_ods_rows: 1,
            row_key_cols: 1,
            export_main_cols: Some(2),
        }];
        // Same 6 cells as a generic subtotal TAX data row (export uses self-closing empties + formula cell).
        let tax_row = r#"<table:table-cell office:value-type="string"><text:p>~1</text:p></table:table-cell><table:table-cell table:style-name="ce1"/><table:table-cell table:style-name="ce2"/><table:table-cell table:style-name="ce2" office:value-type="string" table:formula="of:=(A*0.1)"><text:p>=(A*0.1) -- TAX</text:p></table:table-cell><table:table-cell table:style-name="ce3"/><table:table-cell table:style-name="ce2" office:value-type="string"><text:p>TOTAL</text:p></table:table-cell>"#;
        // Synthetic header row: 6 cells (row-key + 5) like export, all written so the row advances col_idx.
        let header6: String = (0..6)
            .map(|_| {
                r#"<table:table-cell office:value-type="string"><text:p>.</text:p></table:table-cell>"#
            })
            .collect();
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:of="urn:oasis:names:tc:opendocument:xmlns:of:1.2" office:version="1.2">
<office:body><office:spreadsheet>
<table:table table:name="Sheet1">
<table:table-row>{header6}</table:table-row>
<table:table-row>{tax_row}</table:table-row>
</table:table>
</office:spreadsheet></office:body>
</office:document-content>"#,
        );
        let wb = parse_ods_content_with_layout(&xml, &layouts).expect("parse");
        let g = &wb.sheets[0].state.grid;
        let h703 = CellAddr::Header {
            row: (HEADER_ROWS - 1) as u32,
            col: ColumnAddr::Main(1),
        };
        let h706 = CellAddr::Header {
            row: (HEADER_ROWS - 1) as u32,
            col: ColumnAddr::Main(4),
        };
        let t703 = g.text(&h703);
        let t706 = g.text(&h706);
        assert!(
            t703.contains("TAX") && t706.is_empty(),
            "TAX must land in header col 703, not 706: 703={t703:?} 706={t706:?}"
        );
    }

    /// ODF: `<table:table` is a prefix of `<table:table-column>`, etc. Find the data
    /// `table:table` element (the byte after `table:table` is not `-` — column/row have `…-column`,
    /// `…-row`, `…-cell`, …).
    fn table_table_open_byte_offset(s: &str) -> Option<usize> {
        const OPEN: &str = "<table:table";
        let mut at = 0usize;
        while let Some(i) = s[at..].find(OPEN) {
            let abs = at + i;
            // `…-cell`, `…-column`, `…-row` all use `-` right after the second `table` in the tag
            // name; the sheet element is `table:table` (space, `>`, or attr) with no extra `-` here.
            let is_data_table = s
                .as_bytes()
                .get(abs + OPEN.len())
                .map_or(true, |&b| b != b'-');
            if is_data_table {
                return Some(abs);
            }
            at = abs + OPEN.len();
        }
        None
    }

    /// String-level: the first **data** ODF `table:table-row` (2nd `table:table-row` in the
    /// `split`, after `.skip(1)` drops the prologue) must have TAX in the 4th cell (`gc` 703 for
    /// `row_key=1` when `c=3`).
    #[test]
    fn subtotal_export_ods_2nd_row_4th_cell_is_tax_string() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("subtotal.corro");
        if !p.is_file() {
            return;
        }
        let data = std::fs::read_to_string(&p).expect("read");
        let mut wb = WorkbookState::new();
        let mut active = wb.sheet_id(wb.active_sheet);
        for line in data.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            crate::ops::apply_log_line_to_workbook(t, &mut wb, &mut active).expect("log");
        }
        let opts = DelimitedExportOptions {
            content: ExportContent::Generic,
            ..Default::default()
        };
        let bytes = export_ods_bytes_workbook_with_options(&wb, &opts).expect("export");
        let mut a = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        let mut full = String::new();
        a.by_name("content.xml")
            .expect("content.xml")
            .read_to_string(&mut full)
            .expect("read");
        let sp = full.find("<office:spreadsheet>").expect("spreadsheet");
        let rel = &full[sp..];
        let t0 = table_table_open_byte_offset(rel).expect("data table");
        let t_from = sp + t0;
        let t_tail = &full[t_from..];
        let t_end = t_tail.find("</table:table>").expect("end table");
        let t_only = &t_tail[..t_end];
        let row_parts: Vec<&str> = t_only
            .split("<table:table-row>")
            .skip(1)
            .map(|s| s.split("</table:table-row>").next().unwrap_or(""))
            .collect();
        assert!(
            row_parts.len() >= 2,
            "expected prologue+ ≥2 ODF rows, got {} row segments",
            row_parts.len()
        );
        // `skip(1)` removed `parts[0]` (prologue); `row_parts[0]` = 1st ODF row = TSV header,
        // `row_parts[1]` = 2nd ODF row = 1st data line (`~1` + TAX).
        let first_data = row_parts[1];
        let cell_chunks: Vec<&str> = first_data.split("<table:table-cell").skip(1).collect();
        assert!(
            cell_chunks.len() >= 4,
            "1st data ODF row: want ≥4 cells, got {} (prefix ~800B: {})",
            cell_chunks.len(),
            first_data.chars().take(800).collect::<String>()
        );
        let tax_chunk = cell_chunks[3];
        assert!(
            tax_chunk.contains("TAX"),
            "4th ODF cell in 1st data row must be TAX; got prefix: {}",
            tax_chunk.chars().take(200).collect::<String>()
        );
    }

    /// The first **data** row (TAX) must stay on global col 703 even when a second data row is
    /// present: widening `main_cols` during re-import used to remapped header `gc` 703→706+.
    #[test]
    fn subtotal_ods_table_3_ods_row_prefix_taxes_703() {
        use crate::formula::refresh_spills;
        const K: usize = 3;
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("subtotal.corro");
        if !p.is_file() {
            return;
        }
        let data = std::fs::read_to_string(&p).expect("read");
        let mut wb = WorkbookState::new();
        let mut active = wb.sheet_id(wb.active_sheet);
        for line in data.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            crate::ops::apply_log_line_to_workbook(t, &mut wb, &mut active).expect("log");
        }
        for s in &mut wb.sheets {
            refresh_spills(&mut s.state.grid);
        }
        let opts = DelimitedExportOptions {
            content: ExportContent::Generic,
            ..Default::default()
        };
        let bytes = export_ods_bytes_workbook_with_options(&wb, &opts).expect("export");
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).expect("zip");
        let mut full = String::new();
        z.by_name("content.xml")
            .expect("content")
            .read_to_string(&mut full)
            .expect("read");
        let mut s2 = z.by_name(CORRO_ODS_LAYOUT_PATH).expect("sidecar");
        let mut side = String::new();
        s2.read_to_string(&mut side).expect("read");
        let one_layout = vec![parse_corro_ods_layout_list(&side)
            .first()
            .expect("v2[0]")
            .clone()];
        let sp = full.find("<office:spreadsheet>").expect("spreadsheet");
        let rel = &full[sp..];
        let t0 = table_table_open_byte_offset(rel).expect("data table");
        let t_from = sp + t0;
        let t_tail = &full[t_from..];
        let t_end = t_tail.find("</table:table>").expect("end table");
        let t_only = &t_tail[..t_end];
        let row_parts: Vec<&str> = t_only.split("<table:table-row>").collect();
        assert!(row_parts.len() > K, "expected ODF data rows, got {} parts", row_parts.len());
        let mut inner = String::new();
        inner.push_str(row_parts[0]);
        for p in &row_parts[1..=K] {
            inner.push_str("<table:table-row>");
            inner.push_str(p);
        }
        inner.push_str("</table:table>");
        let one_table_xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:of="urn:oasis:names:tc:opendocument:xmlns:of:1.2" office:version="1.2">
<office:body><office:spreadsheet>{}</office:spreadsheet></office:body>
</office:document-content>"#,
            inner
        );
        let back = parse_ods_content_with_layout(&one_table_xml, &one_layout).expect("import");
        let g = &back.sheets[0].state.grid;
        let t703 = g.text(&CellAddr::Header {
            row: (HEADER_ROWS - 1) as u32,
            col: ColumnAddr::Main(1),
        });
        assert!(t703.contains("TAX"), "TAX in 703 after 3 ODF row prefix; 706={:?}", g.text(&CellAddr::Header { row: (HEADER_ROWS - 1) as u32, col: ColumnAddr::Main(4) }));
    }

    /// Only the TSV **header** + first **data** ODF `table:table-row` (two rows), from a real
    /// `subtotal.corro` ODS. If the full sheet shifts TAX to 706, this test isolates whether a
    /// 2×`tc` first table re-imports TAX to 703.
    #[test]
    fn subtotal_two_ods_header_plus_data_row_only_taxes_703() {
        use crate::formula::refresh_spills;
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("subtotal.corro");
        if !p.is_file() {
            return;
        }
        let data = std::fs::read_to_string(&p).expect("read");
        let mut wb = WorkbookState::new();
        let mut active = wb.sheet_id(wb.active_sheet);
        for line in data.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            crate::ops::apply_log_line_to_workbook(t, &mut wb, &mut active).expect("log");
        }
        for s in &mut wb.sheets {
            refresh_spills(&mut s.state.grid);
        }
        let opts = DelimitedExportOptions {
            content: ExportContent::Generic,
            ..Default::default()
        };
        let bytes = export_ods_bytes_workbook_with_options(&wb, &opts).expect("export");
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).expect("zip");
        let mut full = String::new();
        z.by_name("content.xml")
            .expect("content.xml")
            .read_to_string(&mut full)
            .expect("read");
        let mut s2 = z.by_name(CORRO_ODS_LAYOUT_PATH).expect("sidecar");
        let mut side = String::new();
        s2.read_to_string(&mut side).expect("read");
        let layouts = parse_corro_ods_layout_list(&side);
        let one_layout = vec![layouts.first().expect("v2[0]").clone()];
        let sp = full.find("<office:spreadsheet>").expect("spreadsheet");
        let rel = &full[sp..];
        let t0 = table_table_open_byte_offset(rel).expect("data table");
        let t_from = sp + t0;
        let t_tail = &full[t_from..];
        let t_end = t_tail.find("</table:table>").expect("end table");
        let t_only = &t_tail[..t_end];
        let row_parts: Vec<&str> = t_only.split("<table:table-row>").collect();
        assert!(
            row_parts.len() > 2,
            "expected ≥2 ODF data rows, got {} parts",
            row_parts.len() - 1
        );
        // `t_only` omits the closing `</table:table>`. `parts[0]` already starts with
        // `<table:table…>`; only rebuild the first two data rows.
        let two_rows_inner = {
            let mut o = String::new();
            o.push_str(row_parts[0]);
            for p in &row_parts[1..3] {
                o.push_str("<table:table-row>");
                o.push_str(p);
            }
            o.push_str("</table:table>");
            o
        };
        let one_table_xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:of="urn:oasis:names:tc:opendocument:xmlns:of:1.2" office:version="1.2">
<office:body><office:spreadsheet>{}</office:spreadsheet></office:body>
</office:document-content>"#,
            two_rows_inner
        );
        let back = parse_ods_content_with_layout(&one_table_xml, &one_layout).expect("import");
        let g = &back.sheets[0].state.grid;
        let t = g.text(&CellAddr::Header {
            row: (HEADER_ROWS - 1) as u32,
            col: ColumnAddr::Main(1),
        });
        assert!(t.contains("TAX"), "2-row slice of real export: TAX in 703, got {t:?}");
    }

    /// `subtotal.corro` multi-sheet: re-import only the first `table:table` from exported ODS. If
    /// TAX then lands on 703, the +3 error comes from the second `table` / v2 `layouts` handoff.
    #[test]
    fn subtotal_first_ods_table_only_taxes_703() {
        use crate::formula::refresh_spills;
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("subtotal.corro");
        if !p.is_file() {
            return;
        }
        let data = std::fs::read_to_string(&p).expect("read");
        let mut wb = WorkbookState::new();
        let mut active = wb.sheet_id(wb.active_sheet);
        for line in data.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            crate::ops::apply_log_line_to_workbook(t, &mut wb, &mut active).expect("log");
        }
        for s in &mut wb.sheets {
            refresh_spills(&mut s.state.grid);
        }
        let opts = DelimitedExportOptions {
            content: ExportContent::Generic,
            ..Default::default()
        };
        let bytes = export_ods_bytes_workbook_with_options(&wb, &opts).expect("export");
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).expect("zip");
        let mut full = String::new();
        z.by_name("content.xml")
            .expect("content.xml")
            .read_to_string(&mut full)
            .expect("read");
        let mut s = z.by_name(CORRO_ODS_LAYOUT_PATH).expect("sidecar");
        let mut side = String::new();
        s.read_to_string(&mut side).expect("read side");
        let layouts = parse_corro_ods_layout_list(&side);
        let one_layout = vec![layouts.first().expect("v2 layout").clone()];
        let only_first_table = {
            let sp = full.find("<office:spreadsheet>").expect("spreadsheet");
            let rel = &full[sp..];
            let t0 = table_table_open_byte_offset(rel).expect("data table");
            let from = sp + t0;
            let tail = &full[from..];
            let end = tail.find("</table:table>").expect("end table")
                + "</table:table>".len();
            &full[from..from + end]
        };
        let one_table_xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:of="urn:oasis:names:tc:opendocument:xmlns:of:1.2" office:version="1.2">
<office:body><office:spreadsheet>{only_first_table}</office:spreadsheet></office:body>
</office:document-content>"#,
        );
        let back = parse_ods_content_with_layout(&one_table_xml, &one_layout).expect("import");
        let g = &back.sheets[0].state.grid;
        let h703 = CellAddr::Header {
            row: (HEADER_ROWS - 1) as u32,
            col: ColumnAddr::Main(1),
        };
        let t = g.text(&h703);
        assert!(
            t.contains("TAX"),
            "isolated first `table` only: TAX must be in header col 703, got {t:?} (706={:?})",
            g.text(&CellAddr::Header {
                row: (HEADER_ROWS - 1) as u32,
                col: ColumnAddr::Main(4),
            })
        );
    }

    /// The first three ODF `table:table-cell` tags in a TAX data row (before the formula cell) must
    /// not use `table:number-columns-repeated` or import maps TAX to global col 706, not 703.
    #[test]
    fn subtotal_export_ods_tax_row_no_cell_column_repeated() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("subtotal.corro");
        if !p.is_file() {
            return;
        }
        let data = std::fs::read_to_string(&p).expect("read");
        let mut wb = WorkbookState::new();
        let mut active = wb.sheet_id(wb.active_sheet);
        for line in data.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            crate::ops::apply_log_line_to_workbook(t, &mut wb, &mut active).expect("log");
        }
        let opts = DelimitedExportOptions {
            content: ExportContent::Generic,
            ..Default::default()
        };
        let bytes = export_ods_bytes_workbook_with_options(&wb, &opts).expect("export");
        let mut a = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        let mut xml = String::new();
        a.by_name("content.xml")
            .expect("content.xml")
            .read_to_string(&mut xml)
            .expect("read");
        for seg in xml.split("<table:table-row>") {
            if !seg.contains("TAX")
                || !seg.contains("0.1")
                || !seg.contains("~1")
            {
                continue;
            }
            let row = seg.split("</table:table-row>").next().unwrap_or("");
            // First three physical cells: row label + two style-only empties; each tag must not repeat
            // over multiple logical columns, or the importer will advance col_idx (and map TAX to gc 706).
            let mut parts = row.split("table:table-cell");
            let _prologue = parts.next();
            for (i, chunk) in parts.take(3).enumerate() {
                if chunk.contains("table:number-columns-repeated") {
                    panic!(
                        "TAX row cell {i} must not set table:number-columns-repeated; first 400B of cell: {}",
                        &chunk.chars().take(400).collect::<String>()
                    );
                }
            }
        }
    }

    /// `subtotal.corro` (optional in repo): the TAX data row in exported `content.xml` should have
    /// a small fixed number of ODF `table:table-cell` opens (current export: row key + four), with
    /// no `table:number-columns-repeated` on cells (import advances `col_idx` per repeated column).
    #[test]
    fn subtotal_export_ods_tax_row_table_cell_count() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("subtotal.corro");
        if !p.is_file() {
            return;
        }
        let data = std::fs::read_to_string(&p).expect("read subtotal.corro");
        let mut wb = WorkbookState::new();
        let mut active = wb.sheet_id(wb.active_sheet);
        for line in data.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            crate::ops::apply_log_line_to_workbook(t, &mut wb, &mut active).expect("log");
        }
        let opts = DelimitedExportOptions {
            content: ExportContent::Generic,
            ..Default::default()
        };
        let bytes = export_ods_bytes_workbook_with_options(&wb, &opts).expect("export");
        let mut a = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        let mut xml = String::new();
        a.by_name("content.xml")
            .expect("content.xml")
            .read_to_string(&mut xml)
            .expect("read");
        for seg in xml.split("<table:table-row>") {
            if seg.contains("TAX")
                && seg.contains("0.1")
                && seg.contains("~1")
            {
                let row_only = seg.split("</table:table-row>").next().unwrap_or("");
                let n_cell = row_only.matches("<table:table-cell").count();
                assert_eq!(
                    n_cell, 5,
                    "TAX data row: expected 5 ODF <table:table-cell opens>, got {n_cell} (extra opens or column-repeat break TsvParity col_idx)"
                );
            }
        }
    }

    /// Optional `subtotal.corro`: re-import the exported ODS and assert the TAX label lands on the
    /// same marginal **header** B column (global 703) as a live workbook, not +3 (706).
    #[test]
    fn subtotal_ods_reimport_tsv_parity_taxes_marginal_col_703() {
        use crate::formula::refresh_spills;
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("subtotal.corro");
        if !p.is_file() {
            return;
        }
        let data = std::fs::read_to_string(&p).expect("read");
        let mut wb = WorkbookState::new();
        let mut active = wb.sheet_id(wb.active_sheet);
        for line in data.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            crate::ops::apply_log_line_to_workbook(t, &mut wb, &mut active).expect("log");
        }
        for s in &mut wb.sheets {
            refresh_spills(&mut s.state.grid);
        }
        let opts = DelimitedExportOptions {
            content: ExportContent::Generic,
            ..Default::default()
        };
        let bytes = export_ods_bytes_workbook_with_options(&wb, &opts).expect("export");
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).expect("zip");
        let mut side = String::new();
        z.by_name(CORRO_ODS_LAYOUT_PATH)
            .expect("sidecar")
            .read_to_string(&mut side)
            .expect("read side");
        let layouts = parse_corro_ods_layout_list(&side);
        let lay0 = &layouts[0];
        if let OdsTableLayout::TsvParity {
            col_start,
            row_key_cols,
            ..
        } = lay0
        {
            assert_eq!(
                *col_start, 701,
                "v2[0] col_start: expected 701 from export, got {col_start} (sidecar parse bug?)"
            );
            assert_eq!(*row_key_cols, 1, "v2[0] row_key_cols");
        } else {
            panic!("first layout not TsvParity");
        }
        let mut tmp = NamedTempFile::new().expect("tmp");
        std::io::Write::write_all(&mut tmp, &bytes).expect("write");
        let back = import_ods_workbook(tmp.path()).expect("re-import");
        let g = &back.sheets[0].state.grid;
        let h703 = CellAddr::Header {
            row: (HEADER_ROWS - 1) as u32,
            col: ColumnAddr::Main(1),
        };
        let h706 = CellAddr::Header {
            row: (HEADER_ROWS - 1) as u32,
            col: ColumnAddr::Main(4),
        };
        let t = g.text(&h703);
        assert!(
            t.contains("TAX"),
            "TAX re-import: expected in header col 703, got {t:?}; col 706={:?}",
            g.text(&h706)
        );
        assert!(
            !g.text(&h706).contains("TAX"),
            "TAX should not be shifted to col 706: {:?}",
            g.text(&h706)
        );
    }
}
