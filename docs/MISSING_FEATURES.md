# Missing Features: `pi` → `pir`

`pir` is a deliberate, featherweight reimplementation of `pi` in Rust. It already
covers the core loop (streaming tool-use against Anthropic + OpenAI-compatible
providers), resumable sessions, persistent goals, background jobs, the `wt`
worktree extension, per-project sandbox users, and an `su`-based local authority
toggle. This document lists the features `pi` has that `pir` does not, ordered by
recommended implementation priority (value-to-effort, then how much the omission
hurts real coding work).

> **Runtime note.** `pir` is *not* fully synchronous — it links `smol` and uses
> `smol::channel` for the background-job / model-broadcast machinery (`src/main.rs`).
> The core tool loop in `agent.rs` is a plain synchronous function (tools run in
> matching `match` arms), and the IPC bridge design below uses `smol` channels
> exactly like that existing machinery. This matters for the extension design
> (#21): a TS bridge `confirm` is a synchronous request/response over IPC, which
> has the *same* concurrency contract as `pir`'s native `bash`/`edit_file` tools
> (they block the turn too), so no capability is lost by going through IPC.

Each item notes: what `pi` does, why it matters, rough effort, and where it would
live in `pir`. "L" = low, "M" = medium, "H" = high effort.

---

## 1. Thinking / reasoning levels — `/thinking` (`-L`/`--thinking`)
**Priority: highest.** Effort: **L**.

`pi` exposes `off | minimal | low | medium | high | xhigh | max` (and per-model
budgets) via `/thinking`, `/model …:high`, and `defaultThinkingLevel`. Extended
thinking drastically changes capability on Claude/OpenAI/Google — `pir` currently
sends no thinking control at all, so it silently underperforms the same model.

Implementation: add `--thinking <level>` and `PIR_THINKING`; thread a
`thinking` field through `config` → `Agent` → `provider`. For Anthropic, set
`thinking { type: "enabled", budget_tokens }` and forward `thinking_delta`; for
OpenAI-compatible, use `reasoning_effort` / `thinkingTokenBudgetField`; for Google,
native thinking budget. Map levels → token budgets in `config.rs`.

---

## 2. Google / Gemini + broad multi-provider catalog
**Priority: high.** Effort: **M**.

`pi` ships ~40 providers (Anthropic, OpenAI, Google Gemini, Vertex, Bedrock,
Mistral, Groq, DeepSeek, xAI, OpenRouter, …) via `packages/ai`. `pir` speaks only
Anthropic + OpenAI-compatible (`ApiKind`). Gemini is a common, cheap, strong model
family that `pir` cannot use at all because it has a distinct request/response
shape (the `google-shared` / `google-generative-ai` / `google-vertex` APIs).

Implementation: add an `ApiKind::Google` to `provider.rs`
(`chat`/`anthropic_request`/`openai_request` plus `google` +
`stream_google`), let `load_providers` accept a `kind`/`apiStyle` field in
`models.json` (defaulting to anthropic/openai). Keep the `{env:VAR}` key
expansion already in `config.rs`.

---

## 3. `--print` / `-p` and `--mode json` event stream
**Priority: high.** Effort: **M** (print), **M** (json).

`pir` supports one-shot (`prompt` arg, REPL, `-bg`) but no clean non-interactive
exit-after-answer (`-p`) and no structured machine output. `pi --mode json`
emits a documented `JsonAgentSessionEvent` stream (agent_start … tool_start …
message_update deltas … agent_end) that other tools/UIs consume. `pi -p` also
merges piped stdin into the prompt.

Implementation: `-p` is a thin wrapper around the existing one-shot path that
suppresses the REPL and prints the final assistant text. JSON mode needs a
serializer in `agent.rs` emitting the same event taxonomy `pi` uses (reuse
`types.rs` message model; add a `usage` accumulator). Good prerequisite for #19.

---

## 4. Auto-compaction (context window management)
**Priority: high.** Effort: **M–H**.

`pi` auto-compacts when `contextTokens > window - reserveTokens` (default
reserve 16384, keepRecent 20000), and `/compact [instructions]` on demand. It
summarizes old turns into a structured `## Goal / Progress / …` block with
cumulative `<read-files>`/`<modified-files>` tracking, preserving recent turns.
Long `pir` sessions just run out of context and degrade.

Implementation: add token estimation (cheap heuristic on `Message::blocks`), a
`CompactionEntry` persisted in the JSONL, and a `compact()` that calls the model
with `serializeConversation`-style text (reuse the existing prompt-builder
style). Wire `agent_end`-style threshold check before each model call, gated by
`compaction.enabled` in settings. This is the single biggest "long-running task"
gap. Higher value than most UI features because `pir` already does multi-turn
tasks but loses coherence over time.

---

## 5. Distinct `grep` / `find` / `ls` tools (safe built-ins)
**Priority: medium-high.** Effort: **L**.

`pi` ships `grep`, `find`, `ls` as separate, sandboxed, read-only tools so the
model reads/search the tree without shelling out. `pir` only has `bash`,
`read_file`, `write_file`, `edit_file`, `list_dir`. Bare `bash` to grep/find is
fine but costs a shell spawn per call and is coarser-grained for any future
permission policy.

Implementation: add three `ToolSpec`s to `extensions/builtin` (ripgrep-style
`grep`, `find`, and an `ls` alias for `list_dir`, or fold into `list_dir`). Low
risk; improves parity and is the natural hook for the permission model (#17).

---

## 6. Skills (Agent Skills spec)
**Priority: medium.** Effort: **M**.

`pi` loads `SKILL.md` files (directories or root `.md` with frontmatter) from
`~/.pi/agent/skills/`, `~/.agents/skills/`, `.pi/skills/`, extensions, and
settings; exposes them as `/skill:name` and progressive-disclosure in the system
prompt. Skills are the main way `pi` users add capability without forking.

Implementation: a `skills` extension that scans the standard locations (respecting
project-trust from #17), parses frontmatter (`name`/`description`/etc.), injects
descriptions into the system prompt, and adds a `read_skill`/`/skill:name`
command. Reuses the existing `ToolBackend`/`registerCommand` patterns.

---

## 7. Prompt templates
**Priority: medium.** Effort: **L–M**.

`pi` expands `~/.pi/agent/prompts/*.md` (and project/package/CLI) as `/name`
commands, with positional args `$1`, `$@`, `${1:-default}`, slicing. Natural
companion to #6.

Implementation: a `prompt-templates` extension mirroring the discovery rules and
a small arg-substitution function. Can ship before skills.

---

## 8. Themes / TUI theming
**Priority: medium.** Effort: **M** (plain REPL); partially present in the TUI feature.

`pi` themes are JSON files with ~51 color tokens (dark/light built-ins + custom,
hot-reloaded). `pir`'s plain REPL uses `term.rs` color flags; the `tui` feature
(`ratatui`) has its own palette. A theme file would unify both and match `pi`.

Implementation: a `theme` extension reading `~/.pi/agent/themes/*.json` +
`.pi/themes/*.json`, mapping `pi`'s token names onto `term` ANSI helpers and the
`ratatui` style. Lower priority than capability gaps but cheap polish.

---

## 9. Session branching: `/tree`, `/fork`, `/clone`
**Priority: medium.** Effort: **M–H**.

`pi` stores sessions as a tree (every entry has `id`/`parentId`; a `BranchSummaryEntry`
is injected on `/tree` navigation). `/fork` starts a new session from a past user
message; `/clone` duplicates the active branch. `pir` stores linear JSONL with no
branch ids, so it cannot revisit a fork point.

Implementation: tag entries with `id`/`parentId` in the JSONL writer, add a
`/tree` navigator (cursor + filter modes already sketched in `pi`), and branch
summarization (see #4's summary format). This is real work; do it after compaction
since both serialize the same way.

---

## 10. `@`-file references, editor autocomplete, external editor
**Priority: medium.** Effort: **M** (TUI), **L** (plain REPL `@` merge).

`pi` fuzzy-finds project files with `@`, does path completion, opens an
external editor with Ctrl+G, and pastes images via Ctrl+V. `pir` takes a plain
prompt line.

Implementation: plain REPL can expand leading `@path`/`@file` args into the user
message now (low effort). Full editor autocomplete + external-editor launch
belongs to the `tui` feature; add a small input component with path completion and
a `ctrl-g` → `$VISUAL`/`$EDITOR`/`nano` spawn.

---

## 11. Multimodal / image input
**Priority: medium.** Effort: **M**.

`pi` accepts images (`prompt` with `{type:"image", data, mimeType}`, `@screenshot.png`),
and renders inline images via Kitty/iTerm2 protocols. `pir`'s `Message` model is
text + tool blocks only; `provider.rs` never sends `image` content blocks.

Implementation: extend `types::Block` with `Image { data, mime }`, add an `image`
tool / `@image` input, and emit Anthropic `image` blocks. (Also a prerequisite for
pi's `image` built-in tool behavior.)

---

## 12. Steering & follow-up queues
**Priority: medium.** Effort: **M**.

`pi` lets you interrupt while tools run (`agent.steer`) and queue work after the
turn (`agent.followUp`), with `one-at-a-time` / `all` modes. `pir` only accepts
input when idle and has ESC/ctrl-c hard-cancel.

Implementation: a bounded steering queue in the REPL/agent; inject queued user
messages at safe points (turn boundary). Adds resilience for long unattended runs.

---

## 13. Extension hot-reload (`/reload`)
**Priority: low-medium.** Effort: **L–M**.

`pi` hot-reloads extensions/skills/prompts/themes with `/reload`. `pir`'s
extensions are compile-time linked (`build.rs`), so they cannot reload at runtime.

Implementation: `pir`'s whole extension model is static-link-by-design (no runtime
loader, no `Cargo.toml`); true hot-reload conflicts with that. Cheaper alternative:
a `/reload` that re-reads *data* extensions (skills #6, prompt templates #7, themes
#8) without recompiling. Note this trade-off explicitly — it is the main intentional
parity gap vs `pi`'s TypeScript extensions.

---

## 14. Packages: `pi install` / `pi update` (npm + git)
**Priority: low (for most users).** Effort: **H**.

`pi` installs extensions/skills/themes from npm (`npm:@foo/bar`) and git
(`git:github.com/...`) via `pi install`/`update`/`remove`/`list`/`config`. `pir`'s
"package" story is the statically-linked `extensions/` folder.

Implementation: a `packages` extension that clones/installs into
`~/.pi/agent/extensions` and recompiles `pir` (needs a build step) — heavy, and
conflicts with the static-link philosophy. Recommend *deferring*: document that
`pir` extensions are built from source, not fetched at runtime.

---

## 15. RPC mode + SDK
**Priority: low.** Effort: **H**.

`pi` embeds via `--mode rpc` and a JS SDK (`@earendil-works/pi-coding-agent`).
`pir` has no IPC surface. Only needed for process integration / embedding.

Implementation: requires a stable JSON event protocol (#3) plus a socket/stdio
RPC server wrapping `Agent`. Out of scope unless `pir` is embedded elsewhere.

---

## 16. Session export / share (`/export`, `/share`)
**Priority: low-medium.** Effort: **L–M**.

`pi` exports a session to HTML/JSONL (`/export`) and uploads a private gist
(`/share`). `pir` has the JSONL already on disk but no transform/export command.

Implementation: `/export` = render JSONL → HTML (markdown already partially
rendered in TUI); `/share` = a `gh`/`curl` wrapper to a gist. Low effort once #3
exists.

---

## 17. Project trust model (`trust.json`, `/trust`)
**Priority: medium (security).** Effort: **M**.

`pi` gates loading of project-local `.pi/settings.json`, `.pi/extensions`,
`.pi/skills`, etc. behind a per-directory trust decision in
`~/.pi/agent/trust.json`, with `defaultProjectTrust` (`ask`/`always`/`never`) and
`--approve`/`--no-approve`. `pir` reads `./AGENTS.md` unconditionally and has no
trust gate, so a repo can inject instructions/skills silently.

Implementation: add trust scan at startup (the same "requires trust" resource list
`pi` uses), prompt on first interactive use, persist to `trust.json`, honor
`--approve`/`--no-approve`. Ties into #6/#7 (only load project skills/templates
after trust).

---

## 18. Telemetry + `pi update --models` catalog refresh
**Priority: low.** Effort: **M**.

`pi` emits structured telemetry (`@earendil-works/pi-telemetry`) and refreshes its
provider/model catalog (`pi update --models`) since provider lists change. `pir`
relies on a static `models.json` the user maintains.

Implementation: a no-op-friendly telemetry hook at agent/turn boundaries (cheap;
can be local an optional `update_models` command that re-fetches a
provider catalog. Lower priority than correctness/capability gaps.

---

## 19. Usage footer: cache hit-rate, cost, reasoning level
**Priority: low.** Effort: **L**.

`pi`'s footer shows `↑ input ↓ output R cache-read W cache-write CH cache-hit`
and cost. `pir` tracks `Usage { input, output }` but does not surface cache
read/write, cache-hit rate, or cost in the TUI footer, and has no reasoning-level
display.

Implementation: extend `types::Usage` with `cacheRead`/`cacheWrite`, parse them
from provider responses (Anthropic `cache_creation`/`cache_read`), and render in
`term.rs`/`tui.rs`. Pairs with #1.

---

## 20. Parallel tool execution
**Priority: low-medium.** Effort: **M**.

`pi` runs allowed tool calls concurrently (`toolExecution: "parallel"`, default)
with `beforeToolCall`/`afterToolCall` hooks. `pir` executes tool batches
sequentially (one match arm at a time).

Implementation: spawn allowed tools concurrently after sequential preflight;
preserve ordering of `toolResult` messages per assistant source order. Mostly a
restructure in `agent.rs`; note `bash` side effects mean concurrency needs care
(keep `bash` sequential unless explicitly safe).

---

## 21. `pi`-style extensions via a TS IPC bridge (`pi-extensions`)
**Priority: high (unblocks #6–#14).** Effort: **M** (ABI + host) then **M** (TS bridge).

Running `pi`'s *actual* TypeScript extensions verbatim is not viable (it would
mean embedding a JS runtime as `pir`'s agent core — contradicts the 3-dep, no-big-
runtime charter). But `pir` can host a **translation layer**: extend `pir`'s Rust
`ToolBackend` ABI, then ship one Rust extension (`pi-extensions`) that brokers
`pi` extensions over IPC to per-extension TS bridge processes. This reaches the
~70% of `pi` extensions that matter (tools, commands, hooks, gates) without a JS
core.

**Step 1 — extend `pir`'s Rust ABI** (`src/plugin.rs`, `src/agent.rs`):
- a lifecycle **event bus**: `tool_call` pre-flight hook that may return
  `{block,reason}` (shared across backends), `turn_start`/`turn_end`,
  `session_start`, `agent_end`;
- `registerCommand(name, handler)` so a backend adds REPL slash commands;
- a minimal `ctx.ui` (`notify`, `confirm`, `status`) threaded to backends;
- `sessionManager`-lite (`appendEntry`/`getEntries`) over the JSONL + `.pir/`.

**Step 2 — the `pi-extensions` Rust backend** spawns, at session start, one TS
bridge (`bridge.ts`, run under `deno`/`bun`/`node`) per discovered extension in
`~/.pi/agent/extensions/` + `.pi/extensions/` (same trust-gated locations as `pi`,
so this also forces the project-trust work from #17). It speaks a line-framed
JSON protocol over each child's stdin/stdout (one JSON object per line; `smol`
channels on the Rust side). The TS side implements `pi`'s surface:

```
pi.on("tool_call", …)       → HOST→bridge "event"  → bridge returns "hook_result" {block,reason}
pi.registerTool({…})        → bridge→HOST "register_tool"  → host marshals into Registry
pi.registerCommand("name")  → bridge→HOST "register_cmd"   → REPL dispatches /name to bridge
ctx.ui.confirm(…)           → HOST→bridge "confirm" → host renders pir's y/a/n → returns bool
ctx.ui.notify(…)            → host prints dimmed line
ctx.sessionManager.appendEntry → host writes into the JSONL the bridge owns
```

`tool_call`/`registerTool`/`registerCommand`/`ctx.ui`/`sessionManager` — the
common cases — map directly. `ctx.ui.custom()` (full TUI widgets) and
`before_provider_request` are the honest gaps; the bridge returns
`"unsupported_api"` for any call it can't translate and `pir` surfaces that.

**Step 3 — pre-install ABI analyzer (the safety gate).** Before a bridge ever
runs an extension's code, `pir` statically scans the `.ts` against a
machine-readable allowlist the current `pir` build advertises (`pir --abi`
lists supported events / `ctx` members). If the extension references an
**unsupported** API, `pir` does *not* install silently — it offers:

> `ext "brave-search" uses ctx.ui.custom() and before_provider_request, which this pir build does not support.`
> `[f] ask an agent to port it  [s] skip those APIs (may break)  [a] abort`

- `f` → `pir` hands the extension + the ABI spec to a sub-agent (reuse the
  existing agent loop in a throwaway session) to rewrite the unsupported calls
  into supported equivalents or stub them with a warning; the patched copy is
  written locally and re-scanned until clean (bounded attempts, then `[a]`).
- `s` → install anyway, tagged "partial"; unsupported calls become no-ops and
  `pir` logs them at runtime.
- `a` → abort install.

This is what makes a translation layer *safe*: gaps are resolved at install
time by an agent against a machine-readable contract, never discovered mid-task.

**Honest gaps:** `ctx.ui.custom()` (TUI widgets, needs the `tui` feature + a
widget protocol), `before_provider_request`/`before_provider_headers`
(provider interception), and true hot-reload of TS — `/reload` can kill+respawn
bridge children cheaply (it's IPC, not a build), which is actually *better* than
`pir`'s static-link model for data extensions.

Implementation order: #1 ABI extensions first (also unblocks #5 thinking? no — but
unblocks #17 trust + the bridge), then the `pi-extensions` backend, then the
analyzer. See also #13 (hot-reload) and #17 (trust).

---

## Out of scope (intentional, matches `pi`)
`pi` itself omits sub-agents and plan mode by design. `pir` also does not aim to
reimplement `pi`'s full `packages/agent` event/streaming harness, declarative
merging, or the Bun executable builder. These are explicitly out of scope for a
"featherweight" reimplementation; this list covers user-visible capability gaps
only.

---

## Suggested order of work
1. **Extend the Rust ABI + `pi-extensions` bridge (#21)** — unlocks `pi` extensions and is the substrate for #6/#7/#17. Biggest leverage.
2. Thinking levels (#1) — biggest capability win, tiny diff.
3. Google/Gemini provider (#2) — unlocks a major model family.
4. `--print`/`-p` + JSON mode (#3) — unblocks automation & the items below.
5. Auto-compaction (#4) — makes long tasks reliable.
6. `grep`/`find`/`ls` built-ins (#5) — cheap, safer parity.
7. Skills (#6) + prompt templates (#7) — main extensibility users expect (now reachable via the bridge).
8. Project trust (#17) — must land before the bridge loads project-local extensions.
9. Themes (#8), `/tree`+`/fork` (#9), `@`/editor (#10), images (#11) — UX parity.
10. Steering/follow-up (#12), parallel tools (#20), export/share (#16), telemetry+catalog (#18), usage footer (#19).
11. Defer packages (#14), RPC/SDK (#15), true hot-reload (#13) — conflict with the static-link design or are rarely needed.
