**Remove Option From Grid**

Summary
1. Replace the current mixed semantics where Grid.get returns Option<&str> (None for absent main/ margins, Some("") for dense header/footer empty cells) with a consistent API that always returns text and treats absent cells as empty string "" from the user-facing API.
2. Do this in phases: a safe, non-breaking Phase 1 (add a non-Option accessor and migrate callers), a Phase 2 breaking change (make the trait / canonical API non-Option), and an optional Phase 3 to optimise storage (dense/ chunked hybrid) once the API is stable.

Goals
1. Uniform public API: callers never see Option for a cell value; missing and empty cells are equivalent ("").
2. Small, reviewable commits that keep CI green and let us roll back easily.
3. Preserve behaviour and avoid UI regressions.

Non-Goals (for this migration)
1. Major storage refactors (dense/ chunked) — these are optional follow-ups after the API work.
2. Performance micro-optimisations beyond obvious low-risk changes.

Phase 1: Non-breaking accessor (recommended first step)
1. Add a default helper to the GridImpl trait:

```rust
// inside src/grid/mod.rs GridImpl trait
fn get_str<'a>(&'a self, addr: &CellAddr) -> std::borrow::Cow<'a, str> {
    self.get(addr).unwrap_or(std::borrow::Cow::Borrowed(""))
}
```

2. Add a concrete convenience method on the concrete Grid type:

```rust
// inside impl Grid
pub fn get_str<'a>(&'a self, addr: &CellAddr) -> std::borrow::Cow<'a, str> {
    self.get(addr).map(Cow::Borrowed).unwrap_or(Cow::Borrowed(""))
}
```

3. Add GridBox wrappers:

```rust
impl GridBox {
    pub fn get_str(&self, addr: &CellAddr) -> std::borrow::Cow<'_, str> {
        self.inner.get_str(addr)
    }
    pub fn get_owned_str(&self, addr: &CellAddr) -> String {
        self.inner.get_str(addr).into_owned()
    }
}
```

4. Migrate call sites to the new accessor. Typical replacements:

 - `grid.get(&addr).unwrap_or("")` -> `grid.get_str(&addr)`
 - `if let Some(v) = grid.get(&addr) { ... }` -> `let v = grid.get_str(&addr); if !v.is_empty() { ... }`
 - `assert_eq!(grid.get(&addr), None)` -> `assert_eq!(grid.get_str(&addr), "")`

5. Files to update (search & migrate). Use ripgrep to find call sites:

 - Run: `rg "\bget\(&CellAddr" -n src` and `rg "grid\.get\(" -n src`
 - Expected hotspots: src/grid/mod.rs, src/ui/mod.rs, src/export.rs, src/ops/mod.rs, src/io/mod.rs, src/ods.rs and tests.

6. Tests & CI:
 - Run `cargo test` frequently. Fix tests that expect Option::None by changing them to assert empty string where appropriate.
 - Update UI code that used pattern matching on Option to handle empty-string semantics.

7. Back-compat strategy: keep the old `get` (Option-returning) during this phase. Optionally annotate it with a `#[deprecated]` note to discourage new usage once most callers migrated.

Phase 1 Acceptance Criteria
1. All call sites have been migrated to use `get_str` (or equivalent wrapper calls).
2. `cargo test` passes and the UI behaves identically.
3. No runtime panics or obvious performance regressions.

Phase 2: Replace Option-returning API (breaking change)
1. Make the trait canonical: change `GridImpl::get` signature to return `Cow<'a, str>` (non-Option) and remove the Option-returning `get`.
2. Rename `get_str` -> `get` across trait, Grid impl, and GridBox so the public API is concise.
3. Update all call sites to call `get` (no unwrap_or needed) and remove `get_str` helpers.
4. Update and run full test suite, fix remaining issues.

Phase 2 Acceptance Criteria
1. Trait and types use the non-Option `get` everywhere.
2. All tests pass and code compiling cleanly.

Phase 3 (Optional): Storage optimisations
Rationale: the repository currently stores main cells as HashMap<(u32,u32), String>. For typical small-and-dense use this is suboptimal; for very large sparse sheets the HashMap approach is fine. Implementing a hybrid storage gives best-of-both-worlds.

Outline design (hybrid MainStorage)
1. Introduce enum MainStorage:

```rust
enum MainStorage {
    Sparse(std::collections::HashMap<(u32,u32), String>),
    Dense { rows: usize, cols: usize, data: Vec<String> },
    Chunked { shift: u8, map: HashMap<u64, Box<[String]>> },
}
```

