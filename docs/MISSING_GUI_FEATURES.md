# Missing GUI features (`--gui` vs `--tui`)

What the GTK GUI (`src/gui.rs`) does **not** have, compared with the ratatui TUI
(`src/tui.rs`) and the streaming REPL (`src/main.rs`).

Method: both frontends were read end to end and their user-facing surface
inventory was compared — slash commands, key bindings, panes, widgets, display
features and status indicators. Every claim below was re-verified against the
source before being written down; file:line references are current as of
`docs/MISSING_GUI_FEATURES.md` being added.

**Status:** the Severity 1 items and the cheap Severity 2/3/5 items are now
**fixed** — see the *Implemented* section at the end. Severity 3.2 (colour on
Linux) is the one item deliberately **not** done, because it requires changes to
the vendored `rustxWidgets` toolkit rather than to this repo's frontend.

One framing note, because it changes how the list should be read: **the GUI is
not "a window with a session button".** It is a working terminal-parity port —
a tabbed GTK chat window with a conversation log, single-line prompt entry,
status line, ~21 slash commands, tab completion, history, background jobs and
log-driven streaming. What was missing is not "functionality" in that sense. It
is the set of things that make a *graphical* frontend usable as a graphical
frontend: you could not scroll the output, you could not answer a question the
model asked you, and there were no affordances beyond a text box.

---

## Severity 1 — broken or structurally impossible

### 1.1 The conversation pane cannot be scrolled

- **TUI:** scrollback via `state.scroll` on the conversation `Paragraph`
  (`src/tui.rs:644`), with a 4000-line cap (`src/tui.rs:237-240`).
- **GUI:** `app.create_textview()` is placed **directly into the VBox**
  (`src/gui.rs:621`). A `GtkTextView` needs a viewport to scroll, and the
  toolkit's `create_scrolled_window()` (`rustxWidgets/…/backends/gtk.rs:147`,
  `…/core.rs:348`) is **never called anywhere in `gui.rs`**. The TextView also
  has no size request.
- **Impact:** the user cannot scroll up to read earlier output. Once a turn
  fills the window, everything before it is unreachable.
- **Note:** `src/gui.rs:3` advertises "a scrollable conversation TextView". That
  is currently false, and it is the one thing in the file's own header comment
  that the code does not do.
- **Fix:** wrap the TextView in a `ScrolledWindow`. The toolkit call already
  exists; this is a wiring change, not new toolkit work.

### 1.2 The agent can never ask the operator anything

- **TUI/REPL:** `install_ask_renderer()` (`src/main.rs:160`) installs the
  renderer that `askuser::ask` needs, so `ask_user_question` and plan approval
  are answerable.
- **GUI:** the `--gui` block at `src/main.rs:1398` **returns before**
  `install_ask_renderer()` is reached at `src/main.rs:1432`. So in `--gui` mode
  no renderer is ever installed, `askuser::interactive()` is false, and every
  request from the model comes back as *"no interactive user"*.
- **Impact:** plan-mode approval and the `ask_user_question` tool are both
  unusable in the GUI. This is not a missing widget — it is a startup-ordering
  bug, and it cannot be fixed from inside `gui.rs`.
- **Fix:** install the ask renderer before the GUI block returns, or have the
  GUI install its own renderer. Needs a decision: the existing renderer is
  terminal-oriented (`term::prompt_key`), so a real fix likely means a GUI
  dialog implementing the same trait.

### 1.3 `/goal` and `/continue` freeze the GUI window

- **TUI:** drives the goal on the worker; the UI stays live.
- **GUI:** both run synchronously on the main thread (`src/gui.rs:1620`,
  `:1634`), as the comment at `:1618` openly acknowledges.
- **Impact:** the window is unresponsive for the entire goal run — no
  redraw, no scroll, and `/cancel` cannot be typed.

---

## Severity 2 — GUI has no graphical affordances

