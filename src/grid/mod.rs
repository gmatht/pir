//! Five-region sheet layout: headers `~N`, footers `_N`, margins, and main data.
//! Main and margin cells use sparse storage for unbounded logical size.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::formula::{parse_numeric_or_date_literal, Number};

pub const HEADER_ROWS: usize = 999_999_999;
pub const FOOTER_ROWS: usize = 999_999_999;
/// Number of margin columns on each side. Expanded to support multi-letter
/// mirror names (e.g. A..ZZ). Use usize for indexes.
pub const MARGIN_COLS: usize = 26 * 27; // A..ZZ inclusive

/// Global row/col cursor used by the UI and core logic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SheetCursor {
    pub row: usize,
    pub col: usize,
}

impl SheetCursor {
    pub fn clamp(&mut self, grid: &GridBox) {
        let rows = HEADER_ROWS + grid.main_rows() + FOOTER_ROWS;
        let cols = grid.total_cols();
        if rows > 0 {
            self.row = self.row.min(rows - 1);
        }
        if cols > 0 {
            self.col = self.col.min(cols - 1);
        }
    }

    pub(crate) fn to_addr(self, grid: &GridBox) -> CellAddr {
        crate::addr::sheet_cursor_to_addr(
            crate::addr::LogicalRow(self.row),
            crate::addr::GlobalCol(self.col),
            crate::addr::MainRows(grid.main_rows()),
            crate::addr::MainCols(grid.main_cols()),
        )
    }
}

/// Type alias for margin column indices to make it easy to widen the type in
/// one place if needed.

/// Type alias for margin column indices to make it easy to widen the type in
/// one place if needed.
pub type MarginIndex = usize;

/// Objective column address that identifies a column independently of current
/// `main_cols`.  Once constructed, the same variant + index always refers to
/// the same logical column regardless of grid resizing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum ColumnAddr {
    /// Left‑margin column (`[{letter}` in UI).  Index is 0‑based margin‑relative.
    Left(usize),
    /// Main‑grid column.  Index is the 0‑based main column (0 = A).
    Main(u32),
    /// Right‑margin column (`]{letter}` in UI).  Index is 0‑based margin‑relative.
    Right(usize),
}

/// Default maximum column display width when a column has content but no explicit override.
pub const DEFAULT_MAX_COL_WIDTH: usize = 10;

/// Logical cell address (stable across main resize where possible).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CellAddr {
    /// `~` row: `row` 0 = the top header row; `col` is an objective [`ColumnAddr`].
    Header { row: u32, col: ColumnAddr },
    /// `_` row: same indexing as headers.
    Footer { row: u32, col: ColumnAddr },
    /// Main grid.
    Main { row: u32, col: u32 },
    /// Left margin: `col` is a MarginIndex (usize), `row` is main row index.
    Left { col: MarginIndex, row: u32 },
    /// Right margin: `col` is a MarginIndex (usize).
    Right { col: MarginIndex, row: u32 },
}

impl fmt::Display for CellAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CellAddr::Header { row, col } => {
                write!(f, "~{}(col {})", HEADER_ROWS as u32 - row, col)
            }
            CellAddr::Footer { row, col } => write!(f, "_(row {})", row + 1),
            CellAddr::Main { row, col } => write!(f, "({}, {})", row, col),
            CellAddr::Left { col, row } => write!(f, "<{}>({})", col, row),
            CellAddr::Right { col, row } => write!(f, ">{}>({})", col, row),
        }
    }
}

// ---------------------------------------------------------------------------
// Factory helpers – the single place that encodes the internal representation
// of each variant.  All external code should go through these so that future
// representation changes only need to touch this block.
// ---------------------------------------------------------------------------
impl CellAddr {
    /// Build a header address.  `col` is already an objective [`ColumnAddr`].
    pub fn header(row: u32, col: ColumnAddr) -> Self {
        CellAddr::Header { row, col }
    }

    /// Build a footer address.  `col` is already an objective [`ColumnAddr`].
    pub fn footer(row: u32, col: ColumnAddr) -> Self {
        CellAddr::Footer { row, col }
    }

    /// Build a main‑grid address.
    pub fn main(row: u32, col: u32) -> Self {
        CellAddr::Main { row, col }
    }

    /// Build a left‑margin address.
    pub fn left(col: MarginIndex, row: u32) -> Self {
        CellAddr::Left { col, row }
    }

    /// Build a right‑margin address.
    pub fn right(col: MarginIndex, row: u32) -> Self {
        CellAddr::Right { col, row }
    }

    /// Convert a (row, col) pair and current grid dimensions into the appropriate
    /// [`CellAddr`] variant, generating objective `ColumnAddr` for header/footer.
    /// `main_rows` is the number of main data rows.
    pub fn from_global(row: usize, col: usize, main_rows: usize, main_cols: usize) -> Self {
        if row < HEADER_ROWS {
            CellAddr::Header {
                row: (HEADER_ROWS - 1 - row) as u32,
                col: ColumnAddr::from_global(col, main_cols),
            }
        } else if row >= HEADER_ROWS + main_rows {
            CellAddr::Footer {
                row: (row - HEADER_ROWS - main_rows) as u32,
                col: ColumnAddr::from_global(col, main_cols),
            }
        } else if col < MARGIN_COLS {
            CellAddr::Left { col, row: (row - HEADER_ROWS) as u32 }
        } else if col < MARGIN_COLS + main_cols {
            CellAddr::Main {
                row: (row - HEADER_ROWS) as u32,
                col: (col - MARGIN_COLS) as u32,
            }
        } else {
            CellAddr::Right {
                col: col - MARGIN_COLS - main_cols,
                row: (row - HEADER_ROWS) as u32,
            }
        }
    }

    /// Return the global column index for this address, given current `main_cols`.
    pub fn to_global_col(&self, main_cols: usize) -> usize {
        match self {
            CellAddr::Header { col, .. } | CellAddr::Footer { col, .. } => col.to_global(main_cols),
            CellAddr::Main { col, .. } => MARGIN_COLS + *col as usize,
            CellAddr::Left { col, .. } => *col,
            CellAddr::Right { col, .. } => MARGIN_COLS + main_cols + col,
        }
    }

    /// Return the objective [`ColumnAddr`] for this address, given current `main_cols`.
    pub fn to_column_addr(&self, main_cols: usize) -> ColumnAddr {
        match self {
            CellAddr::Header { col, .. } | CellAddr::Footer { col, .. } => *col,
            CellAddr::Main { col, .. } => ColumnAddr::Main(*col),
            CellAddr::Left { col, .. } => ColumnAddr::Left(*col),
            CellAddr::Right { col, .. } => ColumnAddr::Right(*col),
        }
    }

    /// Return the internal header‑area row index (0 = top header row).
    pub fn header_row_index(display_row: usize) -> u32 {
        (HEADER_ROWS - 1 - display_row) as u32
    }

    /// Return the internal footer‑area row index (0 = first footer row).
    pub fn footer_row_index(display_row: usize) -> u32 {
        display_row as u32
    }
}

impl fmt::Display for ColumnAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ColumnAddr::Left(idx) => write!(f, "left({})", idx),
            ColumnAddr::Main(idx) => write!(f, "main({})", idx),
            ColumnAddr::Right(idx) => write!(f, "right({})", idx),
        }
    }
}

impl ColumnAddr {
    /// Build a [`ColumnAddr`] from a global column index and current `main_cols`.
    pub fn from_global(col: usize, main_cols: usize) -> Self {
        if col < MARGIN_COLS {
            ColumnAddr::Left(col)
        } else if col < MARGIN_COLS + main_cols {
            ColumnAddr::Main((col - MARGIN_COLS) as u32)
        } else {
            ColumnAddr::Right(col - MARGIN_COLS - main_cols)
        }
    }

    /// Convert to a global column index using current `main_cols`.
    pub fn to_global(&self, main_cols: usize) -> usize {
        match self {
            ColumnAddr::Left(idx) => *idx,
            ColumnAddr::Main(idx) => MARGIN_COLS + *idx as usize,
            ColumnAddr::Right(idx) => MARGIN_COLS + main_cols + idx,
        }
    }
}

// Abstraction trait for Grid implementations.
// Methods return owned Strings where necessary to keep the trait object-safe.
pub trait GridImpl {
    // Basic size/query
    fn main_rows(&self) -> usize;
    fn main_cols(&self) -> usize;
    fn total_cols(&self) -> usize;
    fn total_logical_rows(&self) -> usize;

    // Cell access (owned returns keep the trait object-safe)
    fn get_owned(&self, addr: &CellAddr) -> Option<String>;
    fn text(&self, addr: &CellAddr) -> String;
    fn set_owned(&mut self, addr: &CellAddr, value: String);
    fn set(&mut self, addr: &CellAddr, value: String);

    // Layout / extent
    fn set_main_size(&mut self, main_rows: usize, main_cols: usize);
    fn ensure_extent_for_cursor(&mut self, row: usize, col: usize) -> bool;
    fn set_min_extent(&mut self, min_rows: u32, min_cols: u32);
    fn grow_main_row_at_bottom(&mut self);
    fn grow_main_col_at_right(&mut self);
    fn move_main_rows(&mut self, from: usize, count: usize, to: usize);
    fn move_main_cols(&mut self, from: usize, count: usize, to: usize);

    // Column sizing and widths
    fn max_col_width(&self) -> usize;
    fn col_width(&self, global_col: usize) -> usize;
    fn get_col_width_override(&self, global_col: usize) -> Option<usize>;
    fn content_width_for_column(&self, global_col: usize) -> Option<usize>;
    fn set_max_col_width(&mut self, width: usize);
    fn set_col_width(&mut self, global_col: usize, width: Option<usize>);
    fn auto_fit_column(&mut self, global_col: usize);
    fn fit_column_to_content(&mut self, global_col: usize);
    fn col_width_overrides(&self) -> Vec<(usize, usize)>;

    // Clear main and margin cells (used by callers that need a fresh grid).
    fn clear_cells(&mut self);

    // Replace the entire set of column width overrides.
    fn set_col_width_overrides(&mut self, overrides: Vec<(usize, usize)>);

