# rustxWidgets subtree workflow

`rustxWidgets/` is vendored into this repo via **`git subtree`** — it is a
plain directory of tracked files (no submodule, no nested `.git`), synced
with the upstream repo `gmatht/rustxWidgets` (canonical local clone:
`~ai_pir/rustxWidgets-canonical`, GitHub once push credentials are
available). Both pir and corro vendor the same canonical history.

## One-time state

* canonical `main` = `d70a445` (contains corro's ratatui/headless/
  pancurses_draw convergence work **and** pir's loader markup/timeout API +
  the `pump_events` cfg fix; the two previously diverged lineages were
  reconciled by `git merge -X subtree=rustxWidgets corro/main`).
* pir wired with `git subtree add --prefix=rustxWidgets rustxwidgets main`
  (commit `026d3bf`, no `--squash`, so the full widget history is preserved
  and future merges are minimal diffs).
* corro wired with `git subtree merge --prefix=rustxWidgets rustxwidgets/main`
  (branch `rustx-subtree`, commit `a9aa976`) — a clean 3-way that applied
  only the 316/19-line pir delta onto corro's vendored copy.

## Everyday commands

```sh
# pull upstream changes into the vend copy
git subtree pull --prefix=rustxWidgets rustxwidgets main

# commit code INSIDE rustxWidgets/ as usual (any commit works), then push back:
git subtree push --prefix=rustxWidgets rustxwidgets main
#   (--squ variant: push --prefix... --squash; see git-subtree(1))

# see which upstream commits are in the subtree
git log --oneline rustxWidgets/   # regular commits touching the prefix
```

Add the remote once per clone: `git remote add rustxwidgets <url>`
(currently `/home/ai_pir/rustxWidgets-canonical`; replace with
`https://github.com/gmatht/rustxWidgets.git` when push works).

## Rules of thumb

* **Edit `rustxWidgets/**` files in place** in whichever repo you're working
  in; subtree push/pull moves the changes, not the working copy.
* Prefer committing widget changes *separately* from host-repo changes so
  `subtree push` produces clean upstream commits.
* Don't add files inside `rustxWidgets/` that belong to pir — the subtree
  push would try to publish them upstream.
* Keep `Cargo.toml`'s `rustxwidgets = { path = "rustxWidgets/rustxwidgets",
  optional = true }` — the subtree satisfies the path dep, so fresh
  worktrees/clones build without any sibling checkout. The old
  deploy.sh "materialize external path deps" step is now only a fallback.
* The standalone clone used as the subtree remote lives outside the repo
  (`.rustxWidgets-upstream/` is gitignored); it is a normal clone you can
  `git pull` in, and it must not be referenced by tracked files.

## corro

Same workflow, prefix `rustxWidgets` (capital W, same as here). corro's
branch `rustx-subtree` (clone at `~ai_pir/corro-subtree`) holds the initial
subtree merge; fast-forward the root-owned checkout to it if it hasn't been
already:

```sh
git -C /root/src/corro fetch /home/ai_pir/corro-subtree rustx-subtree
git -C /root/src/corro merge --ff-only FETCH_HEAD
```

## Feature notes

* rustxwidgets builds: default (gtk-dlopen), `ratatui`, `headless`,
  `pancurses`, `gtk4-rs` (vendored sys crates need their own fixes).
* `pump_events` is a real GTK pump under `gui` and a no-op elsewhere, so
  pancurses/ratatui-only builds compile (fixed in `d70a445`).