### 2.1 No cancel keybinding

- **TUI:** `Esc` and `Ctrl-C` cancel the running turn; `Ctrl-D` quits cleanly
  (`src/tui.rs:919-945`, `976-986`, `931-937`).
- **GUI:** `grep -c "GDK_KEY_Escape\|GDK_KEY_c"` → **0 matches**. Cancel is only
  reachable by typing `/cancel`. The only key bindings that exist are `Ctrl+Q`,
  `Tab`, and `Arrow Up`/`Down` (`src/gui.rs:675-698`).
- **Impact:** a user who does not know `/cancel` has no way to stop a running
  turn except typing it.

### 2.2 No buttons, menus, dialogs, or file tree

Zero. `grep` for `button|dialog|modal|menu|picker|on_click|connect_clicked` in
`gui.rs` returns no functional hits. Every action is a typed line in one Entry.
That is defensible for a terminal port, but it means the GUI has no way to
present a *choice*.

### 2.3 No interactive pickers

- **Model selection:** `/models` prints a list, `/model <sel>` takes a typed
  string, Tab completes it (`src/gui.rs:1573-1597`). No picker widget.
- **Approval / question prompts:** see 1.2 — impossible today.

### 2.4 `Tab` is hijacked globally

`src/gui.rs:679-689` returns `true` from the key handler for `Tab`
unconditionally, so GTK's default Entry behaviour is dead, and Tab-completion
cycles nothing (it completes once, and on a second press re-completes the
already-completed text).

### 2.5 `focus-out` unconditionally re-grabs focus

`src/gui.rs:661-664` re-focuses the Entry on every `focus-out`, so the user
cannot click into another widget, and a click on the native tab bar fights it.

---

## Severity 3 — display parity gaps

### 3.1 No markdown rendering

- **TUI:** `crate::md::render(t, false)` on assistant text (`src/tui.rs:1684`).
- **GUI:** raw text appended (`src/gui.rs:1298-1321`).

### 3.2 No syntax highlighting, and the colour palette is dead on Linux

`ConvKind::style` and `line_markup` (`src/gui.rs:160`, `:185`) build Pango
spans, but the Linux `sync_textview` (`src/gui.rs:1678`) is documented as
monochrome — *"the conversation pane is monochrome. Colour lives in the
per-line prefix glyph"* (`:1686-1688`). Only the `#[cfg(windows)]` version
(`:1700`) calls `set_markup`. So on the primary target platform the entire
`ConvKind` palette is decorative, and `escape_markup` protects nothing.

**This one needs toolkit work, not a GUI change.** The GTK `TextView` wrapper
exposes only `set_text` / `append_text` (`vendors/rustxWidgets/…/
backends_gtk_adapter_impl.rs:648-661`) — no `set_markup`, and no
`gtk_text_buffer_create_tag` / `apply_tag` in the dynamic loader at all
(`vendors/rustxWidgets/gtk_dynamic_loader/src/wrappers.rs`). So there is
currently *no* way to colour GTK text through the toolkit. Closing this means
adding text-tag support to `rustxWidgets` first; only then does the existing
`ConvKind` palette become reachable on Linux.

**Not done as part of this pass** — deliberately, because it means changing the
vendored toolkit rather than this repo's frontend, and a half-wired colour path
would be worse than an honest monochrome one.

### 3.3 Scrollback desync at the second cap

`drain_session_log` trims at 200 000 lines with `state.conv.split_off(keep)`
(`src/gui.rs:1337-1340`) **without** decrementing `state.rendered` or setting
`needs_rebuild`, unlike `push()` (`src/gui.rs:376-381`). A trim at that cap can
leave the write cursor past the end of the model.

---

## Severity 4 — commands the TUI/REPL has and the GUI does not

The GUI implements ~21 commands. These are present in `src/tui.rs` or
`src/main.rs` and absent from `handle_command` (`src/gui.rs:1443`):