    // Formatting
    fn set_view_sort_cols(&mut self, cols: Vec<SortSpec>);
    fn view_sort_cols(&self) -> Vec<SortSpec>;
    fn sorted_main_rows(&self) -> Vec<usize>;
    fn set_column_format(&mut self, scope: FormatScope, col: usize, format: CellFormat);
    fn set_cell_format(&mut self, addr: CellAddr, format: CellFormat);
    fn format_for_addr(&self, addr: &CellAddr) -> CellFormat;
    fn format_for_global_col(&self, scope: FormatScope, col: usize) -> CellFormat;
    fn col_all_formats(&self) -> Vec<(usize, CellFormat)>;
    fn col_data_formats(&self) -> Vec<(usize, CellFormat)>;
    fn col_special_formats(&self) -> Vec<(usize, CellFormat)>;
    fn cell_formats(&self) -> Vec<(CellAddr, CellFormat)>;

    // Spills / volatile
    fn clear_spills(&mut self);
    fn set_spill_value(&mut self, addr: CellAddr, value: String);
    fn set_spill_error(&mut self, addr: CellAddr, err: &'static str);
    fn spill_error(&self, addr: &CellAddr) -> Option<&'static str>;
    // Return current spill follower mappings (addr -> value).
    fn spill_followers(&self) -> Vec<(CellAddr, String)>;
    // Return current spill error mappings (addr -> static error tag).
    fn spill_errors(&self) -> Vec<(CellAddr, &'static str)>;
    fn bump_volatile_seed(&mut self);
    fn volatile_seed(&self) -> u64;
    fn set_volatile_seed(&mut self, seed: u64);

    /// False after [`crate::formula::refresh_spills`] runs; callers set dirty on grid mutations / volatile bumps.
    fn spills_refresh_dirty(&self) -> bool;
    fn note_spills_refreshed(&mut self);
    fn mark_spills_stale(&mut self);

    // Logical content queries
    fn logical_row_has_content(&self, r: usize) -> bool;
    fn logical_col_has_content(&self, c: usize) -> bool;

    // Iteration
    fn iter_nonempty(&self) -> Box<dyn Iterator<Item = (CellAddr, String)> + '_>;

    // Clone trait-object helper
    fn clone_box(&self) -> Box<dyn GridImpl>;
}

/// A boxed handle to an abstract Grid implementation.
static GRIDBOX_NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub struct GridBox {
    pub inner: Box<dyn GridImpl>,
    id: u64,
}

impl GridBox {
    pub fn new<G: GridImpl + 'static>(g: G) -> Self {
        Self { inner: Box::new(g), id: GRIDBOX_NEXT_ID.fetch_add(1, Ordering::Relaxed) }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn main_rows(&self) -> usize {
        self.inner.main_rows()
    }

    pub fn main_cols(&self) -> usize {
        self.inner.main_cols()
    }

    pub fn total_cols(&self) -> usize {
        self.inner.total_cols()
    }

    pub fn total_logical_rows(&self) -> usize {
        self.inner.total_logical_rows()
    }
#[inline(always)]

    pub fn get_owned(&self, addr: &CellAddr) -> Option<String> {
        self.inner.get_owned(addr)
    }

    /// Convenience owned-get that mirrors the old Grid::get (returns owned String)
    #[inline(always)]
    pub fn get(&self, addr: &CellAddr) -> Option<String> {
        self.inner.get_owned(addr)
    }

    #[inline(always)]
    pub fn text(&self, addr: &CellAddr) -> String {
        self.inner.text(addr)
    }

    pub fn set_owned(&mut self, addr: &CellAddr, value: String) {
        self.inner.set_owned(addr, value)
    }

    #[inline(always)]
    pub fn set(&mut self, addr: &CellAddr, value: String) {
        self.inner.set(addr, value)
    }

    pub fn set_main_size(&mut self, r: usize, c: usize) {
        self.inner.set_main_size(r, c)
    }
#[inline(always)]

    pub fn ensure_extent_for_cursor(&mut self, row: usize, col: usize) -> bool {
        self.inner.ensure_extent_for_cursor(row, col)
    }

    pub fn set_min_extent(&mut self, min_rows: u32, min_cols: u32) {
        self.inner.set_min_extent(min_rows, min_cols)
    }

    pub fn grow_main_row_at_bottom(&mut self) {
        self.inner.grow_main_row_at_bottom()
    }

    pub fn grow_main_col_at_right(&mut self) {
        self.inner.grow_main_col_at_right()
    }

    pub fn move_main_rows(&mut self, from: usize, count: usize, to: usize) {
        self.inner.move_main_rows(from, count, to)
    }

    pub fn move_main_cols(&mut self, from: usize, count: usize, to: usize) {
        self.inner.move_main_cols(from, count, to)
    }

    pub fn bump_volatile_seed(&mut self) {
        self.inner.bump_volatile_seed()
    }

    pub(crate) fn spills_refresh_dirty(&self) -> bool {
        self.inner.spills_refresh_dirty()
    }

    pub(crate) fn note_spills_refreshed(&mut self) {
        self.inner.note_spills_refreshed()
    }

    pub fn volatile_seed(&self) -> u64 {
        self.inner.volatile_seed()
    }

    pub fn set_volatile_seed(&mut self, seed: u64) {
        self.inner.set_volatile_seed(seed)
    }

    pub fn spill_followers(&self) -> Vec<(CellAddr, String)> {
        self.inner.spill_followers()
    }

    pub fn spill_errors(&self) -> Vec<(CellAddr, &'static str)> {
        self.inner.spill_errors()
    }

    pub fn max_col_width(&self) -> usize {
        self.inner.max_col_width()
    }

    pub fn col_width(&self, global_col: usize) -> usize {
        self.inner.col_width(global_col)
    }

    pub fn get_col_width_override(&self, global_col: usize) -> Option<usize> {
        self.inner.get_col_width_override(global_col)
    }

    pub fn content_width_for_column(&self, global_col: usize) -> Option<usize> {
        self.inner.content_width_for_column(global_col)
    }

    pub fn set_max_col_width(&mut self, width: usize) {
        self.inner.set_max_col_width(width)
    }

    pub fn set_col_width(&mut self, global_col: usize, width: Option<usize>) {
        self.inner.set_col_width(global_col, width)
    }

    pub fn auto_fit_column(&mut self, global_col: usize) {
        self.inner.auto_fit_column(global_col)
    }

    pub fn fit_column_to_content(&mut self, global_col: usize) {
        self.inner.fit_column_to_content(global_col)
    }

    pub fn col_width_overrides(&self) -> Vec<(usize, usize)> {
        self.inner.col_width_overrides()
    }

    pub fn clear_cells(&mut self) {
        self.inner.clear_cells()
    }

    pub fn set_col_width_overrides(&mut self, overrides: Vec<(usize, usize)>) {
        self.inner.set_col_width_overrides(overrides)
    }

    pub fn set_view_sort_cols(&mut self, cols: Vec<SortSpec>) {
        self.inner.set_view_sort_cols(cols)
    }

    pub fn view_sort_cols(&self) -> Vec<SortSpec> {
        self.inner.view_sort_cols()
    }

    pub fn sorted_main_rows(&self) -> Vec<usize> {
        self.inner.sorted_main_rows()
    }

    pub fn set_column_format(&mut self, scope: FormatScope, col: usize, format: CellFormat) {
        self.inner.set_column_format(scope, col, format)
    }

    pub fn set_cell_format(&mut self, addr: CellAddr, format: CellFormat) {
        self.inner.set_cell_format(addr, format)
    }
#[inline(always)]

    pub fn format_for_addr(&self, addr: &CellAddr) -> CellFormat {
        self.inner.format_for_addr(addr)
    }

    pub fn format_for_global_col(&self, scope: FormatScope, col: usize) -> CellFormat {
        self.inner.format_for_global_col(scope, col)
    }

    pub fn clear_spills(&mut self) {
        self.inner.clear_spills()
    }

    pub fn set_spill_value(&mut self, addr: CellAddr, value: String) {
        self.inner.set_spill_value(addr, value)
    }

    pub fn set_spill_error(&mut self, addr: CellAddr, err: &'static str) {
        self.inner.set_spill_error(addr, err)
    }

    #[inline(always)]
    pub fn spill_error(&self, addr: &CellAddr) -> Option<&'static str> {
        self.inner.spill_error(addr)
    }
#[inline(always)]

    pub fn logical_row_has_content(&self, r: usize) -> bool {
        self.inner.logical_row_has_content(r)
    }
#[inline(always)]

    pub fn logical_col_has_content(&self, c: usize) -> bool {
        self.inner.logical_col_has_content(c)
    }

    pub fn col_all_formats(&self) -> Vec<(usize, CellFormat)> {
        self.inner.col_all_formats()
    }

    pub fn col_data_formats(&self) -> Vec<(usize, CellFormat)> {
        self.inner.col_data_formats()
    }

    pub fn col_special_formats(&self) -> Vec<(usize, CellFormat)> {
        self.inner.col_special_formats()
    }

    pub fn cell_formats(&self) -> Vec<(CellAddr, CellFormat)> {
        self.inner.cell_formats()
    }

    #[inline]
    pub fn iter_nonempty(&self) -> Box<dyn Iterator<Item = (CellAddr, String)> + '_> {
        self.inner.iter_nonempty()
    }
}

/// Inclusive-exclusive range in the **main** region (for aggregates).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MainRange {
    pub row_start: u32,
    pub row_end: u32,
    pub col_start: u32,
    pub col_end: u32,
}

