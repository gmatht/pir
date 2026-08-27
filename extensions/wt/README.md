# wt — per-agent git worktrees with idle verify + merge-back

An extension for `pir` that gives each agent its own linked git worktree, then
verifies (build + test) the branch on idle and merges it back into the trunk
when green — with an inter-agent lock so multiple agents never merge
concurrently.

Off by default. Enable with `PIR_WT=1`.

## What it does

* `wt_create` — creates a linked worktree off the repo's **trunk** (auto-detected:
  `origin/HEAD`, else the checked-out branch, else `main`/`master`/`trunk`/`develop`)
  with a fresh branch and `cd`s the agent into it, so subsequent `bash` /
  `edit_file` / `read_file` calls operate in the worktree, not the trunk checkout.
* After every user turn (`on_turn_end`), if the agent is sitting in a `wt`
  worktree and auto-merge is on (`PIR_WT_AUTO`, default on with `PIR_WT`):
  1. Best-effort fast-forward the trunk to `origin/<trunk>`.
  2. Pull any upstream into the worktree's branch.
  3. Run project-type build + test checks (see *Verification* below).
  4. If checks pass: merge the branch into the trunk from the trunk checkout
     under the repo merge lock, then remove the worktree.
  5. If checks fail: queue a follow-up prompt asking the model to fix the
     failure (the REPL runs it as the next turn), and do **not** merge. Retried
     at most `MAX_FIX_ATTEMPTS` (2) times; after that it stops asking and leaves
     the branch for manual resolution.
  6. If there is **no verification command** configured or recognized (no
     `PIR_WT_CHECK` and no `Cargo.toml`/`package.json`/`pyproject.toml`/`Makefile`),
     the extension does **not** claim success — it skips auto-merge and tells you
     to set `PIR_WT_CHECK` or run `wt_merge` explicitly. This avoids silently
     merging unverified work.
* `wt_verify` / `wt_merge` / `wt_status` / `wt_remove` — explicit control when
  not running in full-auto or when you want to manage the worktree manually.

On session exit (`on_exit`) the worktree is removed (force) so nothing is left
dangling.

## Locking

A repo-wide lock serializes auto-merges: `<repo>/.git/wt-merge.lock`, taken with
`flock -x -w 0` (a `flock`-held `sleep` child that is killed on drop). A second
agent — or the same agent in a different worktree — that tries to merge while
the lock is held simply skips its auto-merge (it logs a note and waits for a
later turn). Merges are done only from the trunk checkout, and the trunk is
fast-forwarded (never rewritten) before the `--no-ff` merge of the branch.

## Verification (project-type aware)

The check command is chosen from the worktree layout unless `PIR_WT_CHECK`
overrides it:

* `Cargo.toml` present → `cargo build --locked && cargo test --locked`
* `package.json` → `npm ci && npm run build && npm test`
* `pyproject.toml` / `setup.py` → `python -m build && python -m pytest`
* `Makefile` → `make && make test`
* nothing recognized → **not** auto-merged (see step 6 above).

Set `PIR_WT_CHECK` to a shell command to pin verification explicitly.

## Environment

* `PIR_WT=1` — enable the extension.
* `PIR_WT_AUTO=0` — create worktrees but don't auto-verify/merge on idle
  (use the `wt_*` tools manually).
* `PIR_WT_DIR=<path>` — where worktrees live (default `<repo>/.git/wt`, i.e.
  inside `.git`, so they're never seen by the main working tree).
* `PIR_WT_CHECK=<shell cmd>` — explicit verify command.

## Notes

* Worktrees live under `.git/wt/...`, which is inside `.git`, so they don't
  dirty the trunk working tree and aren't committed.
* The branch defaults to `wt-<pid>-<epoch>` (or `<base>-wt-<pid>-<epoch>` when a
  `base` is given to `wt_create`).
* Requires git and `flock` (present on Linux/macOS).

## Tools

| Tool | Purpose |
|------|---------|
| `wt_create` | create worktree off trunk + branch, cd into it |
| `wt_verify` | run build/test checks for the current worktree |
| `wt_merge`  | merge current branch into trunk (under lock) + remove worktree |
| `wt_status` | report current worktree / branch / trunk checkout |
| `wt_remove` | remove worktree + branch (abandon without merging) |
