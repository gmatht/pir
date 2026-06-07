---
name: Absolute Fill References
overview: Implement relative/absolute A1 reference translation for formula duplication across all copy/fill paths, and add a compact single-line fill log form like `FILL B1:B9=A1`.
todos:
  - id: addr-abs-ref
    content: Implement `$`-aware A1 reference parsing and round-trip rendering support.
    status: pending
  - id: formula-lock-translate
    content: Make formula translation honor row/column locks during copy offset rewrites.
    status: pending
  - id: copy-path-unification
    content: Apply formula translation consistently in CopyFromTo and FillRange execution paths.
    status: pending
  - id: compact-fill-syntax
    content: Add `FILL <range>=<expr>` parse support and optional compact serialization.
    status: pending
  - id: tests
    content: Add regression tests for parser, translation, and op parse/apply behaviors.
    status: pending
isProject: false
---

# Add Relative/Absolute Fill Semantics and Compact FILL Syntax

## Goal
Make formula duplication behavior spreadsheet-like across all copy operations: plain refs (`A1`) shift relative to destination; `$A1`, `A$1`, `$A$1` lock column/row as expected. Also support compact log syntax for repeated fills, e.g. `FILL B1:B9=A1`.

## Current State
- Formula references are parsed in [`D:/GitHub/corro/src/addr.rs`](D:/GitHub/corro/src/addr.rs) and AST/evaluation/translation in [`D:/GitHub/corro/src/formula/mod.rs`](D:/GitHub/corro/src/formula/mod.rs).
- Existing translator `translate_formula_text_by_offset` already shifts refs but has no absolute-marker support.
- Fill/copy entry points are handled through ops in [`D:/GitHub/corro/src/ops/mod.rs`](D:/GitHub/corro/src/ops/mod.rs), with UI producers in [`D:/GitHub/corro/src/ui/mod.rs`](D:/GitHub/corro/src/ui/mod.rs).
- Log parser currently accepts `FILL A1=... B1=...` token pairs only.

## Implementation Plan
1. **Add absolute reference model in parser/address layer**
   - Extend cell-ref parsing in [`D:/GitHub/corro/src/addr.rs`](D:/GitHub/corro/src/addr.rs) to recognize optional `$` before column and/or row for main A1-style refs.
   - Introduce a reference structure for formula translation/rendering that preserves lock flags (column_locked, row_locked) so `$` survives round-trip.
   - Keep existing `$sheetId:...` sheet-qualified syntax unambiguous by parsing sheet-qualified refs before A1 refs (current flow already supports this order).

2. **Apply lock-aware translation in formula rewriting**
   - Update translation/rendering in [`D:/GitHub/corro/src/formula/mod.rs`](D:/GitHub/corro/src/formula/mod.rs) so row/col deltas are skipped when corresponding lock flags are set.
   - Ensure ranges (`A1:B2`) carry lock info per endpoint and translate endpoint-wise.
   - Add parser/renderer tests for:
     - `=A1` shifts both axes
     - `=$A1` shifts row only
     - `=A$1` shifts col only
     - `=$A$1` shifts neither

3. **Enforce behavior across all copy/duplication operations (Scope B)**
   - Centralize formula-copy rewriting in op-apply path(s) in [`D:/GitHub/corro/src/ops/mod.rs`](D:/GitHub/corro/src/ops/mod.rs), especially for:
     - `Op::CopyFromTo`
     - `Op::FillRange` where value is a formula
   - Use per-cell source→target delta when writing each destination formula so copy/paste/fill/mitosis/range-set all share consistent semantics.
   - Keep non-formula values unchanged.

4. **Add compact log syntax: `FILL <range>=<expr>`**
   - Extend `parse_op_text` in [`D:/GitHub/corro/src/ops/mod.rs`](D:/GitHub/corro/src/ops/mod.rs) to accept:
     - existing form: `FILL A1=v B1=v2 ...`
     - new compact form: `FILL B1:B9=A1` (or `FILL B1:B9==A1` when preserving leading `=` through log encoding, depending on current value-encoding rules)
   - Expand compact form into internal `Op::FillRange { cells }` entries at parse time.
   - Update `Op::to_log_line` policy:
     - emit compact form when a fill can be represented as one source expression over a contiguous range;
     - otherwise fall back to existing per-cell serialization.

5. **Validation and regression tests**
   - Add/extend tests in:
     - [`D:/GitHub/corro/src/addr.rs`](D:/GitHub/corro/src/addr.rs) for `$` ref parsing
     - [`D:/GitHub/corro/src/formula/mod.rs`](D:/GitHub/corro/src/formula/mod.rs) for lock-aware translation
     - [`D:/GitHub/corro/src/ops/mod.rs`](D:/GitHub/corro/src/ops/mod.rs) for compact `FILL` parse/serialize and apply behavior
   - Verify these workflows produce expected formulas:
     - drag/fill down/right
     - clipboard snapshot copy
     - TSV paste into multi-cell range
     - `SET A1:B2 ...` expansion path

## Notes
- This introduces Excel-like `$` semantics only for A1-style references; existing margin/header/footer addressing remains as-is.
- If compact `FILL` serialization is too risky for compatibility, we can parse compact form first and defer compact emission to a follow-up while still supporting manual one-line input immediately.