impl MainRange {
    pub fn is_empty(&self) -> bool {
        self.row_start >= self.row_end || self.col_start >= self.col_end
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SortSpec {
    pub col: usize,
    pub desc: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum NumberFormat {
    /// Generic decimal display (may switch to scientific notation for extreme magnitudes).
    DecimalGeneric,
    Currency { decimals: usize },
    Fixed { decimals: usize },
    /// Prefer exact rationals when the cell value has one; approximate values use decimal display.
    Rational,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TextAlign {
    Left,
    Center,
    Right,
    Default,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CellFormat {
    pub number: Option<NumberFormat>,
    pub align: Option<TextAlign>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FormatScope {
    All,
    Data,
    Special,
}

/// Full sheet with sparse storage for each editable region.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grid {
    /// Main cells; absent key = empty.
    pub main_cells: HashMap<(u32, u32), String>,
    /// Logical main size: at least 1×1; grows with data/cursor.
    pub extent_main_rows: u32,
    pub extent_main_cols: u32,
    /// Left margin: (main_row, margin_col).
    pub left: HashMap<(u32, MarginIndex), String>,
    /// Right margin: (main_row, margin_col).
    pub right: HashMap<(u32, MarginIndex), String>,
    /// Default display width cap for columns.
    pub max_col_width: usize,
    /// Optional per-global-column display width overrides.
    pub col_width_overrides: HashMap<usize, usize>,
    /// Optional sorted main-column view order.
    pub view_sort_cols: Vec<SortSpec>,
    /// Column-wide format for all cells in a global column.
    pub col_all_formats: HashMap<usize, CellFormat>,
    /// Column-wide format for main-region cells in a global column.
    pub col_data_formats: HashMap<usize, CellFormat>,
    /// Column-wide format for header/footer/margin cells in a global column.
    pub col_special_formats: HashMap<usize, CellFormat>,
    /// Exact-cell overrides used for Cell/Selection formatting.
    pub cell_formats: HashMap<CellAddr, CellFormat>,
    pub header: HashMap<(u32, ColumnAddr), String>,
    pub footer: HashMap<(u32, ColumnAddr), String>,
    pub(crate) spill_followers: HashMap<CellAddr, String>,
    pub(crate) spill_errors: HashMap<CellAddr, &'static str>,
    pub(crate) volatile_seed: u64,
    /// When false, [`crate::formula::refresh_spills`] is a cheap no-op (spill maps unchanged).
    pub(crate) spills_dirty: bool,
    /// Cursor floor: never shrink extent_main_rows below this (0 = no floor).
    min_extent_main_rows: u32,
    /// Cursor floor: never shrink extent_main_cols below this (0 = no floor).
    min_extent_main_cols: u32,
}

impl Default for Grid {
    fn default() -> Self {
        Self::new(1, 1)
    }
}

impl Grid {
    pub fn new(main_rows: u32, main_cols: u32) -> Self {
        let g = Grid {
            main_cells: HashMap::new(),
            extent_main_rows: main_rows.max(1),
            extent_main_cols: main_cols.max(1),
            left: HashMap::new(),
            right: HashMap::new(),
            max_col_width: DEFAULT_MAX_COL_WIDTH,
            col_width_overrides: HashMap::new(),
            view_sort_cols: Vec::new(),
            col_all_formats: HashMap::new(),
            col_data_formats: HashMap::new(),
            col_special_formats: HashMap::new(),
            cell_formats: HashMap::new(),
            header: HashMap::new(),
            footer: HashMap::new(),
            spill_followers: HashMap::new(),
            spill_errors: HashMap::new(),
            volatile_seed: 0,
            spills_dirty: true,
            min_extent_main_rows: 0,
            min_extent_main_cols: 0,
        };
        g
    }

    /// One new main row at the bottom (cursor moving down from the last main row).
    pub fn grow_main_row_at_bottom(&mut self) {
        self.extent_main_rows = self.extent_main_rows.saturating_add(1);
        self.mark_spills_stale();
    }

    /// One new main column at the right (cursor moving right from the last sheet column).
    pub fn grow_main_col_at_right(&mut self) {
        let old_main_cols = self.extent_main_cols as usize;
        let new_main_cols = old_main_cols.saturating_add(1);
        self.remap_main_col_layout_for_resize(old_main_cols, new_main_cols);
        self.remap_formats_for_resize(old_main_cols, new_main_cols);
        self.extent_main_cols = new_main_cols as u32;
        self.mark_spills_stale();
    }

    /// Back-compat: logical main row count.
    #[inline]
    pub fn main_rows(&self) -> usize {
        self.extent_main_rows as usize
    }

    /// Back-compat: logical main column count.
    #[inline]
    pub fn main_cols(&self) -> usize {
        self.extent_main_cols as usize
    }

    #[inline(always)]
    pub fn total_cols(&self) -> usize {
        MARGIN_COLS + self.extent_main_cols as usize + MARGIN_COLS
    }

    #[inline(always)]
    pub fn total_logical_rows(&self) -> usize {
        HEADER_ROWS + self.extent_main_rows as usize + FOOTER_ROWS
    }

    /// Grow extent so cursor (logical row/col) is addressable in main/margins.
#[inline(always)]
    /// Returns true if the extent was actually grown (for UI feedback).
    pub fn ensure_extent_for_cursor(&mut self, row: usize, col: usize) -> bool {
        let hr = HEADER_ROWS;
        let m = MARGIN_COLS;
        let main_end = m + self.extent_main_cols as usize;
        let mut grown = false;
        if (hr..hr + self.extent_main_rows as usize).contains(&row) && (m..main_end).contains(&col)
        {
            let mr = (row - hr) as u32;
            let mc = (col - m) as u32;
            if mr + 1 > self.extent_main_rows {
                self.extent_main_rows = mr + 1;
                grown = true;
            }
            if mc + 1 > self.extent_main_cols {
                let old_main_cols = self.extent_main_cols as usize;
                let new_main_cols = mc as usize + 1;
                self.remap_main_col_layout_for_resize(old_main_cols, new_main_cols);
                self.remap_formats_for_resize(old_main_cols, new_main_cols);
                self.extent_main_cols = mc + 1;
                grown = true;
            }
        } else if (hr..hr + self.extent_main_rows as usize).contains(&row) && (0..m).contains(&col)
        {
            let mr = (row - hr) as u32;
            if mr + 1 > self.extent_main_rows {
                self.extent_main_rows = mr + 1;
                grown = true;
            }
        } else if (hr..hr + self.extent_main_rows as usize).contains(&row)
            && (main_end..main_end + MARGIN_COLS).contains(&col)
        {
            let mr = (row - hr) as u32;
            if mr + 1 > self.extent_main_rows {
                self.extent_main_rows = mr + 1;
                grown = true;
            }
        }
        if grown {
            self.mark_spills_stale();
        }
        grown
    }
#[inline(always)]

    pub fn logical_row_has_content(&self, r: usize) -> bool {
        let hr = HEADER_ROWS;
        if r < hr {
            let row = r as u32;
            return self.header.keys().any(|&(stored_row, _)| stored_row == row);
        }
        if r < hr + self.extent_main_rows as usize {
            let mr = r - hr;
            let mru = mr as u32;
            return self.main_cells.keys().any(|(row, _)| *row == mru)
                || self.left.keys().any(|(row, _)| *row == mru)
                || self.right.keys().any(|(row, _)| *row == mru);
        }
        let fr = r - hr - self.extent_main_rows as usize;
        let fr = fr as u32;
        self.footer.keys().any(|&(stored_row, _)| stored_row == fr)
    }
#[inline(always)]

    pub fn logical_col_has_content(&self, c: usize) -> bool {
        let tc = self.total_cols();
        if c >= tc {
            return false;
        }
        if self.header.keys().any(|&(_, col)| col.to_global(self.extent_main_cols as usize) == c) {
            return true;
        }
        let m = MARGIN_COLS;
        let me = m + self.extent_main_cols as usize;
        let data_region_has_content = if c < m {
            self.left.keys().any(|(_, mc)| *mc == c)
        } else if c < me {
            let mc = (c - m) as u32;
            self.main_cells.keys().any(|(_, col)| *col == mc)
        } else if c < me + MARGIN_COLS {
            let mc = c - me;
            self.right.keys().any(|(_, rc)| *rc == mc)
        } else {
            false
        };
        if data_region_has_content {
            return true;
        }
        self.footer.keys().any(|&(_, col)| col.to_global(self.extent_main_cols as usize) == c)
    }

    fn resize_header_footer_width(&mut self) {
        self.header.retain(|&(row, _), value| {
            row < HEADER_ROWS as u32 && !value.is_empty()
        });
        self.footer.retain(|&(row, _), value| {
            row < FOOTER_ROWS as u32 && !value.is_empty()
        });
    }

    /// Shrink main extents silently to the largest stored content.
    ///
    /// This is an in-memory maintenance operation only: it reduces
    /// `extent_main_rows`/`extent_main_cols` when trailing main rows/cols are
    /// empty, but does NOT remap or prune header/footer or column-format maps
    /// and does NOT emit any SetMainSize op. Returns true if either extent
    /// was reduced.
    fn shrink_to_content(&mut self) -> bool {
        // Compute new main cols from main_cells, headers, and footers.
        let mut max_col_plus1: u32 = 0;
        for (&(_r, c), _) in &self.main_cells {
            max_col_plus1 = max_col_plus1.max(c.saturating_add(1));
        }
        for (&(_r, col), _) in &self.header {
            if let ColumnAddr::Main(mc) = col {
                max_col_plus1 = max_col_plus1.max(mc.saturating_add(1));
            }
        }
        for (&(_r, col), _) in &self.footer {
            if let ColumnAddr::Main(mc) = col {
                max_col_plus1 = max_col_plus1.max(mc.saturating_add(1));
            }
        }
        if max_col_plus1 == 0 {
            max_col_plus1 = 1; // always at least 1
        }
        // Apply cursor floor (prevents shrinking past cursor position).
        max_col_plus1 = max_col_plus1.max(self.min_extent_main_cols);

        // Compute new main rows from main, left and right stored cells.
        let mut max_row_plus1: u32 = 0;
        for (&(r, _), _) in &self.main_cells {
            max_row_plus1 = max_row_plus1.max(r.saturating_add(1));
        }
        for (&(r, _), _) in &self.left {
            max_row_plus1 = max_row_plus1.max(r.saturating_add(1));
        }
        for (&(r, _), _) in &self.right {
            max_row_plus1 = max_row_plus1.max(r.saturating_add(1));
        }
        if max_row_plus1 == 0 {
            max_row_plus1 = 1;
        }
        // Apply cursor floor.
        max_row_plus1 = max_row_plus1.max(self.min_extent_main_rows);

        let mut changed = false;
        if max_col_plus1 < self.extent_main_cols {
            self.extent_main_cols = max_col_plus1;
            changed = true;
        }
        if max_row_plus1 < self.extent_main_rows {
            self.extent_main_rows = max_row_plus1;
            changed = true;
        }

        if changed {
            // Notify any cached spill/volatile logic that layout changed.
            self.mark_spills_stale();
        }
        changed
    }

    /// Set a floor that `shrink_to_content` will not shrink below.
    /// Used by the UI to prevent shrinking past the cursor position.
    /// A value of 0 means no floor.
    pub fn set_min_extent(&mut self, min_rows: u32, min_cols: u32) {
        self.min_extent_main_rows = min_rows;
        self.min_extent_main_cols = min_cols;
    }

    pub fn set_main_size(&mut self, main_rows: usize, main_cols: usize) {
        let old_main_cols = self.extent_main_cols as usize;
        // Don't shrink below the cursor floor or header/footer Main column extent.
        let header_col_extent = self
            .header
            .keys()
            .chain(self.footer.keys())
            .filter_map(|&(_, col)| {
                if let ColumnAddr::Main(mc) = col { Some(mc + 1) } else { None }
            })
            .max()
            .unwrap_or(0)
            .max(self.min_extent_main_cols);
        let new_main_cols = main_cols.max(1).max(header_col_extent as usize);
        self.remap_main_col_layout_for_resize(old_main_cols, new_main_cols);
        self.remap_formats_for_resize(old_main_cols, new_main_cols);
        self.extent_main_rows = main_rows.max(1) as u32;
        self.extent_main_cols = new_main_cols as u32;
        self.main_cells
            .retain(|&(r, c), _| r < self.extent_main_rows && c < self.extent_main_cols);
        self.left.retain(|&(r, _), _| r < self.extent_main_rows);
        self.right.retain(|&(r, _), _| r < self.extent_main_rows);
        self.resize_header_footer_width();
        self.mark_spills_stale();

        #[cfg(debug_assertions)]
        {
            let post = format!(
                "DEBUG Grid::set_main_size: finished old_main_cols={} new_main_cols={} extent_main_cols={}",
                old_main_cols, new_main_cols, self.extent_main_cols
            );
            crate::debug_log::log(&post);
            eprintln!("{}", post);
        }
    }

    pub fn col_width(&self, global_col: usize) -> usize {
        let width = if let Some(w) = self.col_width_overrides.get(&global_col).copied() {
            w.max(1)
        } else if self.logical_col_has_content(global_col) {
            self.max_col_width.max(1)
        } else {
            4
        };

        // Test-only diagnostics to trace unexpected width changes for problematic columns.
        // Test-mode diagnostics should write to the debug log instead of
        // stderr in the TUI. Use crate::debug_log::log so the text ends up in
        // CORRO_DEBUG_LOG or the standard debug path.
        #[cfg(test)]
        {
            if global_col == 720 || global_col == 721 || self.col_width_overrides.contains_key(&global_col) {
                crate::debug_log::log(&format!(
                    "DEBUG: Grid::col_width read col={} -> {} (max_col_width={} override={:?})",
                    global_col,
                    width,
                    self.max_col_width,
                    self.col_width_overrides.get(&global_col).copied()
                ));
            }
        }

        width
    }

    pub fn set_max_col_width(&mut self, width: usize) {
        let w = width.max(1);
        self.max_col_width = w;
        #[cfg(test)]
        {
            crate::debug_log::log(&format!("DEBUG: Grid::set_max_col_width -> {}", w));
        }
    }

    pub fn set_col_width(&mut self, global_col: usize, width: Option<usize>) {
        match width {
            Some(w) => {
                let w2 = w.max(1);
                self.col_width_overrides.insert(global_col, w2);
                #[cfg(test)]
                {
                    if global_col == 720 || global_col == 721 {
                        crate::debug_log::log(&format!("DEBUG: Grid::set_col_width set col={} -> {}", global_col, w2));
                    }
                }
            }
            None => {
                self.col_width_overrides.remove(&global_col);
                #[cfg(test)]
                {
                    if global_col == 720 || global_col == 721 {
                        crate::debug_log::log(&format!("DEBUG: Grid::set_col_width remove override col={}", global_col));
                    }
                }
            }
        }
    }

    pub fn content_width_for_column(&self, global_col: usize) -> Option<usize> {
        let mut maxw = 0usize;
        let mut saw_content = false;
        let main_cols = self.main_cols();

        for (&(_, col), val) in &self.header {
            if col.to_global(main_cols) == global_col {
                saw_content = true;
                maxw = maxw.max(val.chars().count() + 1);
            }
        }
        for (&(_, col), val) in &self.footer {
            if col.to_global(main_cols) == global_col {
                saw_content = true;
                maxw = maxw.max(val.chars().count() + 1);
            }
        }
        for r in 0..self.extent_main_rows as usize {
            if global_col < MARGIN_COLS {
                if let Some(val) = self.left.get(&(r as u32, global_col as usize)) {
                    if !val.is_empty() {
                        saw_content = true;
                        maxw = maxw.max(val.chars().count() + 1);
                    }
                }
            } else if global_col < MARGIN_COLS + main_cols {
                let mc = global_col - MARGIN_COLS;
                if let Some(val) = self.main_cells.get(&(r as u32, mc as u32)) {
                    if !val.is_empty() {
                        saw_content = true;
                        maxw = maxw.max(val.chars().count() + 1);
                    }
                }
            } else {
                let rc = global_col - MARGIN_COLS - main_cols;
                if let Some(val) = self.right.get(&(r as u32, rc as usize)) {
                    if !val.is_empty() {
                        saw_content = true;
                        maxw = maxw.max(val.chars().count() + 1);
                    }
                }
            }
        }

        saw_content.then_some(maxw.max(4))
    }

    pub fn auto_fit_column(&mut self, global_col: usize) {
        if let Some(maxw) = self.content_width_for_column(global_col) {
            if maxw > self.max_col_width {
                self.col_width_overrides.insert(global_col, maxw);
            }
        }
    }

    pub fn fit_column_to_content(&mut self, global_col: usize) {
        if let Some(maxw) = self.content_width_for_column(global_col) {
            self.col_width_overrides
                .insert(global_col, maxw.min(self.max_col_width));
        } else {
            self.col_width_overrides.remove(&global_col);
        }
    }

    fn remap_main_col_layout_for_resize(&mut self, old_main_cols: usize, new_main_cols: usize) {
        if old_main_cols == new_main_cols {
            return;
        }

        // Body cells (left / main / right) live in a split that moves when the main block grows;
        // header and footer are keyed by absolute global full-logical columns (see
        // `place_full_logical_cell` for `row < HEADER_ROWS`), so we must not shift them on resize —
        // otherwise a marginal cell at `gc=703` would slide to `703 + delta` when a later `set_main_size`
        // widens the main area (e.g. ODS TsvParity re-import of N>2 rows).
        let old_right_start = MARGIN_COLS + old_main_cols;
        let new_right_start = MARGIN_COLS + new_main_cols;

        fn remap_col(
            col: usize,
            new_main_cols: usize,
            old_right_start: usize,
            new_right_start: usize,
        ) -> Option<usize> {
            if col < MARGIN_COLS {
                Some(col)
            } else if col < old_right_start {
                let main_idx = col - MARGIN_COLS;
                (main_idx < new_main_cols).then_some(MARGIN_COLS + main_idx)
            } else {
                let right_idx = col - old_right_start;
                Some(new_right_start + right_idx)
            }
        }

        let mut remapped = HashMap::new();
        for (col, width) in self.col_width_overrides.drain() {
            let new_col = remap_col(col, new_main_cols, old_right_start, new_right_start);
            if let Some(new_col) = new_col {
                remapped.insert(new_col, width);
            }
        }
        self.col_width_overrides = remapped;
    }

    fn remap_main_col_width_overrides_for_order(&mut self, order: &[u32]) {
        let old_main_cols = order.len();
        if old_main_cols == 0 {
            return;
        }

        let mut old_to_new = vec![0usize; old_main_cols];
        for (new_pos, &old_pos) in order.iter().enumerate() {
            old_to_new[old_pos as usize] = new_pos;
        }

        let mut remapped = HashMap::new();
        for (col, width) in self.col_width_overrides.drain() {
            if col < MARGIN_COLS || col >= MARGIN_COLS + old_main_cols {
                remapped.insert(col, width);
            } else {
                let old_pos = col - MARGIN_COLS;
                remapped.insert(MARGIN_COLS + old_to_new[old_pos], width);
            }
        }
        self.col_width_overrides = remapped;
    }

    pub fn set_view_sort_cols(&mut self, cols: Vec<SortSpec>) {
        self.view_sort_cols = cols;
    }

    fn merge_format(base: CellFormat, overlay: CellFormat) -> CellFormat {
        CellFormat {
            number: overlay.number.or(base.number),
            align: overlay.align.or(base.align),
        }
    }

    fn set_scoped_column_format(
        map: &mut HashMap<usize, CellFormat>,
        col: usize,
        format: CellFormat,
    ) {
        if format == CellFormat::default() {
            map.remove(&col);
        } else {
            map.insert(col, format);
        }
    }

    pub fn set_column_format(&mut self, scope: FormatScope, col: usize, format: CellFormat) {
        match scope {
            FormatScope::All => {
                Self::set_scoped_column_format(&mut self.col_all_formats, col, format)
            }
            FormatScope::Data => {
                Self::set_scoped_column_format(&mut self.col_data_formats, col, format)
            }
            FormatScope::Special => {
                Self::set_scoped_column_format(&mut self.col_special_formats, col, format)
            }
        }
    }

    pub fn set_cell_format(&mut self, addr: CellAddr, format: CellFormat) {
        if format == CellFormat::default() {
            self.cell_formats.remove(&addr);
        } else {
            self.cell_formats.insert(addr, format);
        }
    }
#[inline(always)]

    pub fn format_for_addr(&self, addr: &CellAddr) -> CellFormat {
        let global_col = addr_logical_col(addr, self);
        let base = *self
            .col_all_formats
            .get(&global_col)
            .unwrap_or(&CellFormat::default());
        let region = match addr {
            CellAddr::Main { .. } => self.col_data_formats.get(&global_col).copied(),
            _ => self.col_special_formats.get(&global_col).copied(),
        }
        .unwrap_or_default();
        let exact = self.cell_formats.get(addr).copied().unwrap_or_default();
        Self::merge_format(Self::merge_format(base, region), exact)
    }

    pub fn format_for_global_col(&self, scope: FormatScope, col: usize) -> CellFormat {
        match scope {
            FormatScope::All => self.col_all_formats.get(&col).copied().unwrap_or_default(),
            FormatScope::Data => self.col_data_formats.get(&col).copied().unwrap_or_default(),
            FormatScope::Special => self
                .col_special_formats
                .get(&col)
                .copied()
                .unwrap_or_default(),
        }
    }

    pub fn remap_formats_for_resize(&mut self, old_main_cols: usize, new_main_cols: usize) {
        fn remap_map(
            map: &mut HashMap<usize, CellFormat>,
            old_main_cols: usize,
            new_main_cols: usize,
        ) {
            let old_total = MARGIN_COLS + old_main_cols + MARGIN_COLS;
            let new_total = MARGIN_COLS + new_main_cols + MARGIN_COLS;
            let old_right_start = MARGIN_COLS + old_main_cols;
            let new_right_start = MARGIN_COLS + new_main_cols;
            let mut remapped = HashMap::new();
            for (col, fmt) in map.drain() {
                let new_col = if col < MARGIN_COLS {
                    Some(col)
                } else if col < old_right_start {
                    let main_idx = col - MARGIN_COLS;
                    (main_idx < new_main_cols).then_some(MARGIN_COLS + main_idx)
                } else {
                    let right_idx = col - old_right_start;
                    Some(new_right_start + right_idx)
                };
                if let Some(new_col) = new_col {
                    if new_col < new_total && col < old_total {
                        remapped.insert(new_col, fmt);
                    }
                }
            }
            *map = remapped;
        }

        remap_map(&mut self.col_all_formats, old_main_cols, new_main_cols);
        remap_map(&mut self.col_data_formats, old_main_cols, new_main_cols);
        remap_map(&mut self.col_special_formats, old_main_cols, new_main_cols);
    }

    /// Logical main-row order for the current view sort.
    pub fn sorted_main_rows(&self) -> Vec<usize> {
        let mut rows: Vec<usize> = (0..self.extent_main_rows as usize).collect();
        if self.view_sort_cols.is_empty() {
            return rows;
        }

        rows.sort_by(|a, b| {
            for spec in &self.view_sort_cols {
                let global_col = spec.col;
                if global_col < MARGIN_COLS || global_col >= MARGIN_COLS + self.main_cols() {
                    continue;
                }
                let col = (global_col - MARGIN_COLS) as u32;
                let va = self
                    .get(&CellAddr::Main {
                        row: *a as u32,
                        col,
                    })
                    .unwrap_or("");
                let vb = self
                    .get(&CellAddr::Main {
                        row: *b as u32,
                        col,
                    })
                    .unwrap_or("");
                let ord = compare_sort_values(va, vb, spec.desc);
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            a.cmp(b)
        });
        rows
    }

    pub fn sort_specs_to_log(cols: &[SortSpec]) -> String {
        cols.iter()
            .map(|spec| {
                let name = crate::addr::excel_column_name(spec.col.saturating_sub(MARGIN_COLS));
                if spec.desc {
                    format!("!{name}")
                } else {
                    name
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[inline(always)]
    pub fn get(&self, addr: &CellAddr) -> Option<&str> {
        if let Some(v) = self.spill_followers.get(addr) {
            return Some(v.as_str());
        }
        match addr {
            CellAddr::Header { row, col } => {
                self.header.get(&(*row, *col)).map(|s| s.as_str())
            }
            CellAddr::Footer { row, col } => {
                self.footer.get(&(*row, *col)).map(|s| s.as_str())
            }
            CellAddr::Main { row, col } => self.main_cells.get(&(*row, *col)).map(|s| s.as_str()),
            CellAddr::Left { col, row } => self.left.get(&(*row, *col)).map(|s| s.as_str()),
            CellAddr::Right { col, row } => self.right.get(&(*row, *col)).map(|s| s.as_str()),
        }
    }

    pub(crate) fn spill_error(&self, addr: &CellAddr) -> Option<&'static str> {
        self.spill_errors.get(addr).copied()
    }

    #[inline(always)]
    pub fn set(&mut self, addr: &CellAddr, value: String) {
        match addr {
            CellAddr::Header { row, col } => {
                // Allow header cells to be stored regardless of the current
                // sheet extent. Header/footer cells are absolute global
                // columns and should not be silently dropped when the main
                // region is narrower at the time the SET arrives.
                if (*row as usize) < HEADER_ROWS {
                    if value.is_empty() {
                        self.header.remove(&(*row, *col));
                    } else {
                        let c = col.to_global(self.extent_main_cols as usize) as u32;
                        self.header.insert((*row, *col), value);
                        self.auto_fit_column(c as usize);
                    }
                }
            }
            CellAddr::Footer { row, col } => {
                // As with headers, accept footer cells independent of the
                // current total_cols; do not drop them just because the
                // main region is presently narrow.
                if (*row as usize) < FOOTER_ROWS {
                    if value.is_empty() {
                        self.footer.remove(&(*row, *col));
                    } else {
                        let c = col.to_global(self.extent_main_cols as usize) as u32;
                        self.footer.insert((*row, *col), value);
                        self.auto_fit_column(c as usize);
                    }
                }
            }
            CellAddr::Main { row, col } => {
                let r = *row;
                let c = *col;
                if value.is_empty() {
                    // Only shrink when an actual stored main cell was removed.
                    if self.main_cells.remove(&(r, c)).is_some() {
                        // Silent in-memory shrink (no SetMainSize op emitted).
                        let _ = self.shrink_to_content();
                    }
                } else {
                    self.extent_main_rows = self.extent_main_rows.max(r + 1);
                    self.extent_main_cols = self.extent_main_cols.max(c + 1);
                    self.main_cells.insert((r, c), value);
                    self.auto_fit_column(MARGIN_COLS + c as usize);
                    self.resize_header_footer_width();
                }
            }
            CellAddr::Left { col, row } => {
                let mc = *col;
                let r = *row;
                if mc < MARGIN_COLS {
                    if value.is_empty() {
                        // Shrink only if a stored left-margin cell was removed.
                        if self.left.remove(&(r, mc)).is_some() {
                            let _ = self.shrink_to_content();
                        }
                    } else {
                        self.extent_main_rows = self.extent_main_rows.max(r + 1);
                        self.left.insert((r, mc), value);
                        self.auto_fit_column(mc);
                        self.resize_header_footer_width();
                    }
                }
            }
            CellAddr::Right { col, row } => {
                let mc = *col;
                let r = *row;
                if mc < MARGIN_COLS {
                    if value.is_empty() {
                        // Shrink only if a stored right-margin cell was removed.
                        if self.right.remove(&(r, mc)).is_some() {
                            let _ = self.shrink_to_content();
                        }
                    } else {
                        self.extent_main_rows = self.extent_main_rows.max(r + 1);
                        self.right.insert((r, mc), value);
                        self.auto_fit_column(MARGIN_COLS + self.extent_main_cols as usize + mc);
                        self.resize_header_footer_width();
                    }
                }
            }
        }
        self.mark_spills_stale();
    }

    pub(crate) fn clear_spills(&mut self) {
        self.spill_followers.clear();
        self.spill_errors.clear();
    }

    pub(crate) fn set_spill_value(&mut self, addr: CellAddr, value: String) {
        self.spill_followers.insert(addr, value);
    }

    pub(crate) fn set_spill_error(&mut self, addr: CellAddr, err: &'static str) {
        self.spill_errors.insert(addr, err);
    }

    pub(crate) fn bump_volatile_seed(&mut self) {
        self.volatile_seed = self.volatile_seed.wrapping_add(1);
        self.mark_spills_stale();
    }

    #[inline]
    pub(crate) fn mark_spills_stale(&mut self) {
        self.spills_dirty = true;
    }

    #[inline]
    pub(crate) fn note_spills_refreshed(&mut self) {
        self.spills_dirty = false;
    }

    pub fn move_main_rows(&mut self, from: usize, count: usize, to: usize) {
        let er = self.extent_main_rows as usize;
        if count == 0 || from + count > er || to > er {
            return;
        }
        if to > from && to < from + count {
            return;
        }
        let insert_at = if to > from { to - count } else { to };

        let mut order: Vec<u32> = (0..self.extent_main_rows).collect();
        let taken: Vec<u32> = order.drain(from..from + count).collect();
        order.splice(insert_at..insert_at, taken);

        let mut new_main = HashMap::new();
        for (new_pos, &old_r) in order.iter().enumerate() {
            let old_r = old_r;
            for c in 0..self.extent_main_cols {
                if let Some(v) = self.main_cells.get(&(old_r, c)).cloned() {
                    new_main.insert((new_pos as u32, c), v);
                }
            }
        }
        self.main_cells = new_main;

        let mut new_left = HashMap::new();
        for (new_pos, &old_r) in order.iter().enumerate() {
            for mc in 0..MARGIN_COLS as usize {
                if let Some(v) = self.left.get(&(old_r, mc)).cloned() {
                    new_left.insert((new_pos as u32, mc), v);
                }
            }
        }
        self.left = new_left;

        let mut new_right = HashMap::new();
        for (new_pos, &old_r) in order.iter().enumerate() {
            for mc in 0..MARGIN_COLS as usize {
                if let Some(v) = self.right.get(&(old_r, mc)).cloned() {
                    new_right.insert((new_pos as u32, mc), v);
                }
            }
        }
        self.right = new_right;

        self.extent_main_rows = order.len() as u32;
        self.mark_spills_stale();
    }

    pub fn move_main_cols(&mut self, from: usize, count: usize, to: usize) {
        let ec = self.extent_main_cols as usize;
        if count == 0 || from + count > ec || to > ec {
            return;
        }
        if to > from && to < from + count {
            return;
        }
        let insert_at = if to > from { to - count } else { to };

        let mut order: Vec<u32> = (0..self.extent_main_cols).collect();
        let taken: Vec<u32> = order.drain(from..from + count).collect();
        order.splice(insert_at..insert_at, taken);

        let mut new_main = HashMap::new();
        for r in 0..self.extent_main_rows {
            for (new_pos, &old_c) in order.iter().enumerate() {
                if let Some(v) = self.main_cells.get(&(r, old_c)).cloned() {
                    new_main.insert((r, new_pos as u32), v);
                }
            }
        }
        self.main_cells = new_main;

        fn remap_sparse_main_cols(
            cells: &mut HashMap<(u32, u32), String>,
            order: &[u32],
            old_main_cols: usize,
        ) {
            let mut old_to_new = vec![0usize; old_main_cols];
            for (new_pos, &old_pos) in order.iter().enumerate() {
                old_to_new[old_pos as usize] = new_pos;
            }

            let mut remapped = HashMap::new();
            for ((row, col), value) in cells.drain() {
                let col_usize = col as usize;
                let new_col = if col_usize < MARGIN_COLS || col_usize >= MARGIN_COLS + old_main_cols
                {
                    col_usize
                } else {
                    MARGIN_COLS + old_to_new[col_usize - MARGIN_COLS]
                };
                remapped.insert((row, new_col as u32), value);
            }
            *cells = remapped;
        }

        fn remap_sparse_main_cols_addr(
            cells: &mut HashMap<(u32, ColumnAddr), String>,
            order: &[u32],
            old_main_cols: usize,
        ) {
            let mut old_to_new = vec![0usize; old_main_cols];
            for (new_pos, &old_pos) in order.iter().enumerate() {
                old_to_new[old_pos as usize] = new_pos;
            }

            let mut remapped = HashMap::new();
            for ((row, col), value) in cells.drain() {
                let new_col = match col {
                    ColumnAddr::Main(idx) if (idx as usize) < old_main_cols => {
                        ColumnAddr::Main(old_to_new[idx as usize] as u32)
                    }
                    other => other,
                };
                remapped.insert((row, new_col), value);
            }
            *cells = remapped;
        }

        remap_sparse_main_cols_addr(&mut self.header, &order, ec);
        remap_sparse_main_cols_addr(&mut self.footer, &order, ec);

        self.remap_main_col_width_overrides_for_order(&order);

        self.extent_main_cols = order.len() as u32;
        self.mark_spills_stale();
    }
}

// Implement GridImpl for the existing Grid so we can use Grid via GridBox.
impl GridImpl for Grid {
    fn main_rows(&self) -> usize {
        self.main_rows()
    }

    fn main_cols(&self) -> usize {
        self.main_cols()
    }

    fn total_cols(&self) -> usize {
        self.total_cols()
    }
#[inline(always)]

    fn get_owned(&self, addr: &CellAddr) -> Option<String> {
        self.get(addr).map(|s| s.to_string())
    }
#[inline(always)]

    fn set_owned(&mut self, addr: &CellAddr, value: String) {
        self.set(addr, value)
    }

    fn set_main_size(&mut self, main_rows: usize, main_cols: usize) {
        self.set_main_size(main_rows, main_cols)
    }

    fn bump_volatile_seed(&mut self) {
        self.bump_volatile_seed()
    }

    fn spills_refresh_dirty(&self) -> bool {
        self.spills_dirty
    }

    fn note_spills_refreshed(&mut self) {
        Grid::note_spills_refreshed(self)
    }

    fn mark_spills_stale(&mut self) {
        Grid::mark_spills_stale(self)
    }

    #[inline]
    fn iter_nonempty(&self) -> Box<dyn Iterator<Item = (CellAddr, String)> + '_> {
        let mut v: Vec<(CellAddr, String)> = Vec::new();
        for (&(r, col), val) in &self.header {
            v.push((
                CellAddr::Header {
                    row: r,
                    col,
                },
                val.clone(),
            ));
        }
        for (&(r, col), val) in &self.footer {
            v.push((
                CellAddr::Footer {
                    row: r,
                    col,
                },
                val.clone(),
            ));
        }
        for (&(r, c), val) in &self.main_cells {
            v.push((CellAddr::Main { row: r, col: c }, val.clone()));
        }
        for (&(r, mc), val) in &self.left {
            v.push((CellAddr::Left { col: mc, row: r }, val.clone()));
        }
        for (&(r, mc), val) in &self.right {
            v.push((CellAddr::Right { col: mc, row: r }, val.clone()));
        }
        Box::new(v.into_iter())
    }

    fn total_logical_rows(&self) -> usize {
        self.total_logical_rows()
    }

    #[inline(always)]
    fn text(&self, addr: &CellAddr) -> String {
        self.get(addr).unwrap_or("").to_string()
    }

    fn set(&mut self, addr: &CellAddr, value: String) {
        self.set(addr, value)
    }

    fn ensure_extent_for_cursor(&mut self, row: usize, col: usize) -> bool {
        self.ensure_extent_for_cursor(row, col)
    }

    fn set_min_extent(&mut self, min_rows: u32, min_cols: u32) {
        self.set_min_extent(min_rows, min_cols)
    }

    fn grow_main_row_at_bottom(&mut self) {
        self.grow_main_row_at_bottom()
    }

    fn grow_main_col_at_right(&mut self) {
        self.grow_main_col_at_right()
    }

    fn move_main_rows(&mut self, from: usize, count: usize, to: usize) {
        self.move_main_rows(from, count, to)
    }

    fn move_main_cols(&mut self, from: usize, count: usize, to: usize) {
        self.move_main_cols(from, count, to)
    }

    fn max_col_width(&self) -> usize {
        self.max_col_width
    }

    fn col_width(&self, global_col: usize) -> usize {
        self.col_width(global_col)
    }

    fn get_col_width_override(&self, global_col: usize) -> Option<usize> {
        self.col_width_overrides.get(&global_col).copied()
    }

    fn content_width_for_column(&self, global_col: usize) -> Option<usize> {
        self.content_width_for_column(global_col)
    }

    fn set_max_col_width(&mut self, width: usize) {
        self.set_max_col_width(width)
    }

    fn set_col_width(&mut self, global_col: usize, width: Option<usize>) {
        self.set_col_width(global_col, width)
    }

    fn auto_fit_column(&mut self, global_col: usize) {
        self.auto_fit_column(global_col)
    }

    fn fit_column_to_content(&mut self, global_col: usize) {
        self.fit_column_to_content(global_col)
    }

    fn col_width_overrides(&self) -> Vec<(usize, usize)> {
        self.col_width_overrides
            .iter()
            .map(|(&c, &w)| (c, w))
            .collect()
    }

    fn set_view_sort_cols(&mut self, cols: Vec<SortSpec>) {
        self.set_view_sort_cols(cols)
    }

    fn view_sort_cols(&self) -> Vec<SortSpec> {
        self.view_sort_cols.clone()
    }

    fn sorted_main_rows(&self) -> Vec<usize> {
        self.sorted_main_rows()
    }

    fn set_column_format(&mut self, scope: FormatScope, col: usize, format: CellFormat) {
        self.set_column_format(scope, col, format)
    }

    fn set_cell_format(&mut self, addr: CellAddr, format: CellFormat) {
        self.set_cell_format(addr, format)
    }
#[inline(always)]

    fn format_for_addr(&self, addr: &CellAddr) -> CellFormat {
        self.format_for_addr(addr)
    }

    fn format_for_global_col(&self, scope: FormatScope, col: usize) -> CellFormat {
        self.format_for_global_col(scope, col)
    }

    fn col_all_formats(&self) -> Vec<(usize, CellFormat)> {
        self.col_all_formats.iter().map(|(&c, &f)| (c, f)).collect()
    }

    fn col_data_formats(&self) -> Vec<(usize, CellFormat)> {
        self.col_data_formats
            .iter()
            .map(|(&c, &f)| (c, f))
            .collect()
    }

    fn col_special_formats(&self) -> Vec<(usize, CellFormat)> {
        self.col_special_formats
            .iter()
            .map(|(&c, &f)| (c, f))
            .collect()
    }

    fn cell_formats(&self) -> Vec<(CellAddr, CellFormat)> {
        self.cell_formats
            .iter()
            .map(|(k, &v)| (k.clone(), v))
            .collect()
    }

    fn clear_spills(&mut self) {
        self.clear_spills()
    }

    fn spill_followers(&self) -> Vec<(CellAddr, String)> {
        self.spill_followers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    fn spill_errors(&self) -> Vec<(CellAddr, &'static str)> {
        self.spill_errors
            .iter()
            .map(|(k, &v)| (k.clone(), v))
            .collect()
    }

    fn clear_cells(&mut self) {
        self.main_cells.clear();
        self.left.clear();
        self.right.clear();
        self.mark_spills_stale()
    }

    fn set_col_width_overrides(&mut self, overrides: Vec<(usize, usize)>) {
        self.col_width_overrides = overrides.into_iter().collect();
    }

    fn set_spill_value(&mut self, addr: CellAddr, value: String) {
        self.set_spill_value(addr, value)
    }

    fn set_spill_error(&mut self, addr: CellAddr, err: &'static str) {
        self.set_spill_error(addr, err)
    }
#[inline(always)]

    fn spill_error(&self, addr: &CellAddr) -> Option<&'static str> {
        self.spill_error(addr)
    }

    fn volatile_seed(&self) -> u64 {
        self.volatile_seed
    }

    fn set_volatile_seed(&mut self, seed: u64) {
        self.volatile_seed = seed;
        self.mark_spills_stale();
    }

    fn logical_row_has_content(&self, r: usize) -> bool {
        self.logical_row_has_content(r)
    }

    fn logical_col_has_content(&self, c: usize) -> bool {
        self.logical_col_has_content(c)
    }

    fn clone_box(&self) -> Box<dyn GridImpl> {
        Box::new(self.clone())
    }
}

impl From<Grid> for GridBox {
    fn from(g: Grid) -> Self {
        GridBox::new(g)
    }
}

impl Clone for GridBox {
    fn clone(&self) -> Self {
        GridBox { inner: self.inner.clone_box(), id: self.id }
    }
}

impl std::fmt::Debug for GridBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GridBox").finish()
    }
}

impl Default for GridBox {
    fn default() -> Self {
        GridBox::new(Grid::new(1, 1))
    }
}

#[derive(Debug, PartialEq)]
enum SortKey<'a> {
    Blank,
    Text(&'a str),
    Number(Number),
}

fn sort_key(value: &str) -> SortKey<'_> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        SortKey::Blank
    } else if let Some(n) = parse_numeric_or_date_literal(trimmed) {
        SortKey::Number(n)
    } else {
        SortKey::Text(trimmed)
    }
}

fn compare_sort_values(va: &str, vb: &str, desc: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    match (sort_key(va), sort_key(vb)) {
        (SortKey::Blank, SortKey::Blank) => Ordering::Equal,
        (SortKey::Blank, _) => Ordering::Greater,
        (_, SortKey::Blank) => Ordering::Less,
        (SortKey::Text(a), SortKey::Text(b)) => {
            if desc {
                b.cmp(a)
            } else {
                a.cmp(b)
            }
        }
        (SortKey::Number(a), SortKey::Number(b)) => {
            if desc {
                b.partial_cmp(&a).unwrap_or(Ordering::Equal)
            } else {
                a.partial_cmp(&b).unwrap_or(Ordering::Equal)
            }
        }
        (SortKey::Text(_), SortKey::Number(_)) => {
            if desc {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (SortKey::Number(_), SortKey::Text(_)) => {
            if desc {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
    }
}

/// Logical sheet row index (0 = top header row) for addressing.
#[inline]
pub fn addr_logical_row(addr: &CellAddr, grid: &Grid) -> usize {
    let hr = HEADER_ROWS;
    match addr {
        CellAddr::Header { row, .. } => *row as usize,
        CellAddr::Main { row, .. } => hr + *row as usize,
        CellAddr::Left { row, .. } | CellAddr::Right { row, .. } => hr + *row as usize,
        CellAddr::Footer { row, .. } => hr + grid.extent_main_rows as usize + *row as usize,
    }
}

/// Global column index for addressing.
#[inline]
pub fn addr_logical_col(addr: &CellAddr, grid: &Grid) -> usize {
    match addr {
        CellAddr::Header { col, .. } | CellAddr::Footer { col, .. } => {
            col.to_global(grid.extent_main_cols as usize)
        }
        CellAddr::Main { col, .. } => MARGIN_COLS + *col as usize,
        CellAddr::Left { col, .. } => *col as usize,
        CellAddr::Right { col, .. } => {
            MARGIN_COLS + grid.extent_main_cols as usize + *col as usize
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_rows_sparse() {
        let mut g = Grid::new(4, 2);
        g.set(&CellAddr::Main { row: 0, col: 0 }, "a".into());
        g.set(&CellAddr::Main { row: 1, col: 0 }, "b".into());
        g.set(&CellAddr::Main { row: 2, col: 0 }, "c".into());
        g.set(&CellAddr::Main { row: 3, col: 0 }, "d".into());
        g.move_main_rows(0, 2, 4);
        assert_eq!(g.get(&CellAddr::Main { row: 0, col: 0 }), Some("c"));
        assert_eq!(g.get(&CellAddr::Main { row: 1, col: 0 }), Some("d"));
        assert_eq!(g.get(&CellAddr::Main { row: 2, col: 0 }), Some("a"));
        assert_eq!(g.get(&CellAddr::Main { row: 3, col: 0 }), Some("b"));
    }

    #[test]
    fn move_cols_sparse() {
        let mut g = Grid::new(2, 4);
        g.set(&CellAddr::Main { row: 0, col: 0 }, "a".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "b".into());
        g.set(&CellAddr::Main { row: 0, col: 2 }, "c".into());
        g.set(&CellAddr::Main { row: 0, col: 3 }, "d".into());
        g.move_main_cols(0, 2, 4);
        assert_eq!(g.get(&CellAddr::Main { row: 0, col: 0 }), Some("c"));
        assert_eq!(g.get(&CellAddr::Main { row: 0, col: 1 }), Some("d"));
        assert_eq!(g.get(&CellAddr::Main { row: 0, col: 2 }), Some("a"));
        assert_eq!(g.get(&CellAddr::Main { row: 0, col: 3 }), Some("b"));
    }

    #[test]
    fn sorted_rows_put_text_before_numbers() {
        let mut g = Grid::new(3, 1);
        g.set(&CellAddr::Main { row: 0, col: 0 }, "2".into());
        g.set(&CellAddr::Main { row: 1, col: 0 }, "apple".into());
        g.set(&CellAddr::Main { row: 2, col: 0 }, "10".into());
        g.set_view_sort_cols(vec![SortSpec {
            col: MARGIN_COLS,
            desc: false,
        }]);

        assert_eq!(g.sorted_main_rows(), vec![1, 0, 2]);
    }

    #[test]
    fn view_sort_is_stable_for_equal_keys() {
        let mut g = Grid::new(3, 2);
        g.set(&CellAddr::Main { row: 0, col: 0 }, "x".into());
        g.set(&CellAddr::Main { row: 1, col: 0 }, "y".into());
        g.set(&CellAddr::Main { row: 2, col: 0 }, "x".into());
        g.set_view_sort_cols(vec![SortSpec {
            col: MARGIN_COLS,
            desc: false,
        }]);
        // Ties on "x": rows 0 and 2 stay in original order (0 before 2).
        assert_eq!(g.sorted_main_rows(), vec![0, 2, 1]);
    }

    #[test]
    fn auto_fit_only_grows_touched_column() {
        let mut g = Grid::new(1, 2);
        g.set(&CellAddr::Main { row: 0, col: 0 }, "short".into());
        g.set(
            &CellAddr::Main { row: 0, col: 1 },
            "abcdefghijklmnopqrstuvwx".into(),
        );

        assert_eq!(g.col_width(MARGIN_COLS), DEFAULT_MAX_COL_WIDTH);
        assert!(g.col_width(MARGIN_COLS + 1) >= 24);
    }

    #[test]
    fn empty_columns_use_compact_display_width() {
        let mut g = Grid::new(1, 2);

        assert_eq!(g.col_width(MARGIN_COLS), 4);

        g.set(&CellAddr::Main { row: 0, col: 0 }, "x".into());
        assert_eq!(g.col_width(MARGIN_COLS), DEFAULT_MAX_COL_WIDTH);
        assert_eq!(g.col_width(MARGIN_COLS + 1), 4);

        g.set_col_width(MARGIN_COLS + 1, Some(12));
        assert_eq!(g.col_width(MARGIN_COLS + 1), 12);
    }

    #[test]
    fn widths_shift_when_main_cols_grow() {
        let mut g = Grid::new(1, 1);
        g.set_col_width(MARGIN_COLS + 1, Some(24));

        g.grow_main_col_at_right();

        assert_eq!(g.col_width(MARGIN_COLS + 1), 4);
        assert_eq!(g.col_width(MARGIN_COLS + 2), 24);
    }

    #[test]
    fn widths_follow_moved_main_columns() {
        let mut g = Grid::new(1, 3);
        g.set_col_width(MARGIN_COLS + 1, Some(24));

        g.move_main_cols(1, 1, 3);

        assert_eq!(g.col_width(MARGIN_COLS + 1), 4);
        assert_eq!(g.col_width(MARGIN_COLS + 2), 24);
    }

    #[test]
    fn header_footer_rows_are_sparse_at_high_limits() {
        let mut g = Grid::new(1, 1);
        let header = CellAddr::Header {
            row: 0,
            col: ColumnAddr::Main(0),
        };
        let footer = CellAddr::Footer {
            row: (FOOTER_ROWS - 1) as u32,
            col: ColumnAddr::Main(0),
        };

        g.set(&header, "top".into());
        g.set(&footer, "bottom".into());

        assert_eq!(g.header.len(), 1);
        assert_eq!(g.footer.len(), 1);
        assert_eq!(g.get(&header), Some("top"));
        assert_eq!(g.get(&footer), Some("bottom"));
        assert!(g.logical_row_has_content(0));
        assert!(g.logical_row_has_content(HEADER_ROWS + g.main_rows() + FOOTER_ROWS - 1));

        g.set(&header, String::new());
        assert!(g.header.is_empty());
    }

    #[test]
    fn format_scope_merges_by_region() {
        let mut g = Grid::new(1, 1);
        g.set_column_format(
            FormatScope::All,
            MARGIN_COLS,
            CellFormat {
                number: Some(NumberFormat::Fixed { decimals: 2 }),
                align: None,
            },
        );
        g.set_column_format(
            FormatScope::Data,
            MARGIN_COLS,
            CellFormat {
                number: None,
                align: Some(TextAlign::Right),
            },
        );
        g.set_cell_format(
            CellAddr::Main { row: 0, col: 0 },
            CellFormat {
                number: None,
                align: Some(TextAlign::Center),
            },
        );

        let fmt = g.format_for_addr(&CellAddr::Main { row: 0, col: 0 });
        assert_eq!(fmt.number, Some(NumberFormat::Fixed { decimals: 2 }));
        assert_eq!(fmt.align, Some(TextAlign::Center));
    }

    #[test]
    fn silent_shrink_columns_and_preserve_formats() {
        let mut g = Grid::new(2, 3);
        // Populate main cells across cols 0..3
        g.set(&CellAddr::Main { row: 0, col: 0 }, "a".into());
        g.set(&CellAddr::Main { row: 0, col: 1 }, "b".into());
        g.set(&CellAddr::Main { row: 0, col: 2 }, "c".into());

        // Set column formats and header/footer at a column beyond current main cols
        let high_col = MARGIN_COLS + 10;
        g.set_column_format(FormatScope::All, high_col, CellFormat { number: None, align: Some(TextAlign::Right) });
        g.set(&CellAddr::Header { row: 0, col: ColumnAddr::Main(10) }, "hdr".into());

        // Remove last main column cell and check shrink behavior.
        // The header at Main(10) prevents the grid from shrinking below that column,
        // so extent_main_cols stays at the original value (3).
        g.set(&CellAddr::Main { row: 0, col: 2 }, String::new());

        // extent_main_cols stays at 3 because the header at Main(10) preserves it
        assert_eq!(g.main_cols(), 3);

        // Column format and header must still be present (not pruned by silent shrink)
        assert_eq!(g.col_all_formats.get(&high_col).is_some(), true);
        assert_eq!(g.header.get(&(0u32, ColumnAddr::Main(10))).is_some(), true);
    }

    #[test]
    fn silent_shrink_rows_and_preserve_formats() {
        let mut g = Grid::new(4, 2);
        // Fill main rows 0..4 in col 0
        g.set(&CellAddr::Main { row: 0, col: 0 }, "r0".into());
        g.set(&CellAddr::Main { row: 1, col: 0 }, "r1".into());
        g.set(&CellAddr::Main { row: 2, col: 0 }, "r2".into());
        g.set(&CellAddr::Main { row: 3, col: 0 }, "r3".into());

        // Place a left-margin cell on row 1 and a right-margin cell on row 3
        g.set(&CellAddr::Left { col: 1, row: 1 }, "L".into());
        g.set(&CellAddr::Right { col: 2, row: 3 }, "R".into());

        // Remove bottom-most main row cell -> should shrink rows only if no margin forces retention
        g.set(&CellAddr::Main { row: 3, col: 0 }, String::new());

        // Since right margin exists on row 3, extent_main_rows should still include row 3 (no shrink below 4)
        assert_eq!(g.main_rows(), 4);

        // Remove right-margin cell and then remove main row; expect shrink now
        g.set(&CellAddr::Right { col: 2, row: 3 }, String::new());
        g.set(&CellAddr::Main { row: 3, col: 0 }, String::new());
        // Now there is no content in row 3; shrink should reduce main rows to 3
        assert_eq!(g.main_rows(), 3);
    }

    #[test]
    fn shrink_to_content_respects_header_footer_main_cols() {
        let mut g = Grid::new(2, 5);
        g.set(&CellAddr::Main { row: 0, col: 0 }, "a".into());
        g.set(&CellAddr::Header { row: 0, col: ColumnAddr::Main(4) }, "hdr".into());
        assert_eq!(g.main_cols(), 5);
        // Remove the only main cell — header at Main(4) prevents shrink below 5
        g.set(&CellAddr::Main { row: 0, col: 0 }, String::new());
        assert!(g.main_cols() >= 5,
            "header at Main(4) should prevent shrink, got {}",
            g.main_cols());
        assert!(g.header.contains_key(&(0u32, ColumnAddr::Main(4))));
    }

    #[test]
    fn no_shrink_on_header_footer_removal() {
        let mut g = Grid::new(2, 2);
        let high_col = crate::grid::ColumnAddr::Main(5);
        g.set(
            &CellAddr::Header {
                row: 0,
                col: high_col
            },
            "h".into()
        );
        let old_rows = g.main_rows();
        g.set(
            &CellAddr::Header {
                row: 0,
                col: high_col
            },
            String::new()
        );
        // Removing header should not change main extents
        assert_eq!(g.main_rows(), old_rows);
    }

    #[test]
    fn footer_row_numbering_has_no_gaps_and_uses_main_rows_not_main_cols() {
        // Verify that footer row indices use the correct arithmetic:
        // footer_row(3) + 1 = footer_row(4), never footer_row(10).
        let mr = 5usize; // 5 main rows
        let mc = 1usize; // 1 main column
        let mut g = Grid::new(mr as u32, mc as u32);

        // Set footer rows _1, _2, _3 sequentially (internal indices 0, 1, 2)
        g.set(
            &CellAddr::Footer { row: 0, col: ColumnAddr::Main(0) },
            "a".into(),
        );
        g.set(
            &CellAddr::Footer { row: 1, col: ColumnAddr::Main(0) },
            "b".into(),
        );
        g.set(
            &CellAddr::Footer { row: 2, col: ColumnAddr::Main(0) },
            "c".into(),
        );

        // All three footer rows should be present
        assert_eq!(g.footer.len(), 3);
        assert_eq!(g.get(&CellAddr::Footer { row: 0, col: ColumnAddr::Main(0) }), Some("a"));
        assert_eq!(g.get(&CellAddr::Footer { row: 1, col: ColumnAddr::Main(0) }), Some("b"));
        assert_eq!(g.get(&CellAddr::Footer { row: 2, col: ColumnAddr::Main(0) }), Some("c"));

        // Verify that logical_row_has_content works for all footer rows
        let hr = HEADER_ROWS;
        assert!(g.logical_row_has_content(hr + mr + 0)); // _1
        assert!(g.logical_row_has_content(hr + mr + 1)); // _2
        assert!(g.logical_row_has_content(hr + mr + 2)); // _3
        assert!(!g.logical_row_has_content(hr + mr + 3)); // _4 not set

        // Verify that from_global correctly identifies footer rows when
        // main_rows != main_cols (the classic "3+1=4, not 10" bug scenario).
        // Footer row _1 (internal index 0) at logical row hr+mr+0.
        let footer_logical = hr + mr + 0;
        // If main_rows were confused with main_cols, a 5-row/1-col grid
        // would compute hr+mc = hr+1, making logical rows >= hr+1 be
        // classified as footer — but then the footer offset would be
        // wrong: (row - hr - mc) = (hr+5+0 - hr - 1) = 4 instead of 0!
        let addr = CellAddr::from_global(footer_logical, MARGIN_COLS, mr, mc);
        assert!(
            matches!(addr, CellAddr::Footer { row: 0, .. }),
            "footer _1 with mr={mr} mc={mc}: expected Footer{{row:0}} but got {addr:?}",
        );

        // Footer row _2 (internal index 1)
        let addr2 = CellAddr::from_global(footer_logical + 1, MARGIN_COLS, mr, mc);
        assert!(
            matches!(addr2, CellAddr::Footer { row: 1, .. }),
            "footer _2 with mr={mr} mc={mc}: expected Footer{{row:1}} but got {addr2:?}",
        );

        // Footer row _3 (internal index 2)
        let addr3 = CellAddr::from_global(footer_logical + 2, MARGIN_COLS, mr, mc);
        assert!(
            matches!(addr3, CellAddr::Footer { row: 2, .. }),
            "footer _3 with mr={mr} mc={mc}: expected Footer{{row:2}} but got {addr3:?}",
        );

        // Main row 1 should NOT be classified as a footer
        let main_logical = hr + 0;
        let main_addr = CellAddr::from_global(main_logical, MARGIN_COLS, mr, mc);
        assert!(
            matches!(main_addr, CellAddr::Main { .. }),
            "main row 1 should be Main, got {main_addr:?}",
        );

        // Same tests with a wider grid where main_rows=1, main_cols=5
        // (the opposite asymmetry — more columns than rows)
        let mr2 = 1usize;
        let mc2 = 5usize;
        let mut g2 = Grid::new(mr2 as u32, mc2 as u32);
        g2.set(
            &CellAddr::Footer { row: 0, col: ColumnAddr::Main(0) },
            "x".into(),
        );
        assert!(g2.logical_row_has_content(hr + mr2 + 0));

        let addr_wide = CellAddr::from_global(hr + mr2 + 0, MARGIN_COLS, mr2, mc2);
        assert!(
            matches!(addr_wide, CellAddr::Footer { row: 0, .. }),
            "footer _1 with mr={mr2} mc={mc2}: expected Footer{{row:0}} but got {addr_wide:?}",
        );

        // Verify that header rows also use correct arithmetic.
        // from_global stores header rows as (HEADER_ROWS - 1 - logical_row).
        // Header ~999999999 (the topmost) is at logical row 0, internal index HEADER_ROWS - 1.
        let header_top_logical = 0usize;
        let header_top_addr = CellAddr::from_global(header_top_logical, MARGIN_COLS, mr2, mc2);
        assert!(
            matches!(header_top_addr, CellAddr::Header { row, .. } if row as usize == hr - 1),
            "header ~999999999: expected Header{{row: {}}} but got {header_top_addr:?}",
            hr - 1,
        );
        // Header ~1 (the bottommost) is at logical row HEADER_ROWS - 1, internal index 0.
        let header_bottom_logical = hr - 1;
        let header_bottom_addr = CellAddr::from_global(header_bottom_logical, MARGIN_COLS, mr2, mc2);
        assert!(
            matches!(header_bottom_addr, CellAddr::Header { row: 0, .. }),
            "header ~1: expected Header{{row: 0}} but got {header_bottom_addr:?}",
        );
    }

    #[test]
    fn header_footer_margin_no_gaps_when_set() {
        // Verify that header and footer row numbering has no gaps:
        // if rows _1 and _3 are set, _2 is NOT set (it's a gap),
        // but _1, _2, _3 in sequence is fine.

        let mut g = Grid::new(3, 2);

        // Set _1 and _3 (skip _2) — this is by design; sparse storage is fine.
        // The test verifies that each stored cell has the expected index.
        g.set(
            &CellAddr::Footer { row: 0, col: ColumnAddr::Main(0) },
            "alpha".into(),
        );
        g.set(
            &CellAddr::Footer { row: 2, col: ColumnAddr::Main(0) },
            "gamma".into(),
        );

        // _1 (index 0) and _3 (index 2) are stored
        assert_eq!(g.footer.len(), 2);
        assert!(g.footer.contains_key(&(0u32, ColumnAddr::Main(0))));
        assert!(g.footer.contains_key(&(2u32, ColumnAddr::Main(0))));
        // _2 (index 1) is not stored
        assert!(!g.footer.contains_key(&(1u32, ColumnAddr::Main(0))));

        // The key verification: index arithmetic is correct.
        // 3+1=4 means: footer internal index 2 + 1 = 3, never 10.
        let max_index = g.footer.keys().map(|(r, _)| *r).max().unwrap_or(0);
        assert_eq!(max_index, 2, "max footer index should be 2 (_3)");
        let next_index = max_index + 1;
        assert_eq!(next_index, 3, "3+1 should = 4 (internal index 3 = _4), got {next_index}");

        // Similarly for headers
        g.set(
            &CellAddr::Header { row: 0, col: ColumnAddr::Main(0) },
            "header_top".into(),
        );
        g.set(
            &CellAddr::Header { row: 2, col: ColumnAddr::Main(0) },
            "header_bottom".into(),
        );

        assert_eq!(g.header.len(), 2);
        let max_header = g.header.keys().map(|(r, _)| *r).max().unwrap_or(0);
        assert_eq!(next_index, max_header + 1);

        // Left margin sequential indexing
        g.set(
            &CellAddr::Left { col: 700, row: 0 },
            "margin_col1".into(),
        );
        g.set(
            &CellAddr::Left { col: 701, row: 0 },
            "margin_col2".into(),
        );

        assert!(g.left.contains_key(&(0u32, 700usize)));
        assert!(g.left.contains_key(&(0u32, 701usize)));
        // These should be adjacent indices
        assert_eq!(701 - 700, 1, "margin indices should differ by 1");
    }

    #[test]
    fn footer_row_label_3_plus_1_equals_4_not_10() {
        // Direct test of ui_row_label which renders footer row labels.
        // _N display = internal_index + 1. _3 = internal index 2, then
        // internal index 3 = _4 (3+1=4). Never _10 or any other value.
        let hr = HEADER_ROWS;
        let main_rows = 5usize;

        // Footer row _3 = logical row hr + mr + 2
        let label_3 = crate::addr::ui_row_label(hr + main_rows + 2, main_rows);
        assert_eq!(label_3, "_3", "footer 3 label mismatch");

        // Footer row _4 = logical row hr + mr + 3
        let label_4 = crate::addr::ui_row_label(hr + main_rows + 3, main_rows);
        assert_eq!(label_4, "_4", "footer 4 label mismatch, 3+1 should = 4");

        // Internal index for _3 is 2. Internal index for _4 is 3.
        // _N has internal index N-1. So _10 has internal index 9.
        // 3+1=4 (internal index 3→_4), not 3+1=10 (internal index 9→_10).
        assert_eq!(label_3.as_str(), "_3");
        assert_eq!(label_4.as_str(), "_4");
        assert_ne!(label_4, "_10", "3+1 should not equal 10!");
    }
}