| Command | Where it exists | Notes for the GUI |
|---|---|---|
| `/resume` | `src/main.rs:3386` | Session picker; GUI has `/sessions` for tabs but no resume-across-restart |
| `/menu` | `src/main.rs:2687` | Interactive alt-screen menu — needs a GUI dialog equivalent |
| `/rewind` | `src/main.rs:4248` | Checkpoint rewind; pure logic, no UI needed beyond a confirm |
| `/checkpoints` | `src/main.rs:4239` | Same |
| `/plan` | `src/main.rs:3961` | Reads the plan document |
| `/quarantine` | `src/main.rs:3469` | Reviews staged system-tree writes — **security-relevant**, reviewable only from the terminal today |
| `/limits`, `/resources` | `src/main.rs:3872` | cgroup/sandbox limits |
| `/mcp` | `src/main.rs:4031` | MCP server status |
| `/skills` | `src/main.rs:4197` | Skills catalog |
| `/taste` | `src/main.rs:4104` | Taste preferences |
| `/login`, `/logout` | `src/main.rs:3601`, `3741` | Credential management (`/login P KEY [MODEL]` works in one line) |
| `/l` | `src/main.rs:3083` | Set the persisted default model |
| `/project`, `/create` | `src/main.rs:3408`, `3464` | Sandbox-user provisioning |
| `/autoclean` | `src/main.rs:3677` | Retention cleanup |
| `/scratchpad`, `/scratch` | `src/main.rs:3898` | Scratchpad info |
| `/ext` | `src/main.rs:4362` | Extension listing |
| `/unfinished` | `src/main.rs:3365` | Unfinished-session list |
| `/rebuild` | `src/main.rs:3773` | Build the binary in place |
| `/markup_demo`, `/md_demo` | `src/main.rs` | Debug/demo helpers |
| `/fg` | `src/main.rs:3330` | Foreground a background job |

**Also:** the GUI's own parity guard
(`gui_implements_every_tui_command_the_gui_advertises`, `src/gui.rs:2137`)
checks only an 18-command subset, so it passes while all of the above are
missing. The comment at `:2139-2141` says the subset is "restricted to what a
graphical front-end can meaningfully offer" — which is a reasonable scope, but
it means the test's name overstates what it enforces.

### 4.1 TUI-only commands (for completeness)

Present in `src/tui.rs`, absent in the GUI: `/rebuild` and `/reexec` are
stubs in the TUI itself (`src/tui.rs:1964-1969`), and `/jobs` is a stub there
too (`:1903-1905`) though the GUI's `/jobs` is real. `/reload` is advertised in
the TUI's `/help` but has no match arm (`src/tui.rs:1977` vs `1754-1972`).

---

## Severity 5 — dead and misleading code

Worth removing or fixing, because each of these actively misleads a reader:

1. **Unreachable `/sessions` arm** (`src/gui.rs:1599-1601`) prints *"use `pir -r`
   from a terminal to resume sessions"* — unreachable, because `/sessions` is
   intercepted at `:738`, and **factually wrong**, since the GUI does have
   session tabs.
2. **`src/gui.rs:3` header claims a "scrollable" TextView.** See 1.1.
3. **Unused constants** `ON_GTK` (`:54-56`) and `GTK_WRAP_WORD_CHAR` (`:60`) —
   the wrap mode is hardcoded to literal `2` at `:1354`.
4. **Live colouring is Windows-only** while `ConvKind` implies a cross-platform
   palette. See 3.2.
5. **Synchronous `Agent::new` on the UI thread** in `/sessions new`
   (`src/gui.rs:899`) and `spawn_background` (`:1205`) — provider/network
   construction blocks redraw.

---

## Implemented

Everything below was fixed in `src/gui.rs` (plus the gap document above) and
verified with `cargo test --features gui --bin pir -- --test-threads=8`
(757 passed, 0 failed).

