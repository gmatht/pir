# Incremental (in-place) Markdown Rendering

Agent replies are streamed token-by-token over SSE. Rather than dump the raw
markdown (`**`, `#`, backticks) or wait for the whole turn to finish and render
once, `pir`:

1. **Renders the reply as formatted markdown** using a comrak-based terminal
   renderer (`src/md.rs`): headings show a `#` marker and bold, emphasis/strong
   become ANSI bold/italic, lists keep their bullets, code blocks are fenced,
   and links show their text with the URL in brackets.
2. **Redraws the block in place** as the reply streams. On each throttled
   redraw it jumps the cursor back to the top of the block it drew last and
   overwrites it with the fresh render — so the user watches the formatted
   markdown *grow* rather than stare at a spinner, and the same lines are never
   stacked or duplicated on screen.

## Behaviour

- **On by default** when the process is a tty and the turn is not quiet.
- **Throttled** to at most one redraw per **200&nbsp;ms**, so a fast token
  firehose can't saturate the terminal with a render per byte. The final state
  is always flushed at turn boundaries, so the full reply shows exactly once.
- Turning it **off** (`--no-incremental` or `PIR_INCREMENTAL_MD=0`) makes `pir`
  fall back to rendering the finished reply once at the end.

## Configuration

| Switch | Effect |
|---|---|
| `--no-incremental` | Disable live (in-place) streaming markdown; render once at the end. |
| `PIR_INCREMENTAL_MD=0` | Same, via environment. |
| `PIR_INCREMENTAL_MD_THROTTLE_MS` | Override the redraw throttle window in milliseconds (default `200`). The renderer never redraws more often than this; the final state is always flushed at turn boundaries. Values `< 1` or non-numeric fall back to `200`. |
| Anything else | Incremental rendering stays on (default). |

The default window (`DEFAULT_THROTTLE_MS = 200`), the env-var override, and the
`IncrementalMarkdown::throttle_ms` field live in `src/md.rs`.

## Implementation

- `md::render(md, color)` — stateless comrak → terminal render.
- `md::IncrementalMarkdown` — tracks the accumulated `pending` markdown,
  re-renders whole each time it fires, and emits a frame that starts with
  `\x1b[<n>A\x1b[J` (jump back over the previous block, erase to end of screen)
  so the next draw always overwrites, never appends. `flush()` forces a final
  draw at turn boundaries; when disabled it's a no-op but still accumulates
  `pending()` so the caller can do one final render.
- `Agent` wires it into the streaming REPL path: `use_incremental =
  !silent() && tty && incremental_md`.
- Persistent on/off choice is stored per-session in `<session>.incmd`.

## Tests

- Unit tests in `src/md.rs` (throttle batching, jump-back overwrite, disabled
  no-op, idempotent re-render).
- tmux screen-grab integration tests in `tests/incremental_tmux.rs` prove a
  *real* `pir` in a pane draws markdown formatted and exactly once (no stacking)
  with incremental on, and that `--no-incremental` defers the draw to the end.