2. Behaviour:
 - Start as Dense when `rows * cols` is small (configurable threshold), otherwise start as Sparse.
 - If we need to expand a dense grid beyond threshold, convert to Chunked or Sparse.
 - Chunk size: prefer 16×16 or 32×32 (default 16). Keys: pack chunk coords into u64: `((cx as u64) << 32) | (cy as u64)`.

3. Implementation steps:
 - Add MainStorage enum and a thin adapter API on Grid: `get_main`, `set_main`, `retain_main`, `iter_main_nonempty`.
 - Implement conversion helpers `promote_to_dense`, `demote_to_sparse`, `split_to_chunks`.
 - Update all places that read/write main_cells to go through the new adapter.

4. Tests & benchmarks:
 - Add microbenchmarks for point lookup, neighbor queries, and area scans.
 - Measure memory overhead and hot-path latencies for Dense vs Sparse vs Chunked.

Search & Replace Guidance
1. Grep patterns to find callers:
 - `rg "\bget\(&CellAddr" -n src`
 - `rg "grid\.get\(" -n src`
 - `rg "\.get\(&CellAddr::Main|Header|Footer|Left|Right" -n src`

2. Common replacement examples:

 - `grid.get(&CellAddr::Main { row: r, col: c }).unwrap_or("")` -> `grid.get_str(&CellAddr::Main { row: r, col: c })`
 - `if let Some(s) = grid.get(&addr)` -> `let s = grid.get_str(&addr); if !s.is_empty()`

Verification & Test Plan
1. Run unit tests: `cargo test`.
2. Run UI/acceptance smoke tests: open the app and exercise typical flows (editing cells, saving/loading, sorting). The UI should show empty cells unchanged.
3. Run the following microbenchmarks (create small benches or quick loops):
 - Point lookup hot loop (random and sequential)
 - Neighbor queries across many positions
 - Area scans: small rectangle vs large rectangle
4. Memory checks: inspect RSS on large sparse datasets.

Risks & Mitigations
1. Breakage across many call sites: mitigate by Phase 1 non-breaking approach.
2. Tests expecting None: update tests carefully and use deprecation to find remaining usages.
3. Performance regressions: run microbenchmarks and prefer Cow<'a, str> to avoid allocations where possible.

Timeline & Estimates (rough)
1. Phase 1: 2–6 hours — add trait helper, Grid/GridBox helpers, migrate straightforward call sites, fix tests.
2. Phase 2: 1–3 hours — change trait signature (breaking), update impls and call sites, run tests.
3. Phase 3 (optional): 4–16 hours — design & implement MainStorage, conversion helpers, tests and benchmarks. Amount varies with scope.

Commit Message Suggestions
1. Phase 1 commit: `grid: add non-breaking get_str accessor; migrate callers to treat missing cells as ""`
2. Phase 2 commit: `grid: change GridImpl::get to return Cow<str> (non-Option); remove Option-based getter`
3. Phase 3 commit: `grid: introduce MainStorage hybrid (Dense/Sparse/Chunked) for main region`

Rollout & Revert Plan
1. Implement Phase 1 on a feature branch, open a PR, and run CI.
2. Merge only when tests & manual smoke pass.
3. Implement Phase 2 in a follow-up PR after all call sites migrated and the team is comfortable with the change.
4. To revert: `git revert` the merge commit for the PR if issues are found in master.

Checklist (concrete todos)
1. Add `get_str` default to GridImpl and helpers on Grid and GridBox. (high)
2. Migrate src/grid/mod.rs internals to use get_str where appropriate. (high)
3. Migrate tests in src/grid/mod.rs to assert empty string where they expected None. (high)
4. Run `rg` to find remaining call sites and migrate them incrementally, updating tests as you go. (high)
5. After migration, mark old `get` as `#[deprecated]` and leave it for one release cycle. (medium)
6. Phase 2: change trait signature and remove old API (medium).
7. Phase 3: design and implement MainStorage (optional, low->medium).

Follow-ups
1. Consider adding a small lint or clippy rule to flag uses of `grid.get(...).unwrap_or("")` so migration is easier to track.
2. Add microbenchmarks in `benches/` to measure point/neighbor/area performance before/after storage changes.

Appendix: Example replacements (not applied)

From:

```rust
let v = grid.get(&CellAddr::Main { row: r, col: c }).unwrap_or("");
```

To:

```rust
let v = grid.get_str(&CellAddr::Main { row: r, col: c });
```

From:

```rust
if let Some(val) = grid.get(&addr) {
    do_something(val);
}
```

To:

```rust
let val = grid.get_str(&addr);
if !val.is_empty() {
    do_something(val.as_ref());
}
```

End
