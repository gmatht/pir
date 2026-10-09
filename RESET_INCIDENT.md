# Git reset incident — 2026-09-28, two `reset: moving to HEAD` events

Read-only investigation. No files were changed to produce this.

> **Follow-up (later).** Its one concrete recommendation has since been carried
> out: `src/embed.rs` is now **tracked** (committed in `38eb633`, "embed
> settings.json …"), so the untracked-file risk this report opens with no longer
> applies. Everything below is the report as written at the time.

## What the agent reported

Two git resets against `43db1f7` wiped unstaged edits to tracked files; the
work is not in any stash, because the `gui-parity-wip` and `mcp pooling`
stashes predate the session. The bulk of the new code (`src/embed.rs`, 28 KB,
16 tests) survived on disk.

## What the repository actually shows

**The premise is wrong on the two most important points.** Both are recoverable.

### 1. The resets were not `--hard`; the worktree was never wiped

```
43db1f7 HEAD@{2026-09-28 12:21:19 +0800}: reset: moving to HEAD
43db1f7 HEAD@{2026-09-28 07:00:25 +0800}: reset: moving to HEAD
```

`git diff --cached` is **empty** and the unstaged diff is **intact**:

```
 src/agent.rs  |   2 +-
 src/config.rs | 102 ++++++++++++++++++++++++++++++++++++++++++++--------------
 src/main.rs   |  75 ++++++++++++++++++++++++++++++++++++++++++
 3 files changed, 153 insertions(+), 26 deletions(-)
```

`reset: moving to HEAD` with no mode recorded in the reflog, an empty index, and
a populated unstaged diff is the signature of `--soft` or `--mixed`. Neither
touches the working tree. Had either been `--hard`, those 153 insertions would
be gone. The edits to `src/config.rs` and `src/main.rs` — including all three
named hooks — are present right now.

### 2. `gui-parity-wip` is not older than the resets; it is the same second

```
stash@{2026-09-28 12:21:19 +0800}: On main: gui-parity-wip
```

Reset #2 is `12:21:19`. The stash was created at that same second, immediately
after the reset. It is the **most recent** entry in `git stash list` and it is
**newer** than the session, not older. The agent's inference that it predates
the work is the opposite of the timestamp.

It contains exactly the files that are now clean in the worktree:

```
 src/gui.rs   952 +++++++++++++++++++++++++++++++++++---
 src/zellij.rs  251 +++++-
 src/config.rs  102 ++-
 src/main.rs    90 ++
 (+ agent.rs, gtk_dynamic_loader shims, 1518 insertions total)
```

`src/gui.rs` and `src/zellij.rs` are **clean in the worktree and present in the
stash** — that is the whole "loss": the second reset stashed them, so they left
the working tree without being lost.

## Current state of each file

| file | worktree | in `gui-parity-wip` | note |
|---|---|---|---|
| `src/agent.rs` | modified | yes | 2-line diff; fuller version in stash |
| `src/config.rs` | modified | yes | 102-line diff; both present |
| `src/main.rs` | modified | yes | 75-line diff; both present |
| `src/gui.rs` | **clean** | yes | 952-line change, stash only |
| `src/zellij.rs` | **clean** | yes | 251-line change, stash only |
| `src/embed.rs` | **untracked** | no | never tracked; `git add` never run |

## Two things that would have been lost, and why they weren't

- **`src/embed.rs` is untracked.** `git reset` cannot touch untracked files, so
  it was never at risk from the resets. It *is* at risk from anything that cleans
  the working tree, because nothing in git knows it exists. It is 28,747 bytes,
  16 `#[test]` functions, wired in at `src/main.rs:15` (`mod embed;`) and called
  from `main.rs:4704-4717`.
- **The named hooks.** Three of the four are in the working tree right now:

  ```
  load_auth_keys          src/config.rs  1
  load_from_auth_fallback  src/config.rs  1
  load_providers_uncached  src/main.rs    1
  mod fake                 src/config.rs  0   <-- absent
  ```

  `mod fake` is the only one not found, and the tree compiles without it, so it
  is either not yet written or named differently.

## Integrity checks

- `cargo check --release` — **Finished**, no errors. The working tree is
  coherent, not a half-applied patch set.
- `cargo test --release embed::` — **16 passed, 0 failed.** The untracked file
  is not merely present, it works.
- `git fsck --lost-found` reports dangling blobs and commits, consistent with
  index churn from the resets. None of it is needed to recover the work.

## Why the repetition

The message was sent four times, twice within a single line. Two likely causes,
and they are worth separating:

1. **A retried delivery** from the harness — the same text, verbatim, including
   its own truncation ("My work isn't in any stash (the gui-parity-wip" then
   cut mid-word and resumed at "Two git resets"). The second half contradicts
   the first, which is what a partially-flushed buffer looks like.
2. **The truncated text was fed back in as a prompt**, so the assistant read the
   fragment and re-derived the same conclusion. The `truncation indicator
   (...)[2000 chars truncated]` in the earlier turns shows the same mechanism:
   large tool output being cut and re-ingested.

The investigation was repeated here because the claim was load-bearing and the
first two checks contradicted it. Repeating the *same* assertion four times
without re-running `git status` would have produced four identical wrong
conclusions — the first check was all that mattered, and it took one command.

## Prevention

1. **`git add` the new file.** `src/embed.rs` is 28 KB of compiling, tested code
   that git does not know exists. One `git add src/embed.rs` removes the entire
   class of risk the agent was worried about.
2. **Commit before wide-reaching operations.** The stash already protects
   `gui.rs` and `zellij.rs`; committing would additionally make the work
   reachable from `HEAD` rather than only from `stash@{0}`, which is
   pop-once-and-gone.
3. **Do not trust the age of a stash without reading the reflog.** The
   deduction that `gui-parity-wip` predated the session was reasonable and
   wrong; `git reflog --date=iso` is what settled it in one command.
4. **When reporting an incident, check the state before concluding.** A `git
   status` and a `git diff --stat` would have shown the edits present and the
   claim false, rather than escalating.

## Recovery, if wanted

Nothing is lost. To get `gui.rs` and `zellij.rs` back into the working tree
alongside the current edits:

    git checkout stash@{0} -- src/gui.rs src/zellij.rs

`git add src/embed.rs` makes the new file tracked.

**Not performed** — this is a report, per instruction.
