# Remove column-width overrides

## Current algorithm

`Grid::col_width()` at `grid/mod.rs:912` has three tiers:

```
if override exists  →  override width (min 1)
else if has content →  max_col_width (default 10)
else                →  4
```

The override is a `HashMap<global_col, usize>` written by every codepath
that wants a width different from the two hard-coded defaults:

| Caller | What it does |
|--------|-------------|
| `fit_visible_columns_capped()` | Auto-fit visible columns within screen budget – sets per-column overrides. Also has a cursor-centred window-allocation scheme when budget is tight. |
| `fit_column_to_rendered_content()` | Single-column autofit called from menu / `:fit` |
| `auto_fit_column()` / `fit_column_to_content()` | Manual menu actions (Grid methods) |
| `Op::SetColWidth` | Persisted undoable width changes |
| `:width <col> <val>` CLI command | User command |
| `.corro` log-line parser `COL_WIDTH` | Restores persisted widths |
| `balance.rs` copy | Transfers overrides from source to target sheet |
| Column resize / reorder | Remaps overrides when columns shift |
| 7 test sites | Set explicit overrides for specific tests |

## Problem

The default branch (`has content → max_col_width`) ignores actual cell
text length.  A column holding `"X"` gets the same 10-char allocation as
one holding `"Hello World!"`.  Content-driven sizing only happens when
one of the override-setting paths is *explicitly* invoked – it is not the
default rendering path.

## Proposal

Remove the override HashMap and change `col_width()` to:

```rust
Min(Max(content_width_for_column(global_col)?, 4), max_col_width)
```

`content_width_for_column()` already exists at `grid/mod.rs:974`.  It
iterates every cell in the column (header, footer, main, left margin,
right margin) and returns `max(cell_text.chars().count() + 1)`.  The
result is clamped to `[4, max_col_width]` so empty columns stay narrow
and no column can exceed the user-configured cap.

### What changes

| File | Change |
|------|--------|
| `grid/mod.rs` – `col_width()` | Replace three-tier lookup with `Min(Max(content_width_for_column(…)?, 4), max_col_width)`. |
| `grid/mod.rs` – field + ctor | Remove `col_width_overrides` field from `Grid`. |
| `grid/mod.rs` – trait methods | Remove `set_col_width`, `col_width_overrides`, `set_col_width_overrides` from `GridImpl` trait (or keep as no-ops for backward compat of trait users). |
| `grid/mod.rs` – remap helpers | Remove `remap_main_col_width_overrides_for_resize` and `remap_main_col_width_overrides_for_order` (their only job was remapping the override HashMap). |
| `grid/mod.rs` – `GridImpl` impl | Remove the three method bodies. |
| `grid/mod.rs` – tests | Remove or rework the 3 test sites that call `set_col_width` (they can use `set_max_col_width` instead, or just accept the derived width). |
| `ui/mod.rs` – `fit_visible_columns_capped()` | No longer writes overrides.  The cursor-centred window-shrinking logic can either be: (a) removed and the columns just render at their natural clamped width (trimming in `trim_visible_cols_to_width` will drop rightmost columns that don't fit), or (b) kept as a rendering hint that doesn't persist. |
| `ui/mod.rs` – `fit_column_to_rendered_content()` | Remove (or replace with a `set_max_col_width` call). |
| `ui/mod.rs` – `:width` handler | Set `max_col_width` instead of per-column override. |
| `ui/mod.rs` – 4 test sites | Rework to not depend on per-column overrides. |
| `ops/mod.rs` – `Op::SetColWidth` | Change the persisted op to record a new `max_col_width` instead of a per-column override.  The `.corro` log `COL_WIDTH` line format changes. |
| `balance.rs` | No longer copies overrides (the derived widths are identical when source and target have the same content). |
| `export.rs` – test | Remove the `set_col_width(…, None)` call that clears overrides. |
| `src/bin/inspect_cols.rs` | Remove override diagnostic (or keep reading the removed HashMap). |

### Key concerns

1. **`fit_visible_columns_capped` cursor window** – currently when the
   total desired width exceeds the budget, the function allocates full
   width to a cursor-centred window and shrinks outer columns.  If
   overrides go away, this logic either needs to be removed (rely on
   `trim_visible_cols_to_width` to drop rightmost columns) or kept as a
   transient rendering hint stored separately.

2. **Balance-sheet copy** – the target sheet may have different cell
   content, so the derived width would differ from the source.  This is
   arguably *correct* behaviour – widths should fit the actual content.

3. **`Op::SetColWidth` / `.corro` log compat** – these currently persist
   per-column widths.  Changing them to `max_col_width` (or dropping
   them) would change the on-disk format.  Old `.corro` files with
   `COL_WIDTH` entries would need a migration path (e.g. ignore the
   per-column entry and let the new algorithm derive the width, or use
   the stored width as a one-time override during load).

4. **Performance** – `content_width_for_column` iterates every cell in
   the column every time `col_width` is called.  For spreadsheets with
   thousands of rows this could be costly.  Options: (a) cache the
   result and invalidate on cell mutation, or (b) only scan visible rows
   (like `rendered_width_for_column` already does).

## Migration

1. Add `content_width_for_column` call to `col_width()`.
2. Remove override HashMap and all code that writes/reads it.
3. Update tests.
4. Decide on `fit_visible_columns_capped` future.
5. Decide on `.corro` log backward compat.
6. Profile and add caching if needed.