| Was | Now |
|---|---|
| **1.1** conversation pane not scrollable | TextView wrapped in `ScrolledWindow` (`set_policy(NEVER, AUTOMATIC)`, min height 200). The toolkit call already existed. |
| **1.2** agent could never ask the operator anything | A GUI `question_dialog` (label + one button per option, or an entry for free-text, plus Submit/Cancel), installed as the `ask_user_question` renderer at the top of `run()` — before `main`'s terminal renderer is reached. `askuser::interactive()` is now true in `--gui`. |
| **1.3** `/goal` and `/continue` froze the window | New `spawn_goal` moves the agent to a worker (same shape as `spawn_turn`) and both commands return immediately; output arrives through the session log and the existing 150ms drain. |
| **2.1** no cancel key | `Esc` and `Ctrl-C` cancel, `Ctrl-D` quits. All three route through the same `cancel_running_locked` as `/cancel`, so key and command cannot drift apart. Greeting and `/help` updated. |
| **3.3** 200k-line trim desynced the write cursor | The trim now shifts `rendered` and sets `needs_rebuild`, matching what `push()` already did for the 4000-line cap. |
| **4** `/checkpoints`, `/rewind`, `/quarantine`, `/limits`, `/skills`, `/scratchpad` missing | Added as `handle_command` arms, matching the streaming REPL's output. `/quarantine apply all` is security-relevant (the operator's explicit go-ahead for staged writes) and was terminal-only. |
| **4** completion list incomplete | `/sh` added alongside `/shell`; all new commands added; aliases (`resources`, `skill`, `scratch`, `ckpt`) too. |
| **5.1** dead, wrong `/sessions` arm | Removed. `/sessions` is dispatched by `handle_session_command` before `handle_command` is reached, so the arm was unreachable — and its message ("use `pir -r` from a terminal") was wrong for a GUI that has tabs. |
| **5.2** header comment claimed a "scrollable" TextView | Now true (see 1.1). |
| **5.3** unused `ON_GTK` | Removed; the `cfg(feature = "gtk")` warning it documented moved onto the type aliases it actually explains, and `GTK_WRAP_WORD_CHAR` is now used instead of a bare `2`. |

Tests added, so these cannot silently regress:

- `goal_and_continue_are_spawned_not_run_inline` — source-level guard that
  neither command may call `continue_goal()` inline. A blocking call cannot be
  observed without a display, but it *can* be seen in the code, and that is what
  is most likely to be "tidied" back into a blocking call.
- `gui_implements_every_tui_command_the_gui_advertises` — extended: every
  command must now also be in the Tab-completion list, which is what would have
  caught `/shell`-completable-but-`/sh`-not.

Not done:

- **3.2** Linux colour rendering — blocked on toolkit support (see above).
- **2.2** buttons/menus/dialogs/file-tree, **2.3** interactive pickers beyond
  the question dialog, **2.4** `Tab` hijack, **2.5** `focus-out` focus trap —
  these are redesigns rather than gap-fills, and worth deciding separately.
- **5.4** synchronous `Agent::new` on the UI thread in `/sessions new` and
  `spawn_background` — real, but a bigger change than this pass.

---

## Suggested order of any remaining work

1. **3.2 colour on Linux** — add text-tag support to `rustxWidgets`, then
   `sync_textview` can use the `ConvKind` palette it already defines.
2. **2.4 / 2.5** — stop hijacking `Tab` unconditionally, and stop
   re-grabbing focus on every `focus-out`.
3. **5.4** — move `Agent::new` off the GTK main thread.
4. **2.2 / 2.3** — a deliberate design decision, not a gap-fill.

Deliberately **not** in scope above: adding
buttons/menus/file-tree to the GUI would be a redesign, not a gap-fill. The
list above assumes the GUI stays a terminal-parity port and asks only that it
stop being broken in the ways that matter and gain the affordances that are
cheap.
