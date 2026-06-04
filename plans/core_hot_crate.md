# `corro-core-hot` Crate Proposal

## Goal

Extract the performance-critical, UI-independent code into a separate workspace crate (`corro-core-hot`) compiled at `opt-level = 2` so hot paths get consistent O2 treatment independent of the rest of the workspace profile settings.

## What goes in

The crate contains only pure computation — no terminal, no file watching, no XML/ODS, no zip:

| Module | Lines | Why hot |
|--------|-------|---------|
| `formula/` (mod + functions + number) | ~8,400 | Recursive AST eval, range iteration, builtin dispatch — hottest path in the app |
| `grid/` | ~2,100 | Sparse HashMap-backed cell storage, sorted rows, iter_nonempty — accessed on every render |
| `agg/` | ~260 | Range-iteration for SUM/MEAN/MEDIAN/MIN/MAX/COUNT — fires on every scroll with margin aggregates |
| `ops/` | ~3,500 | `Op::apply` (giant match), log parsing, workbook state — replayed during load and sync |
| `addr/` | ~700 | Cell ref parsing, Excel-column conversion — called during formula eval and log replay |
| `celladdr/` | ~200 | CellRef, RowRegion, ColRegion types |
| `export/` | ~2,700 | TSV/CSV/ASCII matrix generation with per-cell rendering |
| `extrapolate/` | ~560 | Fill-range logic |
| `balance/` | ~610 | Book balancing (calls formula eval) |
| **Total** | **~19,000** | |

These modules share only lightweight external dependencies: `serde` (grid serialization), `chrono` / `num-traits` (date functions in formula), `num-*` family (bigint, rational, complex for Number type), `balance-core` (already a sub-crate).

## What stays out

| Module | Reason excluded |
|--------|----------------|
| `ui/` | Ratatui + crossterm + unicode-* — not computation |
| `io/` | `notify` crate + file I/O — not computation |
| `ods/` | `quick-xml` + `zip` — format conversion, not hot |
| `core/` | Thin glue between ops/ui — negligible CPU |

## Dependency graph after extraction

```
corro (main binary)
  ├── corro-core-hot   (O2, no UI deps)
  │     ├── serde
  │     ├── chrono, num-traits, num-bigint, num-rational, num-complex
  │     └── balance-core
  ├── ratatui, crossterm, unicode-* (UI deps)
  ├── notify, zip, quick-xml (I/O deps)
  └── balance-core
```

## Cargo.toml changes

```toml
# workspace member
[workspace]
members = ["balance-core", "corro-core-hot"]

# new crate
[package]
name = "corro-core-hot"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
chrono = { version = "0.4", default-features = false, features = ["clock"] }
num-bigint = "0.4"
num-rational = { version = "0.4", default-features = false, features = ["std", "num-bigint"] }
num-traits = "0.2"
num-complex = "0.4"
balance-core = { path = "../balance-core" }

# In main Cargo.toml, replace local path deps:
corro-core-hot = { path = "corro-core-hot" }

# In profile.release, set O2 for the hot crate:
[profile.release.package.corro-core-hot]
opt-level = 2
```

## Migration strategy

**Phase 1 — Create crate skeleton** (this PR):
1. `cargo new corro-core-hot --lib` under workspace
2. Copy `formula/`, `grid/`, `agg/`, `ops/`, `addr/`, `celladdr/`, `export/`, `extrapolate/`, `balance/` into it
3. Fix import paths (`crate::` → `corro_core_hot::` or no-op for local use)
4. Strip `use crate::ui` references from `export.rs` (the tsv_effective/format_cell_display calls — inline or move those helpers into export)
5. Add `balance-core` dep
6. Verify `cargo test -p corro-core-hot` passes

**Phase 2 — Re-export from main crate**:
- `src/lib.rs` becomes a thin re-export: `pub use corro_core_hot::{formula, grid, agg, ops, addr, celladdr, export, extrapolate, balance};`
- Files that stay in `corro` crate: `ui/`, `io/`, `ods/`, `core/`, `main.rs`, `lib.rs`
- All existing `use crate::formula`, `use crate::grid`, etc. in remaining modules continue to work via re-export

**Phase 3 — Profile & verify**:
- Run `pgo_mix_benchmark --bench eval` before and after — throughput should be identical or better
- Run `cargo test` — all existing tests pass unchanged
- Build size comparison

## Expected outcome

- Hot code compiled at consistent `opt-level = 2` regardless of top-level profile override
- Cleaner separation of concerns: the hot crate has no knowledge of ratatui, crossterm, or terminal concepts
- Independent test suite for core logic (faster iteration on formula/grid bugs)
- Potential for independent PGO profiling of just the core crate
