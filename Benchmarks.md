# Benchmarks

## Benchmark binary

`src/bin/pgo_mix_benchmark.rs` — a multi-mode harness producing TSV output for piping into analysis tools.

```bash
cargo run --release --bin pgo_mix_benchmark -- --bench eval --duration 10
```

### Modes

| `--bench` | What it measures | Real-world analogy |
|-----------|-----------------|--------------------|
| `mix` | Mixed replay + nav workload (original PGO) | Full-session usage |
| `render` | Standalone `draw()` calls per second | Scrolling through a sheet |
| `nav` | Arrow key + `draw()` cycles per second | Keyboard navigation |
| `eval` | Formula evaluations per second | Editing/recalculating |
| `replay` | `.corro` log lines replayed per second | Loading/syncing a file |
| `export` | TSV + CSV exports per second | Saving/exporting data |
| `startup` | Load + draw completions per second | Launching the app |
| `aggregate` | Aggregate calculations per second | Scrolling with margin totals |

All output is TSV: `bench<TAB>wall_ms<TAB>count<TAB>rate<TAB>meta`. Pipe-friendly:

```bash
cargo run --release --bin pgo_mix_benchmark -- --bench eval --duration 10 |
  column -t -s $'\t'
```

### Benchmark grid sizes

| Mode | Rows | Cols | Cells |
|------|------|------|-------|
| render / nav | 200 | 52 | 10,400 |
| eval | 200 + 100 formula | 52 | ~10,400 |
| aggregate | 500 | 20 | 10,000 |
| export | 100 | 26 | 2,600 |
| startup | varies | — | .corro fixtures |

---

## Hot functions

Functions annotated with `#[optimize(speed)]` (nightly-only) to override the
`opt-level = "z"` base in the tiny profile. Sorted by heat:

### Tier 1 — Eval pipeline (10 functions in `formula/mod.rs`)

| Function | Why hot |
|----------|---------|
| `eval_cell` | Entry point for every formula evaluation |
| `eval_cell_inner` | Core recursive eval with cycle detection |
| `eval_ast` | Recursive AST walker dispatching on all node types |
| `eval_expr_str` | Parse-then-eval for inline `=...` strings |
| `eval_binary_op` | Binary arithmetic dispatched by `BinaryOp` |
| `eval_binary_float` | Float-path for POWER, trig, etc. |
| `eval_sum` | Top-level SUM dispatcher |
| `sum_main_range` | Nested row×col loop over a `MainRange` |
| `refresh_spills` | Iterates *all* non-empty cells looking for array spills — called on every `draw()` |
| `cell_effective_display` | Formats a cell for display (evaluates formulas, handles templates) — called per visible cell per frame |

### Tier 1 — Builtin dispatch (2 functions in `formula/functions.rs`)

| Function | Why hot |
|----------|---------|
| `eval_builtin` | Giant ~60-way match dispatching named functions |
| `collect_numeric_values` | Nested range loop for SUM/AVERAGE/COUNT |

### Tier 1 — Apply (1 function in `ops/mod.rs`)

| Function | Why hot |
|----------|---------|
| `Op::apply` | Giant match on all Op variants (SetCell, Duplicate*, Move*, etc.) — replayed on load and sync |

### Tier 1 — Grid access (7 functions in `grid/mod.rs`)

| Function | Why hot |
|----------|---------|
| `GridBox::get`, `GridBox::set`, `GridBox::text`, `GridBox::iter_nonempty` | Trait-object delegation to concrete Grid |
| `Grid::get`, `Grid::set` | HashMap lookup/insert into 5 sparse regions |
| `GridImpl::iter_nonempty` | Iterates all 5 HashMaps — called by viewport, spills, aggregates, export |

### Tier 1 — Aggregates (1 function in `agg/mod.rs`)

| Function | Why hot |
|----------|---------|
| `compute_aggregate` | Dispatches to `collect_numbers_summable` or `count_numeric_cells` for every visible margin cell |

### Tier 2 — Eval helpers (18 functions in `formula/mod.rs`)

`eval_plain_cell_raw`, `parse_number_literal`, `parse_numeric_or_date_literal`,
`split_labeled_formula`, `control_formula_expr`, `control_formula_label`,
`templated_formula`, `effective_numeric`, `summable_numeric`, `is_formula`,
`truthy`, `coerce_cell_number`, `eval_binary_float_with_complex_fallback`,
`format_number`, `format_number_cell_display`, `eval_result_to_string`,
`formula_references_all_empty`, `cell_reference_is_empty`

### Tier 2 — Builtins (12 functions in `formula/functions.rs`)

`eval_numeric_aggregate`, `count_numeric_values`, `count_nonempty_values`,
`eval_sumproduct`, `eval_match`, `eval_index`, `eval_sort`, `eval_countif`,
`eval_sumif`, `collect_matrix_values`, `criteria_from_ast`, `criteria_matches`

### Tier 2 — Ops parsing (7 functions in `ops/mod.rs`)

`parse_op_text`, `parse_workbook_line`, `apply_workbook_op`,
`apply_log_line_to_workbook`, `apply_any_line`, `margin_key_agg_func`,
`parse_op_line`

### Tier 2 — Address parsing (4 functions in `addr.rs`)

`parse_cell_ref_at`, `cell_ref_text`, `excel_column_name`, `parse_main_range_at`

### Tier 2 — Agg helpers (4 functions in `agg/mod.rs`)

`collect_numbers_summable`, `count_numeric_cells`, `cell_display`,
`median_aggregate`

### Tier 2 — Grid accessors (6 functions in `grid/mod.rs`)

`Grid::main_rows`, `Grid::main_cols`, `Grid::total_cols`,
`GridBox::spill_error`, `addr_logical_row`, `addr_logical_col`

---

## Performance comparison

Numbers from 5-second runs on a single machine. **Release** already at
`opt-level = 2`. **Tiny** base is `opt-level = "z"` with `#[optimize(speed)]`
overrides.

| Benchmark | Release (O2) | Tiny (Oz+73 opt) | Tiny vs Release |
|-----------|-------------|-------------------|-----------------|
| render | 3,324 fps | 1,791 fps | 53.9% |
| eval | 442,040/s | 284,340/s | 64.3% |
| aggregate | 579/s | 369/s | 63.7% |
| Binary size | 2,530 KB | 2,153 KB | 85.1% |

### Binary size breakdown (stripped)

| Configuration | Tiny |
|--------------|------|
| No annotations | 1,788,592 B |
| +21 hot functions | 1,803,272 B (+14 KB) |
| +73 hot functions | 1,803,272 B (same — 21→73 adds no measurable size) |
| +366 functions | 1,838,336 B (+50 KB) |

73 annotations hit the sweet spot: +14 KB stripped for +3% eval in release,
+3% eval in tiny, with no render regression.

---

## Nightly toolchain

`#[optimize(speed)]` requires nightly and the `optimize_attribute` feature:

```rust
// src/lib.rs
#![feature(optimize_attribute)]
```

Build with:
```bash
cargo +nightly build --release
cargo +nightly build --profile tiny
```

To verify annotations are active:
```bash
grep -c 'optimize(speed)' src/**/*.rs
